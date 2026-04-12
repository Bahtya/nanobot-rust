# nanobot-cron

Cron scheduler with tick-based scheduling and JSON file persistence.

Part of the [nanobot-rust](../..) workspace.

## Overview

Manages scheduled jobs that fire messages into the agent at configured times. Supports
three schedule types: one-shot (`at`), recurring interval (`every`), and cron
expressions. Jobs are persisted to a `jobs.json` file so they survive restarts.

## Key Types

| Type | Description |
|---|---|
| `CronService` | Main service: add/remove jobs, tick to find due jobs, persist to disk |
| `CronSchedule` | Schedule definition with kind, timestamp/interval/expression, timezone |
| `ScheduleKind` | Enum: `At` (one-shot), `Every` (interval), `Cron` (expression) |
| `CronJob` | Complete job: id, name, schedule, payload, state, next_run, history |
| `CronPayload` | What to execute: message text, optional channel/chat_id, deliver flag |
| `JobState` | Active / Paused / Done |
| `CronRunRecord` | Timestamp, result, and success flag for a single execution |
| `CronStore` | Persistent container holding all `CronJob` entries |

## CronService API

- `new(cron_dir)` -- Create/load from the `jobs.json` in the given directory
- `add_job(schedule, payload, name)` -> `CronJob`
- `remove_job(id)` -- Delete a job (system jobs are protected)
- `tick()` -> `Vec<CronJob>` -- Return due jobs and advance their schedules
- `mark_completed(id, result)` -- Record execution result
- `list_jobs()` -- Return all jobs

## Usage

```rust
use nanobot_cron::{CronService, CronSchedule, CronPayload, ScheduleKind};
use std::path::PathBuf;

let svc = CronService::new(PathBuf::from("./data/cron"))?;

// One-shot job
let schedule = CronSchedule {
    kind: ScheduleKind::At,
    at_ms: Some(future_timestamp_ms),
    every_ms: None, expr: None, tz: None,
};
let job = svc.add_job(schedule, payload, Some("remind me".into()));

// Recurring every 60 seconds
let every = CronSchedule {
    kind: ScheduleKind::Every,
    every_ms: Some(60_000),
    ..Default::default()
};
svc.add_job(every, payload, None);

// Call tick periodically
for job in svc.tick() {
    svc.mark_completed(&job.id, Some("done".into()));
}
```
