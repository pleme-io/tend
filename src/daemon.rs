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
            let audit = crate::audit::AuditLog::default_path();

            match watch::run_watch_cycle(ws, quiet, &gh, &cache_store, &matrix_appender, &git_ops, &audit).await {
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

        // Nix audit: run convergence loop if enabled
        if let Some(ref audit_cfg) = watch_cfg.nix_audit {
            if audit_cfg.enable {
                match run_nix_audit_cycle(ws, audit_cfg, quiet).await {
                    Ok(()) => {}
                    Err(e) => {
                        display::print_daemon_error(&ws.name, &e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Run the nix-audit convergence cycle for a workspace.
///
/// Steps:
/// 1. Run `nix-audit check --all --format json` to observe violations
/// 2. If auto_fix: run `nix-audit fix --all --commit` to correct violations
/// 3. If auto_propagate: run `tend flake-update` to propagate fixes
/// 4. Run post-hooks with triggers: after_audit, on_violation, on_convergence
async fn run_nix_audit_cycle(
    ws: &crate::config::Workspace,
    cfg: &crate::config::NixAuditConfig,
    quiet: bool,
) -> Result<()> {
    let base_dir = ws.resolved_base_dir()?;
    let audit = crate::audit::AuditLog::default_path();

    // Step 1: Observe — run nix-audit check
    let check_output = tokio::process::Command::new("nix-audit")
        .args(["check", "--all", "--format", "json"])
        .arg(base_dir.to_str().unwrap_or("."))
        .output()
        .await;

    let check_result = match check_output {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Parse JSON output lines to count findings
            let mut total_repos = 0usize;
            let mut passing_repos = 0usize;
            let mut total_findings = 0usize;
            for line in stdout.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    total_repos += 1;
                    let findings = v.get("findings")
                        .and_then(|f| f.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    total_findings += findings;
                    if findings == 0 {
                        passing_repos += 1;
                    }
                }
            }
            audit.nix_audit_completed(total_repos, passing_repos, total_findings);
            if !quiet {
                eprintln!(
                    "  nix-audit: {passing_repos}/{total_repos} repos passing, {total_findings} findings"
                );
            }
            (total_repos, passing_repos, total_findings)
        }
        Err(e) => {
            if !quiet {
                eprintln!("  nix-audit: check failed: {e}");
            }
            return Ok(());
        }
    };

    let (total_repos, passing_repos, total_findings) = check_result;

    // Run after_audit hooks
    for hook in &cfg.post_hooks {
        if hook.trigger == "after_audit" {
            run_hook(hook, &audit).await;
        }
    }

    // Run on_violation hooks if there are findings
    if total_findings > 0 {
        for hook in &cfg.post_hooks {
            if hook.trigger == "on_violation" {
                run_hook(hook, &audit).await;
            }
        }
    }

    // Step 2: Fix — auto-repair if configured
    if cfg.auto_fix && total_findings > 0 {
        let fix_output = tokio::process::Command::new("nix-audit")
            .args(["fix", "--all", "--commit"])
            .arg(base_dir.to_str().unwrap_or("."))
            .output()
            .await;

        if let Ok(output) = fix_output {
            if !quiet && !output.status.success() {
                eprintln!("  nix-audit: fix returned non-zero");
            }
        }
    }

    // Step 3: Propagate — flake-update if configured
    if cfg.auto_propagate && cfg.auto_fix && total_findings > 0 {
        let _ = tokio::process::Command::new("tend")
            .args(["flake-update"])
            .output()
            .await;
    }

    // Check for convergence
    if total_repos > 0 && passing_repos == total_repos {
        let ratio = passing_repos as f64 / total_repos as f64;
        audit.convergence_achieved(ratio);
        for hook in &cfg.post_hooks {
            if hook.trigger == "on_convergence" {
                run_hook(hook, &audit).await;
            }
        }
    }

    Ok(())
}

/// Execute a single post-hook, logging the result.
async fn run_hook(hook: &crate::config::PostHook, audit: &crate::audit::AuditLog) {
    let start = std::time::Instant::now();
    let result = tokio::process::Command::new(&hook.command)
        .args(&hook.args)
        .output()
        .await;

    let (exit_code, duration_ms) = match result {
        Ok(output) => (output.status.code().unwrap_or(-1), start.elapsed().as_millis() as u64),
        Err(_) => (-1, start.elapsed().as_millis() as u64),
    };

    audit.hook_executed(&hook.trigger, &hook.command, exit_code, duration_ms);
}
