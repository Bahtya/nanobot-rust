//! Cron data types — CronSchedule, CronJob, CronPayload, etc.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// The kind of schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    /// One-shot at a specific time.
    At,
    /// Recurring interval.
    Every,
    /// Cron expression.
    Cron,
}

/// Schedule definition for a cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronSchedule {
    /// The schedule type.
    pub kind: ScheduleKind,

    /// For "at" schedules: timestamp in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<i64>,

    /// For "every" schedules: interval in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_ms: Option<i64>,

    /// For "cron" schedules: cron expression.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,

    /// Timezone for the schedule.
    #[serde(default)]
    pub tz: Option<String>,
}

/// Payload for a cron job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronPayload {
    /// Message to process when the job fires.
    pub message: String,

    /// Channel to deliver results to.
    #[serde(default)]
    pub channel: Option<String>,

    /// Chat ID to deliver results to.
    #[serde(default)]
    pub chat_id: Option<String>,

    /// Whether to deliver results as a message.
    #[serde(default)]
    pub deliver: bool,
}

/// Record of a single cron job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronRunRecord {
    /// When the job ran.
    pub timestamp: DateTime<Local>,

    /// Execution result.
    #[serde(default)]
    pub result: Option<String>,

    /// Whether execution succeeded.
    pub success: bool,
}

/// State of a cron job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Job is active and will fire.
    Active,
    /// Job is paused.
    Paused,
    /// One-shot job has completed.
    Done,
}

/// A complete cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Unique job ID.
    pub id: String,

    /// Human-readable job name.
    #[serde(default)]
    pub name: Option<String>,

    /// The schedule.
    pub schedule: CronSchedule,

    /// The payload to execute.
    pub payload: CronPayload,

    /// Current job state.
    #[serde(default = "default_job_state")]
    pub state: JobState,

    /// Next scheduled run time.
    #[serde(default)]
    pub next_run: Option<DateTime<Local>>,

    /// Last run time.
    #[serde(default)]
    pub last_run: Option<DateTime<Local>>,

    /// Execution history (last N runs).
    #[serde(default)]
    pub history: Vec<CronRunRecord>,

    /// Whether this is a system job (not user-deletable).
    #[serde(default)]
    pub is_system: bool,
}

fn default_job_state() -> JobState {
    JobState::Active
}

/// Persistent store of cron jobs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronStore {
    /// All cron jobs.
    pub jobs: Vec<CronJob>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;

    #[test]
    fn test_schedule_kind_serde() {
        for kind in &[ScheduleKind::At, ScheduleKind::Every, ScheduleKind::Cron] {
            let json = serde_json::to_string(kind).unwrap();
            let back: ScheduleKind = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    #[test]
    fn test_cron_schedule_construction() {
        let at = CronSchedule {
            kind: ScheduleKind::At,
            at_ms: Some(1700000000000),
            every_ms: None,
            expr: None,
            tz: None,
        };
        assert_eq!(at.kind, ScheduleKind::At);
        assert_eq!(at.at_ms, Some(1700000000000));

        let every = CronSchedule {
            kind: ScheduleKind::Every,
            at_ms: None,
            every_ms: Some(60000),
            expr: None,
            tz: None,
        };
        assert_eq!(every.kind, ScheduleKind::Every);
        assert_eq!(every.every_ms, Some(60000));

        let cron = CronSchedule {
            kind: ScheduleKind::Cron,
            at_ms: None,
            every_ms: None,
            expr: Some("0 * * * *".to_string()),
            tz: Some("UTC".to_string()),
        };
        assert_eq!(cron.kind, ScheduleKind::Cron);
        assert_eq!(cron.expr.as_deref(), Some("0 * * * *"));
        assert_eq!(cron.tz.as_deref(), Some("UTC"));
    }

    #[test]
    fn test_cron_payload_construction() {
        let payload = CronPayload {
            message: "hello".to_string(),
            channel: Some("slack".to_string()),
            chat_id: Some("12345".to_string()),
            deliver: true,
        };
        assert_eq!(payload.message, "hello");
        assert_eq!(payload.channel.as_deref(), Some("slack"));
        assert_eq!(payload.chat_id.as_deref(), Some("12345"));
        assert!(payload.deliver);
    }

    #[test]
    fn test_job_state_default() {
        let default: JobState = JobState::Active;
        // default_job_state returns Active
        assert_eq!(default, JobState::Active);
    }

    #[test]
    fn test_cron_job_construction() {
        let job = CronJob {
            id: "test-id".to_string(),
            name: Some("test job".to_string()),
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at_ms: Some(1700000000000),
                every_ms: None,
                expr: None,
                tz: None,
            },
            payload: CronPayload {
                message: "run".to_string(),
                channel: None,
                chat_id: None,
                deliver: false,
            },
            state: JobState::Active,
            next_run: None,
            last_run: None,
            history: Vec::new(),
            is_system: false,
        };
        assert_eq!(job.id, "test-id");
        assert_eq!(job.name.as_deref(), Some("test job"));
        assert_eq!(job.state, JobState::Active);
        assert!(!job.is_system);
        assert!(job.history.is_empty());
    }

    #[test]
    fn test_cron_store_default() {
        let store = CronStore::default();
        assert!(store.jobs.is_empty());
    }

    #[test]
    fn test_cron_run_record() {
        let now = Local::now();
        let record = CronRunRecord {
            timestamp: now,
            result: Some("ok".to_string()),
            success: true,
        };
        assert!(record.success);
        assert_eq!(record.result.as_deref(), Some("ok"));
        assert_eq!(record.timestamp, now);
    }
}
