use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use crate::{display, load_config, filter_workspaces, sync};

/// Options for the daemon command.
pub struct DaemonOpts {
    pub config: Option<PathBuf>,
    pub workspace: Option<String>,
    pub interval: u64,
    pub fetch: bool,
    pub quiet: bool,
}

/// Run the daemon loop: sync + fetch on interval, re-reading config each cycle.
pub async fn run(opts: DaemonOpts) -> Result<()> {
    let mut cycle = 0u64;

    loop {
        cycle += 1;

        // Re-read config each cycle so nix rebuild changes are picked up
        let cfg = match load_config(opts.config.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("daemon: failed to load config: {e}");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(opts.interval)) => continue,
                    _ = tokio::signal::ctrl_c() => break,
                }
            }
        };

        let workspaces = filter_workspaces(&cfg.workspaces, opts.workspace.as_deref());
        let ws_count = workspaces.len();

        if !opts.quiet {
            display::print_daemon_cycle_start(cycle);
        }

        for ws in &workspaces {
            match run_workspace_cycle(ws, opts.fetch, opts.quiet).await {
                Ok(()) => {}
                Err(e) => {
                    display::print_daemon_error(&ws.name, &e);
                }
            }
        }

        if !opts.quiet {
            display::print_daemon_cycle_done(cycle, ws_count);
            display::print_daemon_sleeping(opts.interval);
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(opts.interval)) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    Ok(())
}

async fn run_workspace_cycle(
    ws: &crate::config::Workspace,
    fetch: bool,
    quiet: bool,
) -> Result<()> {
    let repos = sync::resolve_repos(ws).await?;
    let (cloned, present) = sync::sync_repos(ws, &repos, quiet).await?;

    if !quiet || cloned > 0 {
        display::print_sync_summary(&ws.name, cloned, present);
    }

    if fetch {
        let (fetched, skipped) = sync::fetch_repos(ws, &repos, quiet).await?;
        if !quiet {
            display::print_fetch_summary(&ws.name, fetched, skipped);
        }
    }

    Ok(())
}
