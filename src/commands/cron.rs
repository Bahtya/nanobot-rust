//! Cron command — list and inspect cron jobs.

use anyhow::Result;
use kestrel_config::Config;

/// List all cron jobs.
pub fn list(_config: &Config) -> Result<()> {
    let home = kestrel_config::paths::get_kestrel_home()?;
    let cron_dir = home.join("cron");

    if !cron_dir.exists() {
        println!("No cron jobs found (directory does not exist).");
        return Ok(());
    }

    let svc = kestrel_cron::CronService::new(cron_dir)?;
    let states = svc.list_job_states();

    if states.is_empty() {
        println!("No cron jobs.");
        return Ok(());
    }

    println!("Cron Jobs ({}):\n", states.len());
    println!(
        "{:<38} {:<20} {:<10} {:<8} {:<20}",
        "ID", "Name", "State", "Runs", "Next Run"
    );
    println!("{}", "-".repeat(100));

    for (job, state) in &states {
        let name = job.name.as_deref().unwrap_or("(unnamed)");
        let job_state = match job.state {
            kestrel_cron::JobState::Active => "active",
            kestrel_cron::JobState::Paused => "paused",
            kestrel_cron::JobState::Done => "done",
        };
        let next = state
            .next_run
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());

        println!(
            "{:<38} {:<20} {:<10} {:<8} {:<20}",
            &job.id[..job.id.len().min(36)],
            name,
            job_state,
            state.run_count,
            next,
        );
    }

    Ok(())
}

/// Show detailed status for a specific cron job.
pub fn status(_config: &Config, name: &str) -> Result<()> {
    let home = kestrel_config::paths::get_kestrel_home()?;
    let cron_dir = home.join("cron");

    if !cron_dir.exists() {
        println!("No cron jobs found.");
        return Ok(());
    }

    let svc = kestrel_cron::CronService::new(cron_dir)?;
    let states = svc.list_job_states();

    // Find by name or ID prefix
    let found = states.iter().find(|(job, _)| {
        job.name.as_deref() == Some(name) || job.id == name || job.id.starts_with(name)
    });

    match found {
        Some((job, state)) => {
            println!("=== Cron Job Status ===\n");
            println!("ID:        {}", job.id);
            println!("Name:      {}", job.name.as_deref().unwrap_or("(none)"));
            println!(
                "State:     {}",
                match job.state {
                    kestrel_cron::JobState::Active => "active",
                    kestrel_cron::JobState::Paused => "paused",
                    kestrel_cron::JobState::Done => "done",
                }
            );
            println!("System:    {}", if job.is_system { "yes" } else { "no" });
            println!();

            println!("Schedule:");
            match job.schedule.kind {
                kestrel_cron::ScheduleKind::At => {
                    let ts = job
                        .schedule
                        .at_ms
                        .and_then(chrono::DateTime::from_timestamp_millis)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                        .unwrap_or_else(|| "invalid".to_string());
                    println!("  Type: one-shot at {}", ts);
                }
                kestrel_cron::ScheduleKind::Every => {
                    let ms = job.schedule.every_ms.unwrap_or(0);
                    let secs = ms / 1000;
                    println!("  Type: every {}s", secs);
                }
                kestrel_cron::ScheduleKind::Cron => {
                    println!(
                        "  Type: cron ({})",
                        job.schedule.expr.as_deref().unwrap_or("?")
                    );
                }
            }
            println!();

            println!("Runtime:");
            println!("  Run count:  {}", state.run_count);
            println!(
                "  Last run:   {}",
                state
                    .last_run
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "never".to_string())
            );
            println!(
                "  Next run:   {}",
                state
                    .next_run
                    .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "n/a".to_string())
            );
            if let Some(err) = &state.last_error {
                println!("  Last error: {}", err);
            }
            println!();

            if !job.history.is_empty() {
                println!("Recent history (last {}):", job.history.len().min(5));
                for record in job.history.iter().rev().take(5) {
                    let status = if record.success { "OK" } else { "FAIL" };
                    let result = record.result.as_deref().unwrap_or("-");
                    println!(
                        "  {} [{}] {}",
                        record.timestamp.format("%Y-%m-%d %H:%M:%S"),
                        status,
                        result,
                    );
                }
            }

            println!("\nPayload:");
            println!("  Message: {}", job.payload.message);
            if let Some(ch) = &job.payload.channel {
                println!("  Channel: {}", ch);
            }
            if let Some(cid) = &job.payload.chat_id {
                println!("  Chat ID: {}", cid);
            }
            println!(
                "  Deliver: {}",
                if job.payload.deliver { "yes" } else { "no" }
            );
        }
        None => {
            println!("Cron job '{}' not found.", name);
        }
    }

    Ok(())
}
