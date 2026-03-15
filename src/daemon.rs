use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use crate::{display, git, github, load_config, filter_workspaces, sync, watch, watch_cache};

/// Options for the daemon command.
pub struct DaemonOpts {
    pub config: Option<PathBuf>,
    pub workspace: Option<String>,
    pub interval: u64,
    pub fetch: bool,
    pub quiet: bool,
}

/// Run the daemon loop: sync + fetch + watch on interval, re-reading config each cycle.
///
/// Workspaces are processed in parallel using tokio tasks.
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

        // Process all workspaces in parallel
        let mut tasks = tokio::task::JoinSet::new();
        for ws in workspaces {
            let ws = ws.clone();
            let fetch = opts.fetch;
            let quiet = opts.quiet;
            tasks.spawn(async move {
                let name = ws.name.clone();
                match run_workspace_cycle(&ws, fetch, quiet).await {
                    Ok(()) => {}
                    Err(e) => {
                        display::print_daemon_error(&name, &e);
                    }
                }
            });
        }

        // Await all workspace tasks
        while let Some(result) = tasks.join_next().await {
            if let Err(e) = result {
                eprintln!("daemon: workspace task panicked: {e}");
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
    let repos = sync::resolve_repos(ws, false).await?;
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

    // Watch: detect new versions if enabled
    if let Some(ref watch_cfg) = ws.watch {
        if watch_cfg.enable {
            let gh = github::HttpGitHubClient::new()?;
            let cache_store = watch_cache::FsWatchStateStore;
            let matrix_appender = watch::TomlMatrixAppender;
            let git_ops = git::SystemGitOps;

            match watch::run_watch_cycle(ws, quiet, &gh, &cache_store, &matrix_appender, &git_ops).await {
                Ok(summary) => {
                    if !quiet {
                        display::print_watch_summary(&ws.name, &summary);
                    }
                }
                Err(e) => {
                    display::print_daemon_error(&ws.name, &e);
                }
            }
        }
    }

    Ok(())
}
