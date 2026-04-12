//! Cron service — manages scheduled job lifecycle.
//!
//! Handles tick-based scheduling, job persistence, and execution dispatch.
//! Mirrors the Python `cron/service.py` CronService.

use crate::types::*;
use anyhow::Result;
use chrono::Local;
use nanobot_core::MAX_CRON_RUN_HISTORY;
use std::path::PathBuf;
use tracing::info;
use uuid::Uuid;

/// The cron scheduler service.
pub struct CronService {
    store_path: PathBuf,
    store: parking_lot::Mutex<CronStore>,
}

impl CronService {
    /// Create a new CronService with the given storage directory.
    pub fn new(cron_dir: PathBuf) -> Result<Self> {
        if !cron_dir.exists() {
            std::fs::create_dir_all(&cron_dir)?;
        }

        let store_path = cron_dir.join("jobs.json");
        let store = if store_path.exists() {
            let content = std::fs::read_to_string(&store_path)?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            CronStore::default()
        };

        Ok(Self {
            store_path,
            store: parking_lot::Mutex::new(store),
        })
    }

    /// Add a new cron job.
    pub fn add_job(
        &self,
        schedule: CronSchedule,
        payload: CronPayload,
        name: Option<String>,
    ) -> CronJob {
        let id = Uuid::new_v4().to_string();
        let next_run = compute_next_run(&schedule);

        let job = CronJob {
            id,
            name,
            schedule,
            payload,
            state: JobState::Active,
            next_run,
            last_run: None,
            history: Vec::new(),
            is_system: false,
        };

        self.store.lock().jobs.push(job.clone());
        let _ = self.persist();
        info!("Added cron job: {}", job.id);
        job
    }

    /// Remove a cron job by ID.
    pub fn remove_job(&self, job_id: &str) -> Result<bool> {
        let mut store = self.store.lock();
        let before = store.jobs.len();
        store.jobs.retain(|j| j.id != job_id || j.is_system);
        let removed = store.jobs.len() < before;
        drop(store);

        if removed {
            self.persist()?;
            info!("Removed cron job: {}", job_id);
        }
        Ok(removed)
    }

    /// Get all jobs.
    pub fn list_jobs(&self) -> Vec<CronJob> {
        self.store.lock().jobs.clone()
    }

    /// Tick — check for due jobs and return them.
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

                    // Compute next run
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
        let _ = self.persist();
        due
    }

    /// Mark a job as completed with a result.
    pub fn mark_completed(&self, job_id: &str, result: Option<String>) {
        let mut store = self.store.lock();
        if let Some(job) = store.jobs.iter_mut().find(|j| j.id == job_id) {
            if let Some(last_run) = job.history.last_mut() {
                last_run.result = result;
            }
        }
        drop(store);
        let _ = self.persist();
    }

    /// Persist the store to disk.
    fn persist(&self) -> Result<()> {
        let store = self.store.lock();
        let json = serde_json::to_string_pretty(&*store)?;
        std::fs::write(&self.store_path, json)?;
        Ok(())
    }
}

/// Compute the next run time for a schedule.
fn compute_next_run(schedule: &CronSchedule) -> Option<chrono::DateTime<Local>> {
    let now = Local::now();
    match schedule.kind {
        ScheduleKind::At => schedule.at_ms.and_then(|ms| {
            chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.with_timezone(&Local))
        }),
        ScheduleKind::Every => schedule
            .every_ms
            .map(|ms| now + chrono::Duration::milliseconds(ms)),
        ScheduleKind::Cron => {
            // For cron expressions, use a simplified approach
            // A full cron parser would use the `cron` crate
            schedule.expr.as_ref().map(|_| {
                // Default: next tick + 1 minute
                now + chrono::Duration::minutes(1)
            })
        }
    }
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
            // 1 hour in the past
            (Local::now() - TimeDelta::hours(1)).timestamp_millis()
        } else {
            // 1 hour in the future
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

    fn make_cron_schedule() -> CronSchedule {
        CronSchedule {
            kind: ScheduleKind::Cron,
            at_ms: None,
            every_ms: None,
            expr: Some("0 * * * *".to_string()),
            tz: None,
        }
    }

    fn make_payload() -> CronPayload {
        CronPayload {
            message: "test".to_string(),
            channel: None,
            chat_id: None,
            deliver: false,
        }
    }

    #[test]
    fn test_cron_service_add_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc.add_job(
            make_at_schedule(false),
            make_payload(),
            Some("test".to_string()),
        );
        assert_eq!(job.name.as_deref(), Some("test"));
        assert_eq!(job.state, JobState::Active);
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job.id);
    }

    #[test]
    fn test_cron_service_remove_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc.add_job(make_at_schedule(false), make_payload(), None);
        assert_eq!(svc.list_jobs().len(), 1);
        let removed = svc.remove_job(&job.id).unwrap();
        assert!(removed);
        assert!(svc.list_jobs().is_empty());
    }

    #[test]
    fn test_cron_service_list_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(
            make_at_schedule(false),
            make_payload(),
            Some("a".to_string()),
        );
        svc.add_job(make_every_schedule(), make_payload(), Some("b".to_string()));
        svc.add_job(make_cron_schedule(), make_payload(), Some("c".to_string()));
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 3);
    }

    #[test]
    fn test_cron_service_tick_no_due() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.add_job(make_at_schedule(false), make_payload(), None);
        let due = svc.tick();
        assert!(due.is_empty());
    }

    #[test]
    fn test_cron_service_tick_due_job() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let job = svc.add_job(make_at_schedule(true), make_payload(), None);
        let due = svc.tick();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, job.id);
        // The "at" job should now be Done
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].state, JobState::Done);
    }

    #[test]
    fn test_cron_service_persist() {
        let dir = tempfile::tempdir().unwrap();
        let job_id = {
            let svc = make_service(dir.path());
            let job = svc.add_job(
                make_at_schedule(false),
                make_payload(),
                Some("persist".to_string()),
            );
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
    fn test_compute_next_run_every() {
        let schedule = make_every_schedule();
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        let next = next.unwrap();
        assert!(next > Local::now());
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
        let next = next.unwrap();
        assert!(next > Local::now());
    }

    #[test]
    fn test_compute_next_run_cron() {
        let schedule = make_cron_schedule();
        let next = compute_next_run(&schedule);
        assert!(next.is_some());
        let next = next.unwrap();
        assert!(next > Local::now());
    }
}
