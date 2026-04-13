//! Cron service — manages scheduled job lifecycle with real cron expression parsing.
//!
//! Handles tick-based scheduling, job persistence, execution dispatch,
//! and bus integration. Mirrors the Python `cron/service.py` CronService.

use crate::state_store::{CronStateStore, FileStateStore};
use crate::types::*;
use anyhow::{Context, Result};
use chrono::Local;
use cron::Schedule;
use nanobot_bus::events::AgentEvent;
use nanobot_bus::MessageBus;
use nanobot_core::{CRON_TICK_INTERVAL_SECS, MAX_CRON_RUN_HISTORY};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

/// The cron scheduler service.
///
/// Manages cron jobs with real expression parsing, CRUD operations,
/// state persistence, and optional bus integration for firing events.
pub struct CronService {
    store_path: PathBuf,
    store: parking_lot::Mutex<CronStore>,
    state_store: Box<dyn CronStateStore>,
    bus: Option<Arc<MessageBus>>,
    running: Arc<RwLock<bool>>,
}

impl CronService {
    /// Create a new CronService with the given storage directory.
    pub fn new(cron_dir: PathBuf) -> Result<Self> {
        if !cron_dir.exists() {
            std::fs::create_dir_all(&cron_dir)?;
        }

        let store_path = cron_dir.join("cron_state.json");
        let store = if store_path.exists() {
            let content = std::fs::read_to_string(&store_path)
                .with_context(|| format!("Failed to read {}", store_path.display()))?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            CronStore::default()
        };

        let state_store_path = cron_dir.join("cron_job_states.json");
        let state_store = FileStateStore::new(state_store_path)?;

        let svc = Self {
            store_path,
            store: parking_lot::Mutex::new(store),
            state_store: Box::new(state_store),
            bus: None,
            running: Arc::new(RwLock::new(false)),
        };

        svc.recover_states();
        Ok(svc)
    }

    /// Create a CronService wired to a MessageBus for event dispatch.
    pub fn with_bus(cron_dir: PathBuf, bus: MessageBus) -> Result<Self> {
        let mut svc = Self::new(cron_dir)?;
        svc.bus = Some(Arc::new(bus));
        Ok(svc)
    }

    /// Create a CronService with a custom state store (for testing).
    pub fn with_state_store(
        cron_dir: PathBuf,
        state_store: Box<dyn CronStateStore>,
    ) -> Result<Self> {
        if !cron_dir.exists() {
            std::fs::create_dir_all(&cron_dir)?;
        }

        let store_path = cron_dir.join("cron_state.json");
        let store = if store_path.exists() {
            let content = std::fs::read_to_string(&store_path)
                .with_context(|| format!("Failed to read {}", store_path.display()))?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            CronStore::default()
        };

        let svc = Self {
            store_path,
            store: parking_lot::Mutex::new(store),
            state_store,
            bus: None,
            running: Arc::new(RwLock::new(false)),
        };

        svc.recover_states();
        Ok(svc)
    }

    /// Recover job states from the state store on startup.
    ///
    /// For each job in the CronStore, checks if a previous state exists
    /// and performs catch-up if the job was due while the service was down.
    fn recover_states(&self) {
        let mut store = self.store.lock();
        let states = match self.state_store.list_states() {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to load job states for recovery: {}", e);
                return;
            }
        };

        let now = Local::now();
        let mut caught_up = 0;

        for job in &mut store.jobs {
            if let Some(prev_state) = states.get(&job.id) {
                // Restore run count and error state
                if let Some(name) = &prev_state.job_name {
                    if job.name.is_none() {
                        job.name = Some(name.clone());
                    }
                }

                // Catch-up: if the job was active and its next_run is in the past,
                // it missed one or more executions.
                if job.state == JobState::Active {
                    if let Some(next_run) = job.next_run {
                        if next_run <= now {
                            // Count missed executions (rough estimate)
                            caught_up += 1;
                            info!(
                                "Catch-up: job '{}' was due at {}, rescheduling",
                                job.id,
                                next_run.format("%Y-%m-%d %H:%M:%S")
                            );
                            // Reschedule to next future occurrence
                            job.next_run = compute_next_run(&job.schedule);
                        }
                    }
                }
            }
        }

        drop(store);
        if caught_up > 0 {
            info!("Recovered {} job(s) that were due during downtime", caught_up);
            let _ = self.persist();
        }
    }

    /// Add a new cron job.
    pub fn add_job(
        &self,
        schedule: CronSchedule,
        payload: CronPayload,
        name: Option<String>,
    ) -> Result<CronJob> {
        self.add_job_with_priority(schedule, payload, name, 0)
    }

    /// Add a new cron job with an explicit priority.
    ///
    /// Higher priority jobs are executed first when multiple jobs are
    /// due in the same tick. Priority 0 is the default (lowest).
    pub fn add_job_with_priority(
        &self,
        schedule: CronSchedule,
        payload: CronPayload,
        name: Option<String>,
        priority: u32,
    ) -> Result<CronJob> {
        // Validate schedule
        validate_schedule(&schedule)?;

        let id = Uuid::new_v4().to_string();
        let next_run = compute_next_run(&schedule);

        let job = CronJob {
            id: id.clone(),
            name: name.clone(),
            schedule,
            payload,
            state: JobState::Active,
            next_run,
            last_run: None,
            history: Vec::new(),
            is_system: false,
            priority,
        };

        self.store.lock().jobs.push(job.clone());
        self.persist()?;
        self.save_job_state(&job);
        info!("Added cron job: {}", job.id);
        Ok(job)
    }

    /// Update an existing cron job's schedule, payload, and/or priority.
    ///
    /// Returns the updated job, or None if not found.
    pub fn update_job(
        &self,
        job_id: &str,
        schedule: Option<CronSchedule>,
        payload: Option<CronPayload>,
        name: Option<String>,
        priority: Option<u32>,
    ) -> Result<Option<CronJob>> {
        // Validate new schedule if provided
        if let Some(ref s) = schedule {
            validate_schedule(s)?;
        }

        let mut store = self.store.lock();
        let job = match store.jobs.iter_mut().find(|j| j.id == job_id) {
            Some(j) => j,
            None => return Ok(None),
        };

        if let Some(s) = schedule {
            job.schedule = s;
            job.next_run = compute_next_run(&job.schedule);
        }
        if let Some(p) = payload {
            job.payload = p;
        }
        if let Some(n) = name {
            job.name = if n.is_empty() { None } else { Some(n) };
        }
        if let Some(p) = priority {
            job.priority = p;
        }

        let updated = job.clone();
        drop(store);
        self.persist()?;
        self.save_job_state(&updated);
        info!("Updated cron job: {}", job_id);
        Ok(Some(updated))
    }

    /// Remove a cron job by ID. System jobs cannot be removed.
    pub fn remove_job(&self, job_id: &str) -> Result<bool> {
        let mut store = self.store.lock();
        let before = store.jobs.len();
        store.jobs.retain(|j| j.id != job_id || j.is_system);
        let removed = store.jobs.len() < before;
        drop(store);

        if removed {
            self.persist()?;
            let _ = self.state_store.delete_state(job_id);
            info!("Removed cron job: {}", job_id);
        }
        Ok(removed)
    }

    /// Pause a cron job by ID.
    pub fn pause_job(&self, job_id: &str) -> Result<bool> {
        let mut store = self.store.lock();
        let found = match store.jobs.iter_mut().find(|j| j.id == job_id) {
            Some(j) if j.state == JobState::Active => {
                j.state = JobState::Paused;
                true
            }
            _ => false,
        };
        drop(store);

        if found {
            self.persist()?;
            // Save updated state
            if let Some(job) = self.get_job(job_id) {
                self.save_job_state(&job);
            }
            info!("Paused cron job: {}", job_id);
        }
        Ok(found)
    }

    /// Resume a paused cron job by ID.
    pub fn resume_job(&self, job_id: &str) -> Result<bool> {
        let mut store = self.store.lock();
        let found = match store.jobs.iter_mut().find(|j| j.id == job_id) {
            Some(j) if j.state == JobState::Paused => {
                j.state = JobState::Active;
                j.next_run = compute_next_run(&j.schedule);
                true
            }
            _ => false,
        };
        drop(store);

        if found {
            self.persist()?;
            if let Some(job) = self.get_job(job_id) {
                self.save_job_state(&job);
            }
            info!("Resumed cron job: {}", job_id);
        }
        Ok(found)
    }

    /// Get all jobs.
    pub fn list_jobs(&self) -> Vec<CronJob> {
        self.store.lock().jobs.clone()
    }

    /// Get a single job by ID.
    pub fn get_job(&self, job_id: &str) -> Option<CronJob> {
        self.store
            .lock()
            .jobs
            .iter()
            .find(|j| j.id == job_id)
            .cloned()
    }

    /// Tick — check for due jobs, fire them via bus, and return them.
    pub fn tick(&self) -> Vec<CronJob> {
        let now = Local::now();
        let mut due = Vec::new();
        let mut store = self.store.lock();

        for job in &mut store.jobs {
            if job.state != JobState::Active {
                continue;
            }

            if let Some(next_run) = &job.next_run {
                if next_run <= &now {
                    due.push(job.clone());

                    // Record the run
                    job.last_run = Some(now);
                    job.history.push(CronRunRecord {
                        timestamp: now,
                        result: None,
                        success: true,
                    });

                    // Trim history
                    if job.history.len() > MAX_CRON_RUN_HISTORY {
                        let excess = job.history.len() - MAX_CRON_RUN_HISTORY;
                        job.history.drain(0..excess);
                    }

                    // Compute next run or mark done
                    match job.schedule.kind {
                        ScheduleKind::At => {
                            job.state = JobState::Done;
                        }
                        ScheduleKind::Every | ScheduleKind::Cron => {
                            job.next_run = compute_next_run(&job.schedule);
                        }
                    }
                }
            }
        }

        drop(store);

        // Sort due jobs by priority (highest first) before emitting events.
        // This ensures higher-priority jobs are processed first when multiple
        // jobs are due in the same tick.
        due.sort_by(|a, b| b.priority.cmp(&a.priority));

        // Emit events via bus
        if let Some(bus) = &self.bus {
            for job in &due {
                bus.emit_event(AgentEvent::CronFired {
                    job_id: job.id.clone(),
                    job_name: job.name.clone(),
                    message: job.payload.message.clone(),
                });
            }
        }

        let _ = self.persist();

        // Save state for each fired job
        for job in &due {
            if let Some(current) = self.get_job(&job.id) {
                self.save_job_state(&current);
            }
        }

        due
    }

    /// Mark a job as completed with a result.
    pub fn mark_completed(&self, job_id: &str, result: Option<String>) {
        let is_error = result.as_ref().is_some_and(|r| r.starts_with("error:") || r.starts_with("Error"));
        let mut store = self.store.lock();
        if let Some(job) = store.jobs.iter_mut().find(|j| j.id == job_id) {
            if let Some(last_run) = job.history.last_mut() {
                last_run.result = result.clone();
                if is_error {
                    last_run.success = false;
                }
            }
        }
        drop(store);
        let _ = self.persist();

        // Update state store
        if let Some(current) = self.get_job(job_id) {
            self.save_job_state(&current);
        }
    }

    /// Run the cron scheduler as a background task.
    ///
    /// Periodically calls `tick()` at the configured interval.
    /// Stops when `stop()` is called.
    pub async fn run(&self) -> Result<()> {
        {
            let mut running = self.running.write().await;
            if *running {
                warn!("Cron service is already running");
                return Ok(());
            }
            *running = true;
        }

        info!(
            "Cron service started with {}s tick interval",
            CRON_TICK_INTERVAL_SECS
        );

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(
            CRON_TICK_INTERVAL_SECS,
        ));

        while *self.running.read().await {
            interval.tick().await;

            let due = self.tick();
            if !due.is_empty() {
                info!("Cron tick: {} job(s) fired", due.len());
            }
        }

        info!("Cron service stopped");
        Ok(())
    }

    /// Stop the cron scheduler.
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Check if the scheduler is running.
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Get the runtime state for a single job.
    pub fn get_job_state(&self, job_id: &str) -> Option<CronJobState> {
        self.state_store.load_state(job_id).ok().flatten()
    }

    /// Get runtime states for all jobs.
    pub fn list_job_states(&self) -> Vec<(CronJob, CronJobState)> {
        let jobs = self.list_jobs();
        let states = self.state_store.list_states().unwrap_or_default();
        jobs.into_iter()
            .map(|job| {
                let state = states.get(&job.id).cloned().unwrap_or_else(|| {
                    // Synthesize state from the job if no stored state exists
                    CronJobState {
                        job_name: job.name.clone(),
                        last_run: job.last_run.map(|dt| dt.with_timezone(&chrono::Utc)),
                        next_run: job.next_run.map(|dt| dt.with_timezone(&chrono::Utc)),
                        is_active: job.state == JobState::Active,
                        run_count: job.history.len() as u64,
                        last_error: None,
                    }
                });
                (job, state)
            })
            .collect()
    }

    /// Convert a CronJob into its CronJobState snapshot.
    fn job_to_state(&self, job: &CronJob) -> CronJobState {
        CronJobState {
            job_name: job.name.clone(),
            last_run: job.last_run.map(|dt| dt.with_timezone(&chrono::Utc)),
            next_run: job.next_run.map(|dt| dt.with_timezone(&chrono::Utc)),
            is_active: job.state == JobState::Active,
            run_count: job.history.len() as u64,
            last_error: job.history.iter().rev().find(|r| !r.success).and_then(|r| r.result.clone()),
        }
    }

    /// Save the runtime state for a job to the state store.
    fn save_job_state(&self, job: &CronJob) {
        let state = self.job_to_state(job);
        if let Err(e) = self.state_store.save_state(&job.id, &state) {
            warn!("Failed to save state for job {}: {}", job.id, e);
        }
    }

    /// Persist the store to disk.
    fn persist(&self) -> Result<()> {
        let store = self.store.lock();
        let json = serde_json::to_string_pretty(&*store)
            .with_context(|| "Failed to serialize cron store")?;
        std::fs::write(&self.store_path, json)
            .with_context(|| format!("Failed to write {}", self.store_path.display()))?;
        Ok(())
    }
}

/// Validate a cron schedule expression.
fn validate_schedule(schedule: &CronSchedule) -> Result<()> {
    match schedule.kind {
        ScheduleKind::At => {
            if schedule.at_ms.is_none() {
                anyhow::bail!("'at' schedule requires at_ms field");
            }
        }
        ScheduleKind::Every => {
            if schedule.every_ms.is_none() || schedule.every_ms.unwrap_or(0) <= 0 {
                anyhow::bail!("'every' schedule requires positive every_ms field");
            }
        }
        ScheduleKind::Cron => {
            if let Some(ref expr) = schedule.expr {
                Schedule::from_str(expr)
                    .with_context(|| format!("Invalid cron expression: '{}'", expr))?;
            } else {
                anyhow::bail!("'cron' schedule requires expr field");
            }
        }
    }
    Ok(())
}

/// Compute the next run time for a schedule using the `cron` crate for cron expressions.
fn compute_next_run(schedule: &CronSchedule) -> Option<chrono::DateTime<Local>> {
    let now = chrono::Utc::now();
    match schedule.kind {
        ScheduleKind::At => schedule.at_ms.and_then(|ms| {
            chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.with_timezone(&Local))
        }),
        ScheduleKind::Every => schedule
            .every_ms
            .map(|ms| (now + chrono::Duration::milliseconds(ms)).with_timezone(&Local)),
        ScheduleKind::Cron => schedule.expr.as_ref().and_then(|expr| {
            match Schedule::from_str(expr) {
                Ok(sched) => sched
                    .upcoming(chrono::Utc)
                    .next()
                    .map(|dt| dt.with_timezone(&Local)),
                Err(e) => {
                    warn!("Failed to parse cron expression '{}': {}", expr, e);
                    // Fallback: next minute
                    Some((now + chrono::Duration::minutes(1)).with_timezone(&Local))
                }
            }
        }),
    }
}

/// Parse a cron expression and return upcoming fire times (for testing/inspection).
pub fn upcoming_from_expression(
    expr: &str,
    count: usize,
) -> Result<Vec<chrono::DateTime<chrono::Utc>>> {
    let schedule =
        Schedule::from_str(expr).with_context(|| format!("Invalid cron expression: '{}'", expr))?;
    Ok(schedule.upcoming(chrono::Utc).take(count).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeDelta};

    fn make_service(dir: &std::path::Path) -> CronService {
        CronService::new(dir.to_path_buf()).unwrap()
    }

    fn make_at_schedule(past: bool) -> CronSchedule {
        let ms = if past {
            (Local::now() - TimeDelta::hours(1)).timestamp_millis()
        } else {
            (Local::now() + TimeDelta::hours(1)).timestamp_millis()
        };
        CronSchedule {
            kind: ScheduleKind::At,
            at_ms: Some(ms),
            every_ms: None,
            expr: None,
            tz: None,
        }
    }

    fn make_every_schedule() -> CronSchedule {
        CronSchedule {
            kind: ScheduleKind::Every,
            at_ms: None,
            every_ms: Some(60_000),
            expr: None,
            tz: None,
        }
    }

    fn make_cron_schedule(expr: &str) -> CronSchedule {
        CronSchedule {
            kind: ScheduleKind::Cron,
            at_ms: None,
            every_ms: None,
            expr: Some(expr.to_string()),
            tz: None,
        }
    }

    /// Helper: 7-field cron (sec min hour day month weekday year)
    /// Common patterns:
    ///   "0 0 * * * * *" — every hour
    ///   "0 */5 * * * * *" — every 5 minutes
    ///   "0 0 0 * * * *" — daily at midnight

    fn make_payload() -> CronPayload {
        CronPayload {
            message: "test".to_string(),
            channel: None,
            chat_id: None,
            deliver: false,
        }
    }

    fn make_payload_with(message: &str) -> CronPayload {
        CronPayload {
            message: message.to_string(),
            channel: Some("telegram".to_string()),
            chat_id: Some("chat123".to_string()),
            deliver: true,
        }
    }

    // === Basic CRUD ===

    #[test]
    fn test_add_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_at_schedule(false), make_payload(), Some("test".to_string()))
            .unwrap();
        assert_eq!(job.name.as_deref(), Some("test"));
        assert_eq!(job.state, JobState::Active);
        assert!(job.next_run.is_some());
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
    }

    #[test]
    fn test_remove_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_at_schedule(false), make_payload(), None)
            .unwrap();
        assert_eq!(svc.list_jobs().len(), 1);
        let removed = svc.remove_job(&job.id).unwrap();
        assert!(removed);
        assert!(svc.list_jobs().is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let removed = svc.remove_job("nope").unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_list_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(make_at_schedule(false), make_payload(), Some("a".to_string()))
            .unwrap();
        svc.add_job(make_every_schedule(), make_payload(), Some("b".to_string()))
            .unwrap();
        svc.add_job(make_cron_schedule("0 0 * * * * *"), make_payload(), Some("c".to_string()))
            .unwrap();
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 3);
    }

    #[test]
    fn test_get_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_at_schedule(false), make_payload(), Some("findme".to_string()))
            .unwrap();
        let found = svc.get_job(&job.id).unwrap();
        assert_eq!(found.name.as_deref(), Some("findme"));
        assert!(svc.get_job("nonexistent").is_none());
    }

    // === Update ===

    #[test]
    fn test_update_job_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), Some("orig".to_string()))
            .unwrap();

        let new_schedule = CronSchedule {
            kind: ScheduleKind::Every,
            at_ms: None,
            every_ms: Some(120_000),
            expr: None,
            tz: None,
        };
        let updated = svc
            .update_job(&job.id, Some(new_schedule), None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(updated.schedule.every_ms, Some(120_000));
    }

    #[test]
    fn test_update_job_payload() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        let new_payload = make_payload_with("updated message");
        let updated = svc
            .update_job(&job.id, None, Some(new_payload), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(updated.payload.message, "updated message");
        assert_eq!(updated.payload.channel.as_deref(), Some("telegram"));
    }

    #[test]
    fn test_update_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let result = svc.update_job("nope", None, None, None, None).unwrap();
        assert!(result.is_none());
    }

    // === Pause / Resume ===

    #[test]
    fn test_pause_resume_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        // Pause
        let paused = svc.pause_job(&job.id).unwrap();
        assert!(paused);
        let found = svc.get_job(&job.id).unwrap();
        assert_eq!(found.state, JobState::Paused);

        // Paused job should not fire on tick
        let due = svc.tick();
        assert!(due.is_empty());

        // Resume
        let resumed = svc.resume_job(&job.id).unwrap();
        assert!(resumed);
        let found = svc.get_job(&job.id).unwrap();
        assert_eq!(found.state, JobState::Active);
        assert!(found.next_run.is_some());
    }

    #[test]
    fn test_pause_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        assert!(!svc.pause_job("nope").unwrap());
        assert!(!svc.resume_job("nope").unwrap());
    }

    #[test]
    fn test_pause_already_paused() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();
        assert!(svc.pause_job(&job.id).unwrap());
        assert!(!svc.pause_job(&job.id).unwrap()); // Already paused
    }

    // === Tick ===

    #[test]
    fn test_tick_no_due() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(make_at_schedule(false), make_payload(), None)
            .unwrap();
        let due = svc.tick();
        assert!(due.is_empty());
    }

    #[test]
    fn test_tick_due_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_at_schedule(true), make_payload(), None)
            .unwrap();
        let due = svc.tick();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, job.id);
        // The "at" job should now be Done
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].state, JobState::Done);
    }

    #[test]
    fn test_tick_every_reschedules() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        // every_ms = 60_000 (1 minute), next_run is 1 minute in the future
        let _job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        // Manually set next_run to the past to trigger
        {
            let mut store = svc.store.lock();
            store.jobs[0].next_run = Some(Local::now() - TimeDelta::seconds(10));
        }

        let due = svc.tick();
        assert_eq!(due.len(), 1);

        // Job should still be Active with a new next_run
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].state, JobState::Active);
        assert!(jobs[0].next_run.is_some());
        assert!(jobs[0].next_run.unwrap() > Local::now());
    }

    #[test]
    fn test_tick_cron_reschedules() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let _job = svc
            .add_job(
                make_cron_schedule("0 0 * * * * *"),
                make_payload(),
                None,
            )
            .unwrap();

        // Manually set next_run to the past
        {
            let mut store = svc.store.lock();
            store.jobs[0].next_run = Some(Local::now() - TimeDelta::seconds(10));
        }

        let due = svc.tick();
        assert_eq!(due.len(), 1);

        // Should reschedule
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].state, JobState::Active);
        assert!(jobs[0].next_run.unwrap() > Local::now());
    }

    #[test]
    fn test_tick_records_history() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(make_at_schedule(true), make_payload(), None)
            .unwrap();

        svc.tick();
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].history.len(), 1);
        assert!(jobs[0].history[0].success);
        assert!(jobs[0].history[0].result.is_none());
    }

    #[test]
    fn test_mark_completed() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let _job = svc
            .add_job(make_at_schedule(true), make_payload(), None)
            .unwrap();
        svc.tick();
        svc.mark_completed(&_job.id, Some("ok".to_string()));

        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].history[0].result.as_deref(), Some("ok"));
    }

    // === State Persistence ===

    #[test]
    fn test_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let job_id = {
            let svc = make_service(dir.path());
            let job = svc
                .add_job(
                    make_at_schedule(false),
                    make_payload(),
                    Some("persist".to_string()),
                )
                .unwrap();
            job.id
        };
        // Create a new service from the same directory
        let svc2 = make_service(dir.path());
        let jobs = svc2.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job_id);
        assert_eq!(jobs[0].name.as_deref(), Some("persist"));
    }

    #[test]
    fn test_persist_file_is_cron_state_json() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(make_at_schedule(false), make_payload(), None)
            .unwrap();

        let state_file = dir.path().join("cron_state.json");
        assert!(state_file.exists());

        let content = std::fs::read_to_string(&state_file).unwrap();
        let store: CronStore = serde_json::from_str(&content).unwrap();
        assert_eq!(store.jobs.len(), 1);
    }

    #[test]
    fn test_persist_after_update() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), Some("orig".to_string()))
            .unwrap();
        svc.update_job(&job.id, None, Some(make_payload_with("new")), None, None)
            .unwrap();

        // Reload
        let svc2 = make_service(dir.path());
        let jobs = svc2.list_jobs();
        assert_eq!(jobs[0].payload.message, "new");
    }

    // === Cron Expression Parsing ===

    #[test]
    fn test_compute_next_run_every() {
        let schedule = make_every_schedule();
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        assert!(next.unwrap() > Local::now());
    }

    #[test]
    fn test_compute_next_run_at() {
        let future_ms = (Local::now() + TimeDelta::hours(2)).timestamp_millis();
        let schedule = CronSchedule {
            kind: ScheduleKind::At,
            at_ms: Some(future_ms),
            every_ms: None,
            expr: None,
            tz: None,
        };
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        assert!(next.unwrap() > Local::now());
    }

    #[test]
    fn test_compute_next_run_cron_every_minute() {
        let schedule = make_cron_schedule("0 0 * * * * *");
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        let next = next.unwrap();
        // Should be within the next hour
        assert!(next > Local::now());
        assert!(next < Local::now() + TimeDelta::hours(2));
    }

    #[test]
    fn test_compute_next_run_cron_specific() {
        // Every day at 00:00 UTC
        let schedule = make_cron_schedule("0 0 0 * * * *");
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        let next = next.unwrap();
        assert!(next > Local::now());
    }

    #[test]
    fn test_compute_next_run_cron_5field() {
        // 5-field cron: minute hour day month weekday
        let schedule = make_cron_schedule("0 */5 * * * * *");
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        let next = next.unwrap();
        assert!(next > Local::now());
        // Should be within 5 minutes
        assert!(next < Local::now() + TimeDelta::minutes(6));
    }

    #[test]
    fn test_validate_schedule_valid() {
        assert!(validate_schedule(&make_at_schedule(false)).is_ok());
        assert!(validate_schedule(&make_every_schedule()).is_ok());
        assert!(validate_schedule(&make_cron_schedule("0 0 * * * * *")).is_ok());
    }

    #[test]
    fn test_validate_schedule_invalid_cron_expr() {
        let bad = CronSchedule {
            kind: ScheduleKind::Cron,
            at_ms: None,
            every_ms: None,
            expr: Some("not a cron".to_string()),
            tz: None,
        };
        assert!(validate_schedule(&bad).is_err());
    }

    #[test]
    fn test_validate_schedule_missing_fields() {
        let no_at = CronSchedule {
            kind: ScheduleKind::At,
            at_ms: None,
            every_ms: None,
            expr: None,
            tz: None,
        };
        assert!(validate_schedule(&no_at).is_err());

        let no_every = CronSchedule {
            kind: ScheduleKind::Every,
            at_ms: None,
            every_ms: None,
            expr: None,
            tz: None,
        };
        assert!(validate_schedule(&no_every).is_err());

        let no_expr = CronSchedule {
            kind: ScheduleKind::Cron,
            at_ms: None,
            every_ms: None,
            expr: None,
            tz: None,
        };
        assert!(validate_schedule(&no_expr).is_err());
    }

    #[test]
    fn test_add_job_invalid_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let bad_schedule = CronSchedule {
            kind: ScheduleKind::Cron,
            at_ms: None,
            every_ms: None,
            expr: Some("invalid cron expr!!!".to_string()),
            tz: None,
        };
        assert!(svc.add_job(bad_schedule, make_payload(), None).is_err());
    }

    // === Upcoming inspection ===

    #[test]
    fn test_upcoming_from_expression() {
        let times = upcoming_from_expression("0 0 * * * * *", 3).unwrap();
        assert_eq!(times.len(), 3);
        // Should be ordered
        assert!(times[0] < times[1]);
        assert!(times[1] < times[2]);
    }

    #[test]
    fn test_upcoming_from_expression_invalid() {
        assert!(upcoming_from_expression("bad expr", 1).is_err());
    }

    // === History trimming ===

    #[test]
    fn test_history_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Create an "every" schedule that fires every tick
        let schedule = CronSchedule {
            kind: ScheduleKind::Every,
            at_ms: None,
            every_ms: Some(1), // 1ms
            expr: None,
            tz: None,
        };
        svc.add_job(schedule, make_payload(), None).unwrap();

        // Fire many ticks
        for _ in 0..25 {
            // Set next_run to past
            {
                let mut store = svc.store.lock();
                store.jobs[0].next_run = Some(Local::now() - TimeDelta::seconds(10));
            }
            svc.tick();
        }

        let jobs = svc.list_jobs();
        assert!(jobs[0].history.len() <= MAX_CRON_RUN_HISTORY);
    }

    // === System job protection ===

    #[test]
    fn test_system_job_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Manually insert a system job
        let job_id = {
            let mut store = svc.store.lock();
            let id = Uuid::new_v4().to_string();
            store.jobs.push(CronJob {
                id: id.clone(),
                name: Some("system".to_string()),
                schedule: make_every_schedule(),
                payload: make_payload(),
                state: JobState::Active,
                next_run: None,
                last_run: None,
                history: Vec::new(),
                is_system: true,
                priority: 0,
            });
            id
        };
        svc.persist().unwrap();

        let removed = svc.remove_job(&job_id).unwrap();
        assert!(!removed);
        assert_eq!(svc.list_jobs().len(), 1);
    }

    // === Multiple jobs tick ===

    #[test]
    fn test_tick_multiple_due() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Add 3 past "at" jobs
        let mut ids = Vec::new();
        for _ in 0..3 {
            let job = svc
                .add_job(make_at_schedule(true), make_payload(), None)
                .unwrap();
            ids.push(job.id);
        }

        let due = svc.tick();
        assert_eq!(due.len(), 3);

        // All should be Done
        let jobs = svc.list_jobs();
        for job in &jobs {
            assert_eq!(job.state, JobState::Done);
        }
    }

    // === State Store Integration ===

    #[test]
    fn test_add_job_saves_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), Some("stateful".to_string()))
            .unwrap();

        let state = svc.get_job_state(&job.id).unwrap();
        assert_eq!(state.job_name.as_deref(), Some("stateful"));
        assert!(state.is_active);
        assert!(state.next_run.is_some());
        assert_eq!(state.run_count, 0);
    }

    #[test]
    fn test_tick_updates_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_at_schedule(true), make_payload(), None)
            .unwrap();

        svc.tick();

        let state = svc.get_job_state(&job.id).unwrap();
        assert!(!state.is_active); // Done
        assert!(state.last_run.is_some());
        assert_eq!(state.run_count, 1);
    }

    #[test]
    fn test_pause_updates_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        svc.pause_job(&job.id).unwrap();

        let state = svc.get_job_state(&job.id).unwrap();
        assert!(!state.is_active);
    }

    #[test]
    fn test_resume_updates_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        svc.pause_job(&job.id).unwrap();
        svc.resume_job(&job.id).unwrap();

        let state = svc.get_job_state(&job.id).unwrap();
        assert!(state.is_active);
        assert!(state.next_run.is_some());
    }

    #[test]
    fn test_remove_job_deletes_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        // State should exist
        assert!(svc.get_job_state(&job.id).is_some());

        svc.remove_job(&job.id).unwrap();

        // State should be gone
        assert!(svc.get_job_state(&job.id).is_none());
    }

    #[test]
    fn test_mark_completed_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc
            .add_job(make_at_schedule(true), make_payload(), None)
            .unwrap();

        svc.tick();
        svc.mark_completed(&job.id, Some("error: timeout".to_string()));

        let state = svc.get_job_state(&job.id).unwrap();
        assert!(state.last_error.is_some());
        assert!(state.last_error.unwrap().contains("timeout"));
    }

    #[test]
    fn test_list_job_states() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(make_every_schedule(), make_payload(), Some("a".to_string()))
            .unwrap();
        svc.add_job(make_every_schedule(), make_payload(), Some("b".to_string()))
            .unwrap();

        let states = svc.list_job_states();
        assert_eq!(states.len(), 2);
        let names: Vec<_> = states.iter().map(|(j, _)| j.name.clone()).collect();
        assert!(names.contains(&Some("a".to_string())));
        assert!(names.contains(&Some("b".to_string())));
    }

    #[test]
    fn test_state_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let job_id = {
            let svc = make_service(dir.path());
            let job = svc
                .add_job(make_every_schedule(), make_payload(), Some("survive".to_string()))
                .unwrap();

            // Simulate some runs
            {
                let mut store = svc.store.lock();
                store.jobs[0].next_run = Some(Local::now() - TimeDelta::seconds(10));
            }
            svc.tick();

            job.id
        };

        // Create a new service — should recover state
        let svc2 = make_service(dir.path());
        let state = svc2.get_job_state(&job_id).unwrap();
        assert_eq!(state.job_name.as_deref(), Some("survive"));
        assert!(state.run_count >= 1);
    }

    #[test]
    fn test_catch_up_missed_jobs() {
        let dir = tempfile::tempdir().unwrap();

        // Create a job with a next_run in the past
        let job_id = {
            let svc = make_service(dir.path());
            let job = svc
                .add_job(make_every_schedule(), make_payload(), Some("late".to_string()))
                .unwrap();

            // Manually set next_run to the past and persist
            {
                let mut store = svc.store.lock();
                store.jobs[0].next_run = Some(Local::now() - TimeDelta::hours(2));
            }
            svc.persist().unwrap();

            // Also save the state as if it had a past next_run
            let state = CronJobState {
                job_name: Some("late".to_string()),
                last_run: Some(chrono::Utc::now() - chrono::Duration::hours(3)),
                next_run: Some(chrono::Utc::now() - chrono::Duration::hours(2)),
                is_active: true,
                run_count: 5,
                last_error: None,
            };
            svc.state_store.save_state(&job.id, &state).unwrap();

            job.id
        };

        // Create a new service — should catch up
        let svc2 = make_service(dir.path());
        let job = svc2.get_job(&job_id).unwrap();
        // Should have rescheduled to a future time
        assert!(job.next_run.unwrap() > Local::now());
        assert_eq!(job.state, JobState::Active);
    }

    #[test]
    fn test_with_state_store_custom() {
        let dir = tempfile::tempdir().unwrap();
        let mem_store = Box::new(crate::state_store::MemoryStateStore::new());
        let svc = CronService::with_state_store(dir.path().to_path_buf(), mem_store).unwrap();

        let job = svc
            .add_job(make_every_schedule(), make_payload(), Some("custom".to_string()))
            .unwrap();

        let state = svc.get_job_state(&job.id).unwrap();
        assert_eq!(state.job_name.as_deref(), Some("custom"));
    }

    // === Priority tests ===

    #[test]
    fn test_add_job_with_priority() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        let job = svc
            .add_job_with_priority(
                make_every_schedule(),
                make_payload(),
                Some("high-pri".to_string()),
                100,
            )
            .unwrap();

        assert_eq!(job.priority, 100);

        // Verify persisted
        let loaded = svc.get_job(&job.id).unwrap();
        assert_eq!(loaded.priority, 100);
    }

    #[test]
    fn test_add_job_default_priority() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();

        assert_eq!(job.priority, 0);
    }

    #[test]
    fn test_update_job_priority() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        let job = svc
            .add_job(make_every_schedule(), make_payload(), None)
            .unwrap();
        assert_eq!(job.priority, 0);

        let updated = svc
            .update_job(&job.id, None, None, None, Some(50))
            .unwrap()
            .unwrap();
        assert_eq!(updated.priority, 50);

        // Verify persisted
        let loaded = svc.get_job(&job.id).unwrap();
        assert_eq!(loaded.priority, 50);
    }

    #[test]
    fn test_tick_sorts_by_priority() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        // Create 3 jobs due now (past "at" schedule)
        let past_ts = (Local::now() - chrono::Duration::seconds(10))
            .timestamp_millis();

        let mut job_ids = Vec::new();
        for (i, pri) in [0u32, 200, 50].iter().enumerate() {
            let schedule = CronSchedule {
                kind: ScheduleKind::At,
                at_ms: Some(past_ts),
                every_ms: None,
                expr: None,
                tz: None,
            };
            let payload = CronPayload {
                message: format!("job-{}", i),
                channel: None,
                chat_id: None,
                deliver: false,
            };
            let job = svc
                .add_job_with_priority(schedule, payload, None, *pri)
                .unwrap();
            job_ids.push(job.id);
        }

        let due = svc.tick();

        // Should be sorted by priority descending: 200, 50, 0
        assert_eq!(due.len(), 3);
        assert_eq!(due[0].priority, 200);
        assert_eq!(due[1].priority, 50);
        assert_eq!(due[2].priority, 0);
    }

    #[test]
    fn test_priority_persists_across_restart() {
        let dir = tempfile::tempdir().unwrap();

        {
            let svc = make_service(dir.path());
            svc.add_job_with_priority(
                make_every_schedule(),
                make_payload(),
                Some("urgent".to_string()),
                999,
            )
            .unwrap();
        }

        // Reload
        let svc2 = make_service(dir.path());
        let jobs = svc2.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].priority, 999);
        assert_eq!(jobs[0].name.as_deref(), Some("urgent"));
    }
}
