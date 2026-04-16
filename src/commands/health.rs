//! Health command — check and display heartbeat service status.

use anyhow::Result;
use kestrel_config::Config;

/// Show current health status.
pub fn check(_config: &Config) -> Result<()> {
    let home = kestrel_config::paths::get_kestrel_home()?;
    let state_path = home.join("heartbeat_state.json");

    if !state_path.exists() {
        println!("No heartbeat state found. The heartbeat service may not have run yet.");
        return Ok(());
    }

    let content = std::fs::read_to_string(&state_path)?;
    let state: kestrel_heartbeat::HeartbeatState = serde_json::from_str(&content)?;

    println!("=== Health Status ===\n");
    println!("Total checks:    {}", state.total_checks);
    println!("Total failures:  {}", state.total_failures);
    println!("Restarts:        {}", state.restarts_requested);

    if let Some(started) = &state.started_at {
        println!("Started at:      {}", started.format("%Y-%m-%d %H:%M:%S"));
    }
    if let Some(stopped) = &state.stopped_at {
        println!("Stopped at:      {}", stopped.format("%Y-%m-%d %H:%M:%S"));
    }

    if let Some(snapshot) = &state.last_snapshot {
        println!();
        println!(
            "Last snapshot ({}): {}",
            snapshot.timestamp.format("%Y-%m-%d %H:%M:%S"),
            if snapshot.healthy {
                "HEALTHY"
            } else {
                "UNHEALTHY"
            }
        );

        if !snapshot.checks.is_empty() {
            println!();
            println!("{:<20} {:<12} {:30}", "Component", "Status", "Message");
            println!("{}", "-".repeat(60));
            for check in &snapshot.checks {
                let status = match check.status {
                    kestrel_heartbeat::CheckStatus::Healthy => "healthy",
                    kestrel_heartbeat::CheckStatus::Degraded => "DEGRADED",
                    kestrel_heartbeat::CheckStatus::Unhealthy => "UNHEALTHY",
                    kestrel_heartbeat::CheckStatus::Skipped => "skipped",
                };
                println!("{:<20} {:<12} {}", check.component, status, check.message);
            }
        }
    }

    if !state.component_failures.is_empty() {
        println!();
        println!("Component failure tracking:");
        println!(
            "{:<20} {:<10} {:<10} {:<12} {:<10}",
            "Component", "Consec.", "Total", "Restart?", "Backoff"
        );
        println!("{}", "-".repeat(65));
        for f in &state.component_failures {
            println!(
                "{:<20} {:<10} {:<10} {:<12} {}s",
                f.component,
                f.consecutive_failures,
                f.total_failures,
                if f.restart_pending { "pending" } else { "no" },
                f.backoff_secs,
            );
        }
    }

    Ok(())
}
