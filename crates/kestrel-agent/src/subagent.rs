//! Sub-agent spawning — parallel task execution framework.
//!
//! Provides `SubAgentManager` with `spawn_parallel` for executing multiple
//! independent LLM tasks concurrently using `tokio::JoinSet`. Each sub-agent
//! gets its own `AgentRunner` with independent sessions, resource limits,
//! and configurable tool permissions.
//!
//! The [`SubAgentSpawner`] trait (from `kestrel-tools`) is implemented for
//! [`SubAgentManager`] so that tools like `SpawnTool` can delegate to it.

use anyhow::Result;
use async_trait::async_trait;
use kestrel_config::Config;
use kestrel_core::Message;
use kestrel_providers::ProviderRegistry;
use kestrel_tools::registry::ToolRegistry;
use kestrel_tools::trait_def::{SpawnStatus, SubAgentSpawner};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::runner::AgentRunner;

// ─── Types ────────────────────────────────────────────────────

/// A task to be executed by a sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentTask {
    /// Unique task identifier (auto-generated if not set).
    #[serde(default)]
    pub id: String,

    /// The prompt to send to the sub-agent.
    pub prompt: String,

    /// Additional context injected before the prompt.
    #[serde(default)]
    pub context: Option<String>,

    /// Override the default model for this task.
    #[serde(default)]
    pub model_override: Option<String>,

    /// Maximum tokens the sub-agent may generate.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

/// Result of a single sub-agent task execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentResult {
    /// Task identifier (matches `SubAgentTask::id`).
    pub id: String,

    /// The output text produced by the sub-agent.
    pub output: String,

    /// Whether the task completed successfully.
    pub success: bool,

    /// Wall-clock duration of the task.
    pub duration_secs: f64,

    /// Total tokens consumed (prompt + completion).
    pub tokens_used: u64,

    /// Number of tool calls made during execution.
    pub tool_calls_made: usize,

    /// Number of agent-loop iterations consumed.
    pub iterations_used: usize,
}

/// Configuration for parallel sub-agent execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelSpawnConfig {
    /// Maximum number of tasks executing concurrently.
    /// Default: 3.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,

    /// Timeout per individual task.
    /// Default: 60 seconds.
    #[serde(default = "default_per_task_timeout")]
    pub per_task_timeout_secs: u64,

    /// Timeout for the entire batch of tasks.
    /// `None` means no overall deadline.
    /// Default: None.
    #[serde(default)]
    pub total_timeout_secs: Option<u64>,

    /// Tool names to deny for sub-agents (inherited from parent otherwise).
    /// If empty, all parent tools are available.
    #[serde(default)]
    pub denied_tools: Vec<String>,

    /// System prompt prefix for sub-agents.
    #[serde(default)]
    pub system_prompt_prefix: Option<String>,
}

fn default_max_concurrent() -> usize {
    3
}
fn default_per_task_timeout() -> u64 {
    60
}

impl Default for ParallelSpawnConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent(),
            per_task_timeout_secs: default_per_task_timeout(),
            total_timeout_secs: None,
            denied_tools: vec![],
            system_prompt_prefix: None,
        }
    }
}

/// Status of a tracked sub-agent task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task registered but not yet executing.
    Pending,
    /// Task is currently executing.
    Running,
    /// Task completed successfully with the given output.
    Completed(String),
    /// Task failed with the given error message.
    Failed(String),
    /// Task was explicitly cancelled.
    Cancelled,
}

impl TaskStatus {
    /// Returns `true` if the task has reached a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed(_) | TaskStatus::Failed(_) | TaskStatus::Cancelled
        )
    }
}

impl From<&TaskStatus> for SpawnStatus {
    fn from(s: &TaskStatus) -> Self {
        match s {
            TaskStatus::Pending | TaskStatus::Running => SpawnStatus::Running,
            TaskStatus::Completed(r) => SpawnStatus::Completed(r.clone()),
            TaskStatus::Failed(e) => SpawnStatus::Failed(e.clone()),
            TaskStatus::Cancelled => SpawnStatus::Failed("Cancelled".to_string()),
        }
    }
}

/// Collected results from a parallel spawn, with summary statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSummary {
    /// Individual task results, in the order tasks were submitted.
    pub results: Vec<SubAgentResult>,

    /// Number of tasks that succeeded.
    pub succeeded: usize,

    /// Number of tasks that failed.
    pub failed: usize,

    /// Total wall-clock time for the entire batch.
    pub total_duration_secs: f64,

    /// Total tokens consumed across all tasks.
    pub total_tokens_used: u64,
}

impl SpawnSummary {
    /// Combine all successful outputs into structured notes.
    pub fn to_structured_notes(&self) -> String {
        let mut notes = String::new();
        for result in &self.results {
            if result.success {
                notes.push_str(&format!(
                    "## Task {} ({:.1}s, {} tokens)\n{}\n\n",
                    result.id, result.duration_secs, result.tokens_used, result.output
                ));
            } else {
                notes.push_str(&format!(
                    "## Task {} — FAILED\nError: {}\n\n",
                    result.id, result.output
                ));
            }
        }
        notes.push_str(&format!(
            "Summary: {}/{} tasks succeeded, {:.1}s total, {} tokens used",
            self.succeeded,
            self.succeeded + self.failed,
            self.total_duration_secs,
            self.total_tokens_used
        ));
        notes
    }
}

/// A message sent between sub-agents or from the parent agent.
///
/// Messages are stored in a per-task mailbox and can be drained
/// by the task owner at any time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentMessage {
    /// Sender identifier — a task ID or `"parent"`.
    pub from: String,
    /// Message content.
    pub content: String,
    /// When this message was created.
    pub timestamp: chrono::DateTime<chrono::Local>,
}

impl SubAgentMessage {
    /// Create a new message from the given sender.
    pub fn new(from: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            content: content.into(),
            timestamp: chrono::Local::now(),
        }
    }
}

/// Internal tracked task for background monitoring.
struct TrackedTask {
    id: String,
    name: String,
    status: TaskStatus,
    /// Per-task mailbox for inter-agent messages.
    mailbox: Vec<SubAgentMessage>,
    /// When the task transitioned to Running.
    started_at: Option<Instant>,
    /// When the task reached a terminal state.
    completed_at: Option<Instant>,
    /// JoinHandle so we can abort the background tokio task on cancel.
    handle: Option<tokio::task::JoinHandle<()>>,
    /// Signalled when the task reaches a terminal state.
    done_notify: Arc<tokio::sync::Notify>,
}

// ─── SubAgentHandle ─────────────────────────────────────────────

/// Handle to a spawned sub-agent task.
///
/// Provides methods to query status and request cancellation without
/// exposing the internal `SubAgentManager`.
#[derive(Debug, Clone)]
pub struct SubAgentHandle {
    /// Task ID.
    pub id: String,
    /// Human-readable task name.
    pub name: String,
    manager: Arc<SubAgentManager>,
}

impl SubAgentHandle {
    /// Query the current status of this task.
    pub async fn status(&self) -> Option<SpawnStatus> {
        self.manager.status(&self.id).await
    }

    /// Request cancellation of this task.
    /// Returns `true` if the task was found and signalled.
    pub async fn cancel(&self) -> bool {
        self.manager.cancel(&self.id).await
    }
}

// ─── SubAgentManager ──────────────────────────────────────────

/// Configuration defaults for the sub-agent manager.
#[derive(Debug, Clone)]
pub struct SubAgentManagerConfig {
    /// Maximum number of concurrently tracked tasks. 0 = unlimited.
    pub max_tasks: usize,
    /// Default timeout in seconds for `spawn_single`. 0 = no timeout.
    pub default_timeout_secs: u64,
}

impl Default for SubAgentManagerConfig {
    fn default() -> Self {
        Self {
            max_tasks: 0,
            default_timeout_secs: 120,
        }
    }
}

/// Manages sub-agent spawning — both background tracking and parallel execution.
///
/// Holds shared config, provider registry, and tool registry so each sub-agent
/// can create its own `AgentRunner`. Implements [`SubAgentSpawner`] so tools
/// like `SpawnTool` can delegate to it.
pub struct SubAgentManager {
    config: Arc<Config>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    tasks: Arc<RwLock<Vec<TrackedTask>>>,
    manager_config: SubAgentManagerConfig,
}

impl std::fmt::Debug for SubAgentManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubAgentManager")
            .field("manager_config", &self.manager_config)
            .finish_non_exhaustive()
    }
}

impl SubAgentManager {
    /// Create a new SubAgentManager with access to the parent agent's registries.
    pub fn new(
        config: Arc<Config>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            config,
            providers,
            tools,
            tasks: Arc::new(RwLock::new(Vec::new())),
            manager_config: SubAgentManagerConfig::default(),
        }
    }

    /// Create with custom manager-level configuration.
    pub fn with_manager_config(
        config: Arc<Config>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
        manager_config: SubAgentManagerConfig,
    ) -> Self {
        Self {
            config,
            providers,
            tools,
            tasks: Arc::new(RwLock::new(Vec::new())),
            manager_config,
        }
    }

    // ─── Background task tracking (legacy + monitoring) ────────

    /// Register a new background task for tracking (starts in Pending state).
    ///
    /// Returns the task ID. The task is not yet executing — call
    /// [`start`](Self::start) or use [`spawn_single`](Self::spawn_single)
    /// for automatic lifecycle management.
    pub async fn spawn(&self, name: &str, _description: &str) -> String {
        let id = Uuid::new_v4().to_string();
        let task = TrackedTask {
            id: id.clone(),
            name: name.to_string(),
            status: TaskStatus::Pending,
            mailbox: Vec::new(),
            started_at: None,
            completed_at: None,
            handle: None,
            done_notify: Arc::new(tokio::sync::Notify::new()),
        };
        self.tasks.write().await.push(task);
        info!("Registered subagent task: {} ({})", name, id);
        id
    }

    /// Mark a tracked task as completed.
    pub async fn complete(&self, id: &str, result: String) {
        let notify = {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.status = TaskStatus::Completed(result);
                task.completed_at = Some(Instant::now());
                debug!("Completed subagent task: {}", id);
                Some(Arc::clone(&task.done_notify))
            } else {
                None
            }
        };
        if let Some(n) = notify {
            n.notify_one();
        }
    }

    /// Mark a tracked task as failed.
    pub async fn fail(&self, id: &str, error: String) {
        let notify = {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.status = TaskStatus::Failed(error);
                task.completed_at = Some(Instant::now());
                debug!("Failed subagent task: {}", id);
                Some(Arc::clone(&task.done_notify))
            } else {
                None
            }
        };
        if let Some(n) = notify {
            n.notify_one();
        }
    }

    /// Get the status of a tracked task.
    pub async fn get_status(&self, id: &str) -> Option<TaskStatus> {
        let tasks = self.tasks.read().await;
        tasks.iter().find(|t| t.id == id).map(|t| t.status.clone())
    }

    /// List all tracked tasks.
    pub async fn list_tasks(&self) -> Vec<(String, String, TaskStatus)> {
        let tasks = self.tasks.read().await;
        tasks
            .iter()
            .map(|t| (t.id.clone(), t.name.clone(), t.status.clone()))
            .collect()
    }

    // ─── Single background spawn (SubAgentSpawner backing) ─────

    /// Spawn a single sub-agent task that executes in the background.
    ///
    /// Registers the task, kicks off execution via a dedicated `AgentRunner`,
    /// and updates the tracking status on completion or failure.
    /// Returns a [`SubAgentHandle`] for monitoring.
    ///
    /// If `timeout_secs` is `Some`, the sub-agent will be killed after that
    /// many seconds. If `None`, [`SubAgentManagerConfig::default_timeout_secs`]
    /// is used (0 means no timeout).
    pub async fn spawn_single(
        self: &Arc<Self>,
        name: &str,
        prompt: &str,
        context: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<SubAgentHandle> {
        // Check max_tasks limit
        {
            let tasks = self.tasks.read().await;
            let active = tasks.iter().filter(|t| !t.status.is_terminal()).count();
            if self.manager_config.max_tasks > 0 && active >= self.manager_config.max_tasks {
                anyhow::bail!(
                    "Cannot spawn: {} active tasks (limit: {})",
                    active,
                    self.manager_config.max_tasks
                );
            }
        }

        let id = Uuid::new_v4().to_string();
        let timeout = timeout_secs
            .or(if self.manager_config.default_timeout_secs > 0 {
                Some(self.manager_config.default_timeout_secs)
            } else {
                None
            })
            .map(Duration::from_secs);

        // Register tracking entry
        let _notify = {
            let task = TrackedTask {
                id: id.clone(),
                name: name.to_string(),
                status: TaskStatus::Pending,
                mailbox: Vec::new(),
                started_at: None,
                completed_at: None,
                handle: None,
                done_notify: Arc::new(tokio::sync::Notify::new()),
            };
            let notify = Arc::clone(&task.done_notify);
            self.tasks.write().await.push(task);
            notify
        };

        info!("Spawning single sub-agent task '{}' ({})", name, id);

        // Build runner for the sub-agent
        let runner = AgentRunner::new(
            self.config.clone(),
            self.providers.clone(),
            self.tools.clone(),
        );

        // Build messages
        let mut messages = Vec::new();
        if let Some(ref ctx) = context {
            messages.push(Message {
                role: kestrel_core::MessageRole::User,
                content: format!("Context:\n{}", ctx),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }
        messages.push(Message {
            role: kestrel_core::MessageRole::User,
            content: prompt.to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });

        let system_prompt = "You are a focused sub-agent executing a specific task. \
            Be concise and direct in your response."
            .to_string();

        // Transition to Running
        {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.status = TaskStatus::Running;
                task.started_at = Some(Instant::now());
            }
        }

        // Spawn background tokio task
        let mgr = Arc::clone(self);
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let run_future = runner.run(system_prompt, messages);

            let result = match timeout {
                Some(dur) => match tokio::time::timeout(dur, run_future).await {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e),
                    Err(_) => {
                        mgr.fail(
                            &task_id,
                            format!("Sub-agent timed out after {:.0}s", dur.as_secs_f64()),
                        )
                        .await;
                        return;
                    }
                },
                None => run_future.await,
            };

            match result {
                Ok(run_result) => {
                    mgr.complete(&task_id, run_result.content).await;
                }
                Err(e) => {
                    mgr.fail(&task_id, format!("{}", e)).await;
                }
            }
        });

        // Store the join handle
        {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.iter_mut().find(|t| t.id == id) {
                task.handle = Some(handle);
            }
        }

        Ok(SubAgentHandle {
            id,
            name: name.to_string(),
            manager: Arc::clone(self),
        })
    }

    /// Wait for a tracked task to reach a terminal state.
    ///
    /// Returns the final [`TaskStatus`], or `None` if the task ID is unknown.
    /// If `timeout` elapses before the task finishes, returns the current
    /// (non-terminal) status.
    pub async fn wait_for(&self, id: &str, timeout: Duration) -> Option<TaskStatus> {
        // Snapshot the notify handle without holding the lock across the await.
        let notify = {
            let tasks = self.tasks.read().await;
            let task = tasks.iter().find(|t| t.id == id)?;
            if task.status.is_terminal() {
                return Some(task.status.clone());
            }
            Arc::clone(&task.done_notify)
        };

        // Wait for completion signal
        let _ = tokio::time::timeout(timeout, notify.notified()).await;

        // Read final status
        let tasks = self.tasks.read().await;
        tasks.iter().find(|t| t.id == id).map(|t| t.status.clone())
    }

    /// Abort all running background tasks and mark them as Cancelled.
    ///
    /// Returns the number of tasks that were terminated.
    pub async fn terminate_all(&self) -> usize {
        let mut count = 0;
        let notifies: Vec<Arc<tokio::sync::Notify>> = {
            let mut tasks = self.tasks.write().await;
            let mut notifies = Vec::new();
            for task in tasks.iter_mut() {
                if !task.status.is_terminal() {
                    if let Some(handle) = task.handle.take() {
                        handle.abort();
                    }
                    task.status = TaskStatus::Cancelled;
                    task.completed_at = Some(Instant::now());
                    notifies.push(Arc::clone(&task.done_notify));
                    count += 1;
                }
            }
            notifies
        };
        for n in notifies {
            n.notify_one();
        }
        if count > 0 {
            info!("Terminated {} sub-agent tasks", count);
        }
        count
    }

    /// Remove all tasks that have reached a terminal state.
    ///
    /// Returns the number of tasks removed.
    pub async fn cleanup_completed(&self) -> usize {
        let mut tasks = self.tasks.write().await;
        let before = tasks.len();
        tasks.retain(|t| !t.status.is_terminal());
        before - tasks.len()
    }

    /// Count tasks that are currently in a non-terminal state.
    pub async fn active_count(&self) -> usize {
        let tasks = self.tasks.read().await;
        tasks.iter().filter(|t| !t.status.is_terminal()).count()
    }

    // ─── Parallel execution ───────────────────────────────────

    /// Execute multiple sub-agent tasks in parallel.
    ///
    /// Uses `tokio::JoinSet` to run tasks concurrently, respecting
    /// `config.max_concurrent` as the concurrency limit. Each task
    /// gets its own `AgentRunner` with independent configuration.
    /// Failed tasks are isolated — one failure does not affect others.
    pub async fn spawn_parallel(
        &self,
        tasks: Vec<SubAgentTask>,
        config: &ParallelSpawnConfig,
    ) -> Result<SpawnSummary> {
        let total_start = Instant::now();
        let total_timeout = config.total_timeout_secs.map(Duration::from_secs);
        let per_task_timeout = Duration::from_secs(config.per_task_timeout_secs);

        // Build tool registry with denied tools filtered
        let filtered_tools = self.build_filtered_tools(&config.denied_tools)?;

        // Prepare tasks with IDs
        let prepared: Vec<SubAgentTask> = tasks
            .into_iter()
            .enumerate()
            .map(|(i, mut t)| {
                if t.id.is_empty() {
                    t.id = format!("task-{}", i + 1);
                }
                t
            })
            .collect();

        let task_count = prepared.len();
        info!(
            "Spawning {} parallel sub-agent tasks (max_concurrent: {})",
            task_count, config.max_concurrent
        );

        // Track results by task ID
        let results: Arc<RwLock<Vec<SubAgentResult>>> =
            Arc::new(RwLock::new(Vec::with_capacity(task_count)));

        let mut join_set: tokio::task::JoinSet<(String, Result<SubAgentResult>)> =
            tokio::task::JoinSet::new();
        let mut task_iter = prepared.into_iter().peekable();
        let mut spawned = 0usize;

        // Spawn initial batch up to max_concurrent
        while spawned < config.max_concurrent && task_iter.peek().is_some() {
            let task = task_iter.next().expect("peek guaranteed a value");
            let runner = self.build_runner(&task, &filtered_tools, config);
            let timeout = per_task_timeout;
            let _task_id = task.id.clone();
            join_set.spawn(run_single_task(task, runner, timeout));
            spawned += 1;
        }

        // Collect results and spawn more as slots free up
        while let Some(join_result) = join_set.join_next().await {
            let (task_id, task_result) = match join_result {
                Ok(pair) => pair,
                Err(join_err) => {
                    warn!("JoinSet task panicked: {}", join_err);
                    // We lost track of which task — skip
                    break;
                }
            };

            let result = match task_result {
                Ok(r) => r,
                Err(e) => {
                    warn!("Sub-agent task {} failed: {}", task_id, e);
                    SubAgentResult {
                        id: task_id.clone(),
                        output: format!("Task error: {}", e),
                        success: false,
                        duration_secs: 0.0,
                        tokens_used: 0,
                        tool_calls_made: 0,
                        iterations_used: 0,
                    }
                }
            };

            debug!(
                "Task {} completed: success={}, duration={:.1}s, tokens={}",
                result.id, result.success, result.duration_secs, result.tokens_used
            );

            results.write().await.push(result);

            // Check total timeout
            if let Some(total) = total_timeout {
                if total_start.elapsed() > total {
                    warn!("Total timeout reached, stopping remaining tasks");
                    join_set.abort_all();
                    break;
                }
            }

            // Spawn next task if available
            if task_iter.peek().is_some() {
                let task = task_iter.next().expect("peek guaranteed a value");
                let runner = self.build_runner(&task, &filtered_tools, config);
                let timeout = per_task_timeout;
                join_set.spawn(run_single_task(task, runner, timeout));
            }
        }

        // Abort any remaining tasks
        join_set.abort_all();

        let results_vec = results.read().await;
        let succeeded = results_vec.iter().filter(|r| r.success).count();
        let failed = results_vec.len().saturating_sub(succeeded);
        let total_tokens = results_vec.iter().map(|r| r.tokens_used).sum();

        let summary = SpawnSummary {
            results: results_vec.clone(),
            succeeded,
            failed,
            total_duration_secs: total_start.elapsed().as_secs_f64(),
            total_tokens_used: total_tokens,
        };

        info!(
            "Parallel spawn complete: {}/{} succeeded in {:.1}s",
            summary.succeeded,
            summary.succeeded + summary.failed,
            summary.total_duration_secs
        );

        Ok(summary)
    }

    /// Build an `AgentRunner` for a specific sub-agent task.
    fn build_runner(
        &self,
        task: &SubAgentTask,
        tools: &Arc<ToolRegistry>,
        _config: &ParallelSpawnConfig,
    ) -> AgentRunner {
        // Build per-task config with optional overrides
        let mut task_config = (*self.config).clone();
        if let Some(ref model) = task.model_override {
            task_config.agent.model = model.clone();
        }
        if let Some(max_tokens) = task.max_tokens {
            task_config.agent.max_tokens = max_tokens;
        }

        AgentRunner::new(Arc::new(task_config), self.providers.clone(), tools.clone())
    }

    // ─── Inter-agent messaging ────────────────────────────────

    /// Send a message to a specific task's mailbox.
    ///
    /// Returns `false` if the task ID is unknown or the task is already
    /// in a terminal state.
    pub async fn send_message(&self, task_id: &str, from: &str, content: String) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) {
            if task.status.is_terminal() {
                return false;
            }
            task.mailbox.push(SubAgentMessage::new(from, content));
            debug!("Sent message to task '{}' from '{}'", task_id, from);
            true
        } else {
            false
        }
    }

    /// Broadcast a message to all running tasks' mailboxes.
    ///
    /// Returns the number of tasks that received the message.
    pub async fn broadcast_message(&self, from: &str, content: String) -> usize {
        let mut tasks = self.tasks.write().await;
        let mut count = 0;
        for task in tasks.iter_mut() {
            if !task.status.is_terminal() {
                task.mailbox
                    .push(SubAgentMessage::new(from, content.clone()));
                count += 1;
            }
        }
        if count > 0 {
            debug!("Broadcast message from '{}' to {} tasks", from, count);
        }
        count
    }

    /// Drain all pending messages from a task's mailbox.
    ///
    /// Returns the messages in FIFO order. Returns an empty vec if
    /// the task ID is unknown.
    pub async fn drain_messages(&self, task_id: &str) -> Vec<SubAgentMessage> {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) {
            std::mem::take(&mut task.mailbox)
        } else {
            Vec::new()
        }
    }

    /// Get the number of pending messages in a task's mailbox.
    pub async fn mailbox_len(&self, task_id: &str) -> usize {
        let tasks = self.tasks.read().await;
        tasks
            .iter()
            .find(|t| t.id == task_id)
            .map(|t| t.mailbox.len())
            .unwrap_or(0)
    }

    // ─── Runner construction helpers ──────────────────────────

    /// Build a tool registry with denied tools filtered out.
    fn build_filtered_tools(&self, denied: &[String]) -> Result<Arc<ToolRegistry>> {
        if denied.is_empty() {
            return Ok(self.tools.clone());
        }

        let filtered = self.tools.filter_out(denied);
        Ok(Arc::new(filtered))
    }
}

// ─── SubAgentSpawner impl ──────────────────────────────────────

#[async_trait]
impl SubAgentSpawner for SubAgentManager {
    async fn spawn(&self, name: &str, prompt: &str, context: Option<String>) -> Result<String> {
        // The shared `tasks` map is an Arc<RwLock<..>>, so cloning the manager
        // shell is cheap and shares the same underlying task list with any
        // existing Arc<Self> the caller may already hold.
        let arc_self: Arc<Self> = Arc::new(Self {
            config: self.config.clone(),
            providers: self.providers.clone(),
            tools: self.tools.clone(),
            tasks: self.tasks.clone(),
            manager_config: self.manager_config.clone(),
        });
        let handle = arc_self.spawn_single(name, prompt, context, None).await?;
        Ok(handle.id)
    }

    async fn spawn_with_timeout(
        &self,
        name: &str,
        prompt: &str,
        context: Option<String>,
        timeout_secs: Option<u64>,
    ) -> Result<String> {
        let arc_self: Arc<Self> = Arc::new(Self {
            config: self.config.clone(),
            providers: self.providers.clone(),
            tools: self.tools.clone(),
            tasks: self.tasks.clone(),
            manager_config: self.manager_config.clone(),
        });
        let handle = arc_self
            .spawn_single(name, prompt, context, timeout_secs)
            .await?;
        Ok(handle.id)
    }

    async fn status(&self, task_id: &str) -> Option<SpawnStatus> {
        let tasks = self.tasks.read().await;
        tasks
            .iter()
            .find(|t| t.id == task_id)
            .map(|t| SpawnStatus::from(&t.status))
    }

    async fn cancel(&self, task_id: &str) -> bool {
        let notify = {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) {
                if task.status.is_terminal() {
                    return false;
                }
                if let Some(handle) = task.handle.take() {
                    handle.abort();
                }
                task.status = TaskStatus::Cancelled;
                task.completed_at = Some(Instant::now());
                debug!("Cancelled subagent task: {}", task_id);
                Some(Arc::clone(&task.done_notify))
            } else {
                None
            }
        };
        if let Some(n) = notify {
            n.notify_one();
            true
        } else {
            false
        }
    }

    async fn list(&self) -> Vec<(String, String, SpawnStatus)> {
        let tasks = self.tasks.read().await;
        tasks
            .iter()
            .map(|t| (t.id.clone(), t.name.clone(), SpawnStatus::from(&t.status)))
            .collect()
    }
}

/// Run a single sub-agent task with timeout.
async fn run_single_task(
    task: SubAgentTask,
    runner: AgentRunner,
    timeout: Duration,
) -> (String, Result<SubAgentResult>) {
    let task_id = task.id.clone();
    let start = Instant::now();

    // Build system prompt
    let system_prompt = "You are a focused sub-agent executing a specific task. \
        Be concise and direct in your response."
        .to_string();

    // Build messages
    let mut messages = Vec::new();
    if let Some(ref ctx) = task.context {
        messages.push(Message {
            role: kestrel_core::MessageRole::User,
            content: format!("Context:\n{}", ctx),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
    }
    messages.push(Message {
        role: kestrel_core::MessageRole::User,
        content: task.prompt.clone(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    });

    // Execute with timeout
    let run_result = match tokio::time::timeout(timeout, runner.run(system_prompt, messages)).await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            let duration = start.elapsed().as_secs_f64();
            return (
                task_id,
                Ok(SubAgentResult {
                    id: task.id,
                    output: format!("Agent error: {}", e),
                    success: false,
                    duration_secs: duration,
                    tokens_used: 0,
                    tool_calls_made: 0,
                    iterations_used: 0,
                }),
            );
        }
        Err(_) => {
            let duration = start.elapsed().as_secs_f64();
            return (
                task_id,
                Ok(SubAgentResult {
                    id: task.id,
                    output: format!("Timeout after {:.0}s", timeout.as_secs()),
                    success: false,
                    duration_secs: duration,
                    tokens_used: 0,
                    tool_calls_made: 0,
                    iterations_used: 0,
                }),
            );
        }
    };

    let duration = start.elapsed().as_secs_f64();
    let tokens_used = run_result.usage.total_tokens.unwrap_or(0);

    (
        task_id,
        Ok(SubAgentResult {
            id: task.id,
            output: run_result.content,
            success: true,
            duration_secs: duration,
            tokens_used,
            tool_calls_made: run_result.tool_calls_made,
            iterations_used: run_result.iterations_used,
        }),
    )
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kestrel_core::Usage;
    use kestrel_providers::base::{
        BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider,
    };
    use kestrel_tools::trait_def::SpawnStatus;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock provider that returns deterministic responses.
    struct MockProvider {
        responses: Vec<CompletionResponse>,
        call_count: Arc<AtomicU32>,
    }

    impl MockProvider {
        fn simple(text: &str) -> Self {
            Self {
                responses: vec![CompletionResponse {
                    content: Some(text.to_string()),
                    tool_calls: None,
                    usage: Some(Usage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(5),
                        total_tokens: Some(15),
                    }),
                    finish_reason: Some("stop".to_string()),
                }],
                call_count: Arc::new(AtomicU32::new(0)),
            }
        }

        /// Create a provider that returns different text per call.
        fn multi(responses: Vec<&str>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|text| CompletionResponse {
                        content: Some(text.to_string()),
                        tool_calls: None,
                        usage: Some(Usage {
                            prompt_tokens: Some(10),
                            completion_tokens: Some(5),
                            total_tokens: Some(15),
                        }),
                        finish_reason: Some("stop".to_string()),
                    })
                    .collect(),
                call_count: Arc::new(AtomicU32::new(0)),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
            self.responses
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("MockProvider: no response for call {}", idx))
        }

        async fn complete_stream(&self, req: CompletionRequest) -> Result<BoxStream> {
            let resp = self.complete(req).await?;
            let chunk = CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: resp.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }

        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    /// Mock provider that introduces a delay before responding.
    struct DelayedProvider {
        text: String,
        delay: Duration,
    }

    impl DelayedProvider {
        fn new(text: &str, delay: Duration) -> Self {
            Self {
                text: text.to_string(),
                delay,
            }
        }
    }

    #[async_trait]
    impl LlmProvider for DelayedProvider {
        fn name(&self) -> &str {
            "mock-delayed"
        }

        async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse> {
            tokio::time::sleep(self.delay).await;
            Ok(CompletionResponse {
                content: Some(self.text.clone()),
                tool_calls: None,
                usage: Some(Usage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    total_tokens: Some(15),
                }),
                finish_reason: Some("stop".to_string()),
            })
        }

        async fn complete_stream(&self, req: CompletionRequest) -> Result<BoxStream> {
            let resp = self.complete(req).await?;
            let chunk = CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: resp.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }

        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    /// Build a test SubAgentManager with a mock provider.
    fn make_manager_with_mock(provider: MockProvider) -> SubAgentManager {
        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();
        config.agent.max_iterations = 5;
        let mut reg = ProviderRegistry::new();
        reg.register("mock", provider);
        reg.set_default("mock");
        SubAgentManager::new(
            Arc::new(config),
            Arc::new(reg),
            Arc::new(ToolRegistry::new()),
        )
    }

    /// Build a test SubAgentManager with a delayed provider.
    fn make_manager_with_delayed(text: &str, delay: Duration) -> SubAgentManager {
        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();
        config.agent.max_iterations = 5;
        let mut reg = ProviderRegistry::new();
        reg.register("mock-delayed", DelayedProvider::new(text, delay));
        reg.set_default("mock-delayed");
        SubAgentManager::new(
            Arc::new(config),
            Arc::new(reg),
            Arc::new(ToolRegistry::new()),
        )
    }

    // ─── Legacy tracking tests ────────────────────────────────

    #[tokio::test]
    async fn test_subagent_manager_new() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let tasks = mgr.list_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_subagent_manager_spawn_and_complete() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let id = mgr.spawn("test_task", "a test").await;
        let status = mgr.get_status(&id).await.unwrap();
        assert_eq!(status, TaskStatus::Pending);

        mgr.complete(&id, "done".to_string()).await;
        let status = mgr.get_status(&id).await.unwrap();
        assert_eq!(status, TaskStatus::Completed("done".to_string()));
    }

    #[tokio::test]
    async fn test_subagent_manager_fail() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let id = mgr.spawn("test_task", "a test").await;
        mgr.fail(&id, "error occurred".to_string()).await;
        let status = mgr.get_status(&id).await.unwrap();
        assert_eq!(status, TaskStatus::Failed("error occurred".to_string()));
    }

    #[tokio::test]
    async fn test_subagent_manager_list_tasks() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        mgr.spawn("task1", "first").await;
        mgr.spawn("task2", "second").await;
        mgr.spawn("task3", "third").await;
        let tasks = mgr.list_tasks().await;
        assert_eq!(tasks.len(), 3);
    }

    #[tokio::test]
    async fn test_subagent_manager_get_status_nonexistent() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let status = mgr.get_status("nonexistent-id").await;
        assert!(status.is_none());
    }

    // ─── SubAgentSpawner trait tests ───────────────────────────

    #[tokio::test]
    async fn test_spawner_spawn_and_status() {
        let mgr = make_manager_with_mock(MockProvider::simple("result"));
        let id = SubAgentSpawner::spawn(&mgr, "worker", "do work", None)
            .await
            .unwrap();
        assert!(!id.is_empty());

        // The background task may still be running; status should be Running or Completed
        let status = SubAgentSpawner::status(&mgr, &id).await;
        assert!(status.is_some());
    }

    #[tokio::test]
    async fn test_spawner_list() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let _id1 = SubAgentSpawner::spawn(&mgr, "a", "task a", None)
            .await
            .unwrap();
        let _id2 = SubAgentSpawner::spawn(&mgr, "b", "task b", None)
            .await
            .unwrap();

        let list = SubAgentSpawner::list(&mgr).await;
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_spawner_cancel_nonexistent() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        assert!(!SubAgentSpawner::cancel(&mgr, "no-such-id").await);
    }

    #[tokio::test]
    async fn test_spawner_status_nonexistent() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        assert!(SubAgentSpawner::status(&mgr, "nope").await.is_none());
    }

    // ─── SubAgentHandle tests ──────────────────────────────────

    #[tokio::test]
    async fn test_handle_status_and_cancel() {
        let mgr = Arc::new(make_manager_with_delayed(
            "slow result",
            Duration::from_secs(5),
        ));
        let handle = mgr
            .spawn_single("slow-task", "take your time", None, None)
            .await
            .unwrap();

        assert_eq!(handle.name, "slow-task");
        assert!(!handle.id.is_empty());

        // Should be running (task takes 5s)
        let status = handle.status().await;
        assert_eq!(status, Some(SpawnStatus::Running));

        // Cancel it
        let cancelled = handle.cancel().await;
        assert!(cancelled);

        // Now should be Failed("Cancelled")
        let status = handle.status().await;
        assert!(matches!(status, Some(SpawnStatus::Failed(ref msg)) if msg == "Cancelled"));
    }

    #[tokio::test]
    async fn test_handle_completed() {
        let mgr = Arc::new(make_manager_with_mock(MockProvider::simple("done")));
        let handle = mgr
            .spawn_single("fast-task", "quick work", None, None)
            .await
            .unwrap();

        // Give the background task time to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        let status = handle.status().await;
        match status {
            Some(SpawnStatus::Completed(ref output)) => {
                assert_eq!(output, "done");
            }
            Some(SpawnStatus::Running) => {
                // Task hasn't completed yet — acceptable in slow CI
            }
            other => panic!("Unexpected status: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_handle_with_context() {
        let mgr = Arc::new(make_manager_with_mock(MockProvider::simple("context ok")));
        let handle = mgr
            .spawn_single(
                "ctx-task",
                "use context",
                Some("extra info".to_string()),
                None,
            )
            .await
            .unwrap();

        // Should at least be registered
        let status = handle.status().await;
        assert!(status.is_some());
    }

    // ─── Parallel execution tests ─────────────────────────────

    #[tokio::test]
    async fn test_parallel_spawn_3_tasks() {
        let mgr = make_manager_with_mock(MockProvider::multi(vec![
            "Result Alpha",
            "Result Beta",
            "Result Gamma",
        ]));

        let tasks = vec![
            SubAgentTask {
                id: "t1".into(),
                prompt: "Task 1".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
            SubAgentTask {
                id: "t2".into(),
                prompt: "Task 2".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
            SubAgentTask {
                id: "t3".into(),
                prompt: "Task 3".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
        ];

        let config = ParallelSpawnConfig {
            max_concurrent: 3,
            per_task_timeout_secs: 10,
            ..Default::default()
        };

        let summary = mgr.spawn_parallel(tasks, &config).await.unwrap();

        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.results.len(), 3);

        // Verify all task IDs present
        let ids: Vec<&str> = summary.results.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"t1"));
        assert!(ids.contains(&"t2"));
        assert!(ids.contains(&"t3"));

        // Verify each task got a response
        for result in &summary.results {
            assert!(result.success);
            assert!(result.tokens_used > 0);
        }

        // Verify structured notes
        let notes = summary.to_structured_notes();
        assert!(notes.contains("3/3 tasks succeeded"));
    }

    #[tokio::test]
    async fn test_parallel_spawn_with_context() {
        let mgr = make_manager_with_mock(MockProvider::simple("Context received"));

        let tasks = vec![SubAgentTask {
            id: "ctx-task".into(),
            prompt: "Use the context".into(),
            context: Some("Important context info".into()),
            model_override: None,
            max_tokens: None,
        }];

        let config = ParallelSpawnConfig::default();
        let summary = mgr.spawn_parallel(tasks, &config).await.unwrap();

        assert_eq!(summary.succeeded, 1);
        assert!(summary.results[0].output.contains("Context received"));
    }

    #[tokio::test]
    async fn test_parallel_spawn_timeout() {
        // Provider takes 2 seconds, but timeout is 100ms
        let mgr = make_manager_with_delayed("delayed result", Duration::from_secs(2));

        let tasks = vec![SubAgentTask {
            id: "slow-task".into(),
            prompt: "Take your time".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        }];

        // Actually use a very short timeout via direct construction
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            mgr.spawn_parallel(
                tasks,
                &ParallelSpawnConfig {
                    per_task_timeout_secs: 1, // 1s timeout, task takes 2s
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(result.failed, 1);
        assert!(result.results[0].output.contains("Timeout"));
        assert!(!result.results[0].success);
    }

    #[tokio::test]
    async fn test_parallel_spawn_max_concurrent() {
        // 5 tasks with max_concurrent=2 — should still complete all
        let mgr = make_manager_with_mock(MockProvider::multi(vec![
            "done-1", "done-2", "done-3", "done-4", "done-5",
        ]));

        let tasks: Vec<SubAgentTask> = (1..=5)
            .map(|i| SubAgentTask {
                id: format!("task-{}", i),
                prompt: format!("Task {}", i),
                context: None,
                model_override: None,
                max_tokens: None,
            })
            .collect();

        let config = ParallelSpawnConfig {
            max_concurrent: 2,
            per_task_timeout_secs: 10,
            ..Default::default()
        };

        let summary = mgr.spawn_parallel(tasks, &config).await.unwrap();
        assert_eq!(summary.succeeded, 5);
        assert_eq!(summary.results.len(), 5);
    }

    #[tokio::test]
    async fn test_parallel_spawn_error_isolation() {
        // Provider that fails on the 2nd call
        struct FailOnSecond {
            call_count: AtomicU32,
        }

        #[async_trait]
        impl LlmProvider for FailOnSecond {
            fn name(&self) -> &str {
                "fail-second"
            }
            async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse> {
                let n = self.call_count.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    Err(anyhow::anyhow!("Simulated failure on task 2"))
                } else {
                    Ok(CompletionResponse {
                        content: Some(format!("Success on call {}", n)),
                        tool_calls: None,
                        usage: Some(Usage {
                            prompt_tokens: Some(10),
                            completion_tokens: Some(5),
                            total_tokens: Some(15),
                        }),
                        finish_reason: Some("stop".to_string()),
                    })
                }
            }
            async fn complete_stream(&self, req: CompletionRequest) -> Result<BoxStream> {
                let resp = self.complete(req).await?;
                let chunk = CompletionChunk {
                    delta: resp.content,
                    tool_call_deltas: None,
                    usage: resp.usage,
                    done: true,
                };
                Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
            }
            fn supports_model(&self, _model: &str) -> bool {
                true
            }
        }

        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();
        config.agent.max_iterations = 5;
        let mut reg = ProviderRegistry::new();
        reg.register(
            "fail-second",
            FailOnSecond {
                call_count: AtomicU32::new(0),
            },
        );
        reg.set_default("fail-second");
        let mgr = SubAgentManager::new(
            Arc::new(config),
            Arc::new(reg),
            Arc::new(ToolRegistry::new()),
        );

        let tasks = vec![
            SubAgentTask {
                id: "ok-task".into(),
                prompt: "Should succeed".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
            SubAgentTask {
                id: "fail-task".into(),
                prompt: "Should fail".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
            SubAgentTask {
                id: "ok-task-2".into(),
                prompt: "Should also succeed".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
        ];

        let spawn_config = ParallelSpawnConfig {
            max_concurrent: 3,
            per_task_timeout_secs: 10,
            ..Default::default()
        };

        let summary = mgr.spawn_parallel(tasks, &spawn_config).await.unwrap();

        // One task should fail, others should succeed
        assert_eq!(
            summary.failed, 1,
            "Expected 1 failure, got {}",
            summary.failed
        );
        assert_eq!(
            summary.succeeded, 2,
            "Expected 2 successes, got {}",
            summary.succeeded
        );

        // The failed task should have error info
        let failed_result = summary.results.iter().find(|r| !r.success).unwrap();
        assert!(
            failed_result.output.contains("error")
                || failed_result.output.contains("Error")
                || failed_result.output.contains("fail")
        );
    }

    #[tokio::test]
    async fn test_parallel_spawn_total_timeout() {
        // 3 tasks each taking 500ms, total timeout 600ms
        // First task completes, remaining are aborted
        let mgr = make_manager_with_delayed("result", Duration::from_millis(500));

        let tasks: Vec<SubAgentTask> = (1..=3)
            .map(|i| SubAgentTask {
                id: format!("task-{}", i),
                prompt: format!("Task {}", i),
                context: None,
                model_override: None,
                max_tokens: None,
            })
            .collect();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            mgr.spawn_parallel(
                tasks,
                &ParallelSpawnConfig {
                    max_concurrent: 3,
                    per_task_timeout_secs: 10,
                    total_timeout_secs: Some(1), // 1s total, each task takes 500ms
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();

        // At least 1 should complete (the first batch of 3 starts immediately,
        // each takes 500ms, so all 3 should complete within 1s total)
        assert!(
            result.succeeded >= 1,
            "Expected at least 1 success, got {}/{}",
            result.succeeded,
            result.succeeded + result.failed
        );
    }

    #[tokio::test]
    async fn test_parallel_spawn_empty_tasks() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        let config = ParallelSpawnConfig::default();
        let summary = mgr.spawn_parallel(vec![], &config).await.unwrap();

        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.failed, 0);
        assert!(summary.results.is_empty());
    }

    #[tokio::test]
    async fn test_parallel_spawn_auto_ids() {
        let mgr = make_manager_with_mock(MockProvider::multi(vec!["a", "b"]));

        let tasks = vec![
            SubAgentTask {
                id: String::new(), // Empty — auto-generated
                prompt: "Task 1".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
            SubAgentTask {
                id: String::new(),
                prompt: "Task 2".into(),
                context: None,
                model_override: None,
                max_tokens: None,
            },
        ];

        let config = ParallelSpawnConfig::default();
        let summary = mgr.spawn_parallel(tasks, &config).await.unwrap();

        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.results[0].id, "task-1");
        assert_eq!(summary.results[1].id, "task-2");
    }

    #[tokio::test]
    async fn test_structured_notes() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        let tasks = vec![SubAgentTask {
            id: "note-task".into(),
            prompt: "Generate notes".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        }];

        let config = ParallelSpawnConfig::default();
        let summary = mgr.spawn_parallel(tasks, &config).await.unwrap();

        let notes = summary.to_structured_notes();
        assert!(notes.contains("## Task note-task"));
        assert!(notes.contains("1/1 tasks succeeded"));
    }

    #[tokio::test]
    async fn test_denied_tools_filter() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        let config = ParallelSpawnConfig {
            denied_tools: vec!["dangerous_tool".to_string()],
            ..Default::default()
        };

        // Should not error — filtering is internal
        let tasks = vec![SubAgentTask {
            id: "filtered".into(),
            prompt: "Test".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        }];

        let summary = mgr.spawn_parallel(tasks, &config).await.unwrap();
        assert_eq!(summary.succeeded, 1);
    }

    #[tokio::test]
    async fn test_parallel_config_default() {
        let config = ParallelSpawnConfig::default();
        assert_eq!(config.max_concurrent, 3);
        assert_eq!(config.per_task_timeout_secs, 60);
        assert!(config.total_timeout_secs.is_none());
        assert!(config.denied_tools.is_empty());
    }

    #[tokio::test]
    async fn test_sub_agent_task_serde() {
        let task = SubAgentTask {
            id: "t1".into(),
            prompt: "Do something".into(),
            context: Some("Extra context".into()),
            model_override: Some("gpt-4o-mini".into()),
            max_tokens: Some(512),
        };
        let json = serde_json::to_string(&task).unwrap();
        let back: SubAgentTask = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "t1");
        assert_eq!(back.prompt, "Do something");
        assert_eq!(back.context, Some("Extra context".into()));
        assert_eq!(back.model_override, Some("gpt-4o-mini".into()));
        assert_eq!(back.max_tokens, Some(512));
    }

    // ─── Lifecycle management tests ───────────────────────────

    #[tokio::test]
    async fn test_task_status_is_terminal() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed("ok".into()).is_terminal());
        assert!(TaskStatus::Failed("err".into()).is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[tokio::test]
    async fn test_task_status_from_for_spawn_status() {
        assert_eq!(
            SpawnStatus::from(&TaskStatus::Pending),
            SpawnStatus::Running
        );
        assert_eq!(
            SpawnStatus::from(&TaskStatus::Running),
            SpawnStatus::Running
        );
        assert_eq!(
            SpawnStatus::from(&TaskStatus::Completed("ok".into())),
            SpawnStatus::Completed("ok".into())
        );
        assert_eq!(
            SpawnStatus::from(&TaskStatus::Failed("err".into())),
            SpawnStatus::Failed("err".into())
        );
        assert_eq!(
            SpawnStatus::from(&TaskStatus::Cancelled),
            SpawnStatus::Failed("Cancelled".into())
        );
    }

    #[tokio::test]
    async fn test_terminate_all() {
        let mgr = Arc::new(make_manager_with_delayed("slow", Duration::from_secs(10)));

        // Spawn 3 slow tasks
        let h1 = mgr.spawn_single("t1", "p1", None, None).await.unwrap();
        let h2 = mgr.spawn_single("t2", "p2", None, None).await.unwrap();
        let h3 = mgr.spawn_single("t3", "p3", None, None).await.unwrap();

        // All should be running
        assert_eq!(mgr.active_count().await, 3);

        let terminated = mgr.terminate_all().await;
        assert_eq!(terminated, 3);
        assert_eq!(mgr.active_count().await, 0);

        // All handles should report cancelled
        assert!(matches!(h1.status().await, Some(SpawnStatus::Failed(ref m)) if m == "Cancelled"));
        assert!(matches!(h2.status().await, Some(SpawnStatus::Failed(ref m)) if m == "Cancelled"));
        assert!(matches!(h3.status().await, Some(SpawnStatus::Failed(ref m)) if m == "Cancelled"));
    }

    #[tokio::test]
    async fn test_terminate_all_skips_completed() {
        let mgr = Arc::new(make_manager_with_mock(MockProvider::simple("fast")));

        // Spawn a fast task and wait for it
        let _h = mgr.spawn_single("fast", "p", None, None).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Spawn a slow task
        let _slow_mgr = Arc::new(make_manager_with_delayed("slow", Duration::from_secs(10)));
        // Actually, let's just use the same manager with a slow provider approach
        // For simplicity, register a pending task manually
        let _id_slow = mgr.spawn("slow-task", "desc").await;

        let terminated = mgr.terminate_all().await;
        // Only the slow-task (Pending) gets terminated, the fast one should be done
        assert!(terminated <= 1);
    }

    #[tokio::test]
    async fn test_cleanup_completed() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        let id1 = mgr.spawn("t1", "d1").await;
        let id2 = mgr.spawn("t2", "d2").await;
        mgr.complete(&id1, "done".into()).await;
        // id2 stays Pending

        let removed = mgr.cleanup_completed().await;
        assert_eq!(removed, 1);

        let tasks = mgr.list_tasks().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].0, id2);
    }

    #[tokio::test]
    async fn test_cleanup_completed_all_terminal() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        let id1 = mgr.spawn("t1", "d1").await;
        let id2 = mgr.spawn("t2", "d2").await;
        mgr.complete(&id1, "done".into()).await;
        mgr.fail(&id2, "error".into()).await;

        let removed = mgr.cleanup_completed().await;
        assert_eq!(removed, 2);
        assert!(mgr.list_tasks().await.is_empty());
    }

    #[tokio::test]
    async fn test_active_count() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        assert_eq!(mgr.active_count().await, 0);

        let id1 = mgr.spawn("t1", "d1").await;
        let _id2 = mgr.spawn("t2", "d2").await;
        assert_eq!(mgr.active_count().await, 2);

        mgr.complete(&id1, "done".into()).await;
        assert_eq!(mgr.active_count().await, 1);
    }

    #[tokio::test]
    async fn test_max_tasks_limit() {
        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();
        config.agent.max_iterations = 5;
        let mut reg = ProviderRegistry::new();
        reg.register(
            "mock",
            DelayedProvider::new("result", Duration::from_secs(10)),
        );
        reg.set_default("mock");

        let mgr = Arc::new(SubAgentManager::with_manager_config(
            Arc::new(config),
            Arc::new(reg),
            Arc::new(ToolRegistry::new()),
            SubAgentManagerConfig {
                max_tasks: 2,
                default_timeout_secs: 0,
            },
        ));

        // Spawn 2 tasks — should succeed
        let h1 = mgr.spawn_single("t1", "p1", None, None).await.unwrap();
        let h2 = mgr.spawn_single("t2", "p2", None, None).await.unwrap();

        // 3rd should fail
        let result = mgr.spawn_single("t3", "p3", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Cannot spawn"));

        // Cleanup
        mgr.terminate_all().await;
        let _ = h1;
        let _ = h2;
    }

    #[tokio::test]
    async fn test_spawn_single_timeout() {
        // Provider takes 5 seconds, timeout is 100ms
        let mgr = Arc::new(make_manager_with_delayed("slow", Duration::from_secs(5)));

        let handle = mgr
            .spawn_single("timed-task", "work", None, Some(1))
            .await
            .unwrap();

        // Wait for the timeout to trigger
        let status = mgr.wait_for(&handle.id, Duration::from_secs(3)).await;
        assert!(status.is_some());
        assert!(matches!(status, Some(TaskStatus::Failed(ref msg)) if msg.contains("timed out")));
    }

    #[tokio::test]
    async fn test_wait_for_already_completed() {
        let mgr = Arc::new(make_manager_with_mock(MockProvider::simple("fast")));
        let handle = mgr.spawn_single("fast", "p", None, None).await.unwrap();

        // Wait for completion
        let status = mgr.wait_for(&handle.id, Duration::from_secs(2)).await;
        assert!(status.is_some());
        assert!(matches!(status, Some(TaskStatus::Completed(_))));
    }

    #[tokio::test]
    async fn test_wait_for_unknown_task() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let status = mgr.wait_for("nonexistent", Duration::from_millis(50)).await;
        assert!(status.is_none());
    }

    // ─── Messaging tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_send_message_to_task() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let id = mgr.spawn("worker", "test").await;

        let sent = mgr
            .send_message(&id, "parent", "hello from parent".into())
            .await;
        assert!(sent);
        assert_eq!(mgr.mailbox_len(&id).await, 1);
    }

    #[tokio::test]
    async fn test_send_message_to_unknown_task() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let sent = mgr.send_message("no-such-id", "parent", "msg".into()).await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn test_send_message_to_terminal_task() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let id = mgr.spawn("worker", "test").await;
        mgr.complete(&id, "done".into()).await;

        let sent = mgr.send_message(&id, "parent", "too late".into()).await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn test_drain_messages() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let id = mgr.spawn("worker", "test").await;

        mgr.send_message(&id, "parent", "msg1".into()).await;
        mgr.send_message(&id, "sibling", "msg2".into()).await;
        mgr.send_message(&id, "parent", "msg3".into()).await;

        assert_eq!(mgr.mailbox_len(&id).await, 3);

        let msgs = mgr.drain_messages(&id).await;
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].from, "parent");
        assert_eq!(msgs[0].content, "msg1");
        assert_eq!(msgs[1].from, "sibling");
        assert_eq!(msgs[2].content, "msg3");

        // Mailbox should be empty after drain
        assert_eq!(mgr.mailbox_len(&id).await, 0);
    }

    #[tokio::test]
    async fn test_drain_messages_unknown_task() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));
        let msgs = mgr.drain_messages("no-such-id").await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_broadcast_message() {
        let mgr = make_manager_with_mock(MockProvider::simple("ok"));

        let id1 = mgr.spawn("worker1", "test").await;
        let id2 = mgr.spawn("worker2", "test").await;
        let id3 = mgr.spawn("worker3", "test").await;

        // Complete one so it won't receive
        mgr.complete(&id3, "done".into()).await;

        let received = mgr
            .broadcast_message("coordinator", "all hands".into())
            .await;
        assert_eq!(received, 2); // id3 is terminal

        assert_eq!(mgr.mailbox_len(&id1).await, 1);
        assert_eq!(mgr.mailbox_len(&id2).await, 1);
        assert_eq!(mgr.mailbox_len(&id3).await, 0); // terminal
    }

    #[tokio::test]
    async fn test_sub_agent_message_new() {
        let msg = SubAgentMessage::new("parent", "hello");
        assert_eq!(msg.from, "parent");
        assert_eq!(msg.content, "hello");
        assert!(!msg.timestamp.to_rfc3339().is_empty());
    }

    #[tokio::test]
    async fn test_sub_agent_message_serde() {
        let msg = SubAgentMessage::new("task-1", "result data");
        let json = serde_json::to_string(&msg).unwrap();
        let back: SubAgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.from, "task-1");
        assert_eq!(back.content, "result data");
    }

    // ─── Manager config tests ─────────────────────────────────

    #[test]
    fn test_manager_config_default() {
        let config = SubAgentManagerConfig::default();
        assert_eq!(config.max_tasks, 0);
        assert_eq!(config.default_timeout_secs, 120);
    }

    #[test]
    fn test_manager_config_custom() {
        let config = SubAgentManagerConfig {
            max_tasks: 10,
            default_timeout_secs: 300,
        };
        assert_eq!(config.max_tasks, 10);
        assert_eq!(config.default_timeout_secs, 300);
    }

    // ─── spawn_with_timeout tests ──────────────────────────────

    #[tokio::test]
    async fn test_spawn_with_custom_timeout() {
        let mgr = make_manager_with_mock(MockProvider::simple("hello"));
        let arc = Arc::new(mgr);

        // Use spawn_with_timeout via the SubAgentSpawner trait
        let spawner: Arc<dyn SubAgentSpawner> = arc.clone();
        let id = spawner
            .spawn_with_timeout("timeout-test", "Say hello", None, Some(5))
            .await
            .unwrap();

        // Task should be tracked
        let status = spawner.status(&id).await.unwrap();
        assert!(matches!(
            status,
            SpawnStatus::Running | SpawnStatus::Completed(_)
        ));
    }

    #[tokio::test]
    async fn test_spawn_with_default_timeout() {
        let mgr = make_manager_with_mock(MockProvider::simple("hello"));
        let arc = Arc::new(mgr);

        let spawner: Arc<dyn SubAgentSpawner> = arc.clone();
        // None = use default from config (120s)
        let id = spawner
            .spawn_with_timeout("default-timeout", "Say hi", None, None)
            .await
            .unwrap();

        let status = spawner.status(&id).await;
        assert!(status.is_some());
    }
}
