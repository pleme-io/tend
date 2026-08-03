use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::kanshou_state::TendDaemonState;
use crate::planner::{ExecutionPlan, WorkItem, WorkKind};
use crate::{
    display, filter_workspaces_checked, git, github, load_config, planner, reconcile, sync, watch,
    watch_cache,
};

/// Options for the daemon command.
pub(crate) struct DaemonOpts {
    pub config: Option<PathBuf>,
    pub workspace: Option<String>,
    pub interval: u64,
    /// Fast-forward clean repos with `git pull --ff-only` each cycle. Implies
    /// fetch (pull does its own fetch). Default true — this is the reconciler
    /// behavior that drives the workspace toward the org's current state.
    pub pull: bool,
    /// Plain `git fetch --all --prune` each cycle. Only takes effect when
    /// `pull` is false (pull already fetches). Kept so a fetch-only daemon
    /// remains expressible (`--no-pull --fetch`).
    pub fetch: bool,
    pub quiet: bool,
    /// Maximum concurrent `git pull` Jobs per workspace per cycle.
    /// Bounds the shigoto-scheduler's per-kind Budget for the
    /// `tend.pull-repo` kind so the daemon doesn't saturate file
    /// handles / SSH multiplexers on large workspaces.
    pub max_inflight: u32,
    /// Path to the GitHub token file (`--github-token-file`). Held so an
    /// external poke (SIGHUP) can re-read it and refresh the process
    /// `GITHUB_TOKEN` cache that `provider`/`sync` consume — a rotated
    /// token then takes effect without restarting the daemon.
    pub github_token_file: Option<PathBuf>,
}

/// Read + trim a token from `path`. Pure (no env mutation) so it is unit
/// testable without racing other tests on the process env.
fn read_token_file(path: &std::path::Path) -> Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading token from {}", path.display()))?;
    Ok(token.trim().to_string())
}

/// Re-read the GitHub token from `path` and refresh the process-global
/// `GITHUB_TOKEN` that `provider::github_token` and `sync` read at call
/// time. This is the body of the external reload poke (SIGHUP) — it lets
/// a rotated credential take effect without a daemon restart.
fn reload_github_token(path: &std::path::Path) -> Result<()> {
    std::env::set_var("GITHUB_TOKEN", read_token_file(path)?);
    Ok(())
}

/// Run the daemon loop: sync + pull + watch on interval, re-reading config each cycle.
///
/// Workspaces are processed in parallel using tokio tasks.
pub(crate) async fn run(opts: DaemonOpts) -> Result<()> {
    run_with_kanshou(opts, Arc::new(TendDaemonState::new())).await
}

/// Variant that accepts a pre-constructed kanshou state. The main()
/// dispatch wraps the kanshou-server bind around this so the state
/// shared with the introspection socket IS the state the loop ticks.
pub(crate) async fn run_with_kanshou(
    opts: DaemonOpts,
    kanshou_state: Arc<TendDaemonState>,
) -> Result<()> {
    let mut cycle = 0u64;

    // Single drain coordinator for the whole loop — handles SIGTERM and
    // SIGINT. `tokio::signal::ctrl_c` alone misses SIGTERM from launchd.
    let shutdown = tsunagu::ShutdownController::install();

    // External reload poke: SIGHUP re-reads the token file and refreshes
    // the `GITHUB_TOKEN` cache, then wakes the loop to reconcile with the
    // fresh credential — so a rotated token (sops edit + rebuild, or a
    // manual rotation) takes effect WITHOUT restarting the daemon. Poke
    // with `kill -HUP <pid>` or
    // `launchctl kill HUP gui/<uid>/io.pleme.tend-daemon`. The reload
    // count + last-reload time are observable via
    // `gen kanshou query tend token`.
    let reload_now = Arc::new(tokio::sync::Notify::new());
    if let Some(token_path) = opts.github_token_file.clone() {
        let notify = Arc::clone(&reload_now);
        let kstate = Arc::clone(&kanshou_state);
        let quiet = opts.quiet;
        tokio::spawn(async move {
            let mut hup =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("daemon: SIGHUP reload unavailable: {e}");
                        return;
                    }
                };
            let audit = crate::audit::AuditLog::default_path();
            while hup.recv().await.is_some() {
                match reload_github_token(&token_path) {
                    Ok(()) => {
                        kstate.token_reloads.fetch_add(1, Ordering::Relaxed);
                        kstate
                            .token_last_reload_unix_ms
                            .store(now_unix_ms(), Ordering::Relaxed);
                        audit.log(
                            "github_token_reloaded",
                            serde_json::json!({
                                "path": token_path.display().to_string(),
                                "trigger": "SIGHUP",
                            }),
                        );
                        if !quiet {
                            eprintln!(
                                "daemon: SIGHUP — reloaded GITHUB_TOKEN from {}; reconciling now",
                                token_path.display()
                            );
                        }
                        notify.notify_one();
                    }
                    Err(e) => {
                        audit.log(
                            "github_token_reload_failed",
                            serde_json::json!({
                                "path": token_path.display().to_string(),
                                "error": e.to_string(),
                            }),
                        );
                        eprintln!("daemon: SIGHUP reload failed: {e}");
                    }
                }
            }
        });
    }

    loop {
        cycle += 1;

        if shutdown.is_triggered() {
            break;
        }

        // Re-read config each cycle so nix rebuild changes are picked up
        let cfg = match load_config(opts.config.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("daemon: failed to load config: {e}");
                let mut tok = shutdown.token();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(opts.interval)) => continue,
                    () = tok.wait_ref() => break,
                }
            }
        };

        let workspaces = filter_workspaces_checked(&cfg.workspaces, opts.workspace.as_deref())?;
        let ws_count = workspaces.len();

        // Build the DAG of configured work BEFORE touching repos or the network.
        // Empty plan → nothing's wired up this cycle → silent sleep.
        let plan = build_plan(&workspaces, opts.pull, opts.fetch);
        if plan.is_empty() {
            let mut tok = shutdown.token();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(opts.interval)) => continue,
                () = tok.wait_ref() => break,
            }
        }

        if !opts.quiet {
            display::print_daemon_cycle_start(cycle);
            eprintln!("  plan: {}", plan.summary());
        }

        // Process all workspaces in parallel
        // ── Host-pressure gate ──────────────────────────────────────
        // Asked ONCE PER CYCLE, before anything is dispatched. `max_inflight`
        // answers "how many at once" but never "should any of this run right
        // now" — so an unattended daemon would keep cloning and evaluating nix
        // on a host nearly out of disk, where every job makes it worse.
        //
        // Failure to READ pressure is not a reason to stop: an unreadable `df`
        // must not silently stall the reconciler, or the guard becomes an
        // outage of its own. Only a real, measured reading can stop a cycle.
        let effective_inflight = {
            let reader = crate::pressure::SystemPressureReader {
                path: workspaces.first().map_or_else(
                    || std::path::PathBuf::from("."),
                    |w| std::path::PathBuf::from(&w.base_dir),
                ),
            };
            match crate::pressure::PressureReader::read(&reader) {
                Ok(reading) => {
                    let verdict = crate::pressure::assess(
                        reading,
                        crate::pressure::Thresholds::default(),
                        opts.max_inflight,
                    );
                    match verdict.inflight(opts.max_inflight) {
                        Some(n) => {
                            if n != opts.max_inflight {
                                eprintln!("tend: throttling to {n} — {}", verdict.why());
                            }
                            n
                        }
                        None => {
                            // Say why, every time. A daemon that goes quiet
                            // without explanation is indistinguishable from one
                            // that has crashed.
                            eprintln!("tend: skipping cycle — {}", verdict.why());
                            continue;
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "tend: pressure unreadable ({e}); proceeding at configured concurrency"
                    );
                    opts.max_inflight
                }
            }
        };

        let mut tasks = tokio::task::JoinSet::new();
        for ws in workspaces {
            let ws = ws.clone();
            let pull = opts.pull;
            let fetch = opts.fetch;
            let quiet = opts.quiet;
            let max_inflight = effective_inflight;
            tasks.spawn(async move {
                let name = ws.name.clone();
                match run_workspace_cycle(&ws, pull, fetch, quiet, max_inflight).await {
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

        // Wire the kanshou counters at the cycle boundary so external
        // observers see "tick N completed at T, ms_since_last = now-T"
        // and "tend has done X total cycles."
        kanshou_state
            .ticks_completed
            .fetch_add(1, Ordering::Relaxed);
        kanshou_state
            .last_tick_unix_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        kanshou_state.current_repo.write().take();
        kanshou_state.current_workspace.write().take();

        let mut tok = shutdown.token();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(opts.interval)) => {}
            // External poke (SIGHUP) reloaded the token — start the next
            // cycle immediately so the fresh credential is used now
            // (e.g. previously-401 https workspaces retry at once).
            () = reload_now.notified() => {}
            () = tok.wait_ref() => break,
        }
    }

    Ok(())
}

async fn run_workspace_cycle(
    ws: &crate::config::Workspace,
    pull: bool,
    fetch: bool,
    quiet: bool,
    max_inflight: u32,
) -> Result<()> {
    let repos = sync::resolve_repos(ws, false).await?;

    let transition_log_path = crate::audit::AuditLog::default_path()
        .path()
        .parent()
        .map(|p| p.join("scheduler-transitions.jsonl"));

    if pull {
        // Full reconcile via the scheduler: SyncRepoJob → (Dag edge) →
        // PullRepoJob per repo. Both kinds share max_inflight (separate
        // per-kind counters so sync waves don't starve pulls). Per
        // theory/SHIGOTO.md §IV.5: this is the canonical daemon cycle
        // — one scheduler run, one typed receipt, all four reconcile
        // primitives reachable from the same audit log.
        let receipt = reconcile::reconcile_workspace_sync_then_pull(
            ws,
            &repos,
            max_inflight,
            transition_log_path.as_deref(),
        )
        .await?;

        // Third copy of this tally (sync.rs has one, and so does the
        // legacy path below). All three dropped Failed on the floor; the
        // comment here said "surfaced in failed_jobs", which is true and
        // does not stop THIS line from printing "all N repos present".
        let mut cloned = 0usize;
        let mut present = 0usize;
        let mut failed = 0usize;
        for outcome in receipt.sync_outcomes.values() {
            match outcome {
                crate::sync::SyncOutcome::Cloned => cloned += 1,
                crate::sync::SyncOutcome::AlreadyPresent
                | crate::sync::SyncOutcome::StubExisted => present += 1,
                crate::sync::SyncOutcome::Failed { .. } => failed += 1,
            }
        }
        if !quiet || cloned > 0 || failed > 0 {
            display::print_sync_summary(&ws.name, cloned, present, failed);
        }

        let summary = receipt.as_pull_summary();
        let audit = crate::audit::AuditLog::default_path();
        audit.pull_completed(
            &ws.name,
            summary.updated,
            summary.up_to_date,
            summary.dirty_skipped,
            summary.missing_skipped,
            summary.failed,
        );
        if !quiet || summary.updated > 0 || summary.failed > 0 {
            display::print_pull_summary(&ws.name, &summary);
        }
    } else {
        // Pull disabled — fall back to legacy batch paths for sync +
        // (optionally) fetch. Migrating these to scheduler is future
        // scope; pull is the load-bearing reconciler step.
        let (cloned, present, failed) = sync::sync_repos(ws, &repos, quiet).await?;
        if !quiet || cloned > 0 {
            display::print_sync_summary(&ws.name, cloned, present, failed);
        }
        if fetch {
            let (fetched, skipped) = sync::fetch_repos(ws, &repos, quiet).await?;
            if !quiet {
                display::print_fetch_summary(&ws.name, fetched, skipped);
            }
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

            match watch::run_watch_cycle(
                ws,
                quiet,
                &gh,
                &cache_store,
                &matrix_appender,
                &git_ops,
                &audit,
            )
            .await
            {
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
                    let findings = v
                        .get("findings")
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

/// Build an ExecutionPlan from workspace config. Pure — no IO.
///
/// Each configured concern becomes a WorkItem. The planner stages them
/// by dependency. An empty plan means no workspace has any concern
/// enabled — the daemon then sleeps silently until the next tick.
fn build_plan(workspaces: &[&crate::config::Workspace], pull: bool, fetch: bool) -> ExecutionPlan {
    let mut items: Vec<WorkItem> = Vec::new();
    for ws in workspaces {
        // Sync is always potentially-useful: resolves missing repos. Cheap
        // to enumerate, cheap to no-op when nothing's missing.
        items.push(WorkItem::workspace(WorkKind::Sync, &ws.name));

        // Pull subsumes fetch; only one ends up in the plan.
        if pull || fetch {
            items.push(WorkItem::workspace(WorkKind::Pull, &ws.name));
        }

        if let Some(wcfg) = &ws.watch {
            if wcfg.enable {
                items.push(WorkItem::workspace(WorkKind::Watch, &ws.name));
            }
            if let Some(ncfg) = &wcfg.nix_audit {
                if ncfg.enable {
                    items.push(WorkItem::workspace(WorkKind::NixAudit, &ws.name));
                }
            }
        }
    }
    planner::plan(items)
}

/// Execute a single post-hook, logging the result.
async fn run_hook(hook: &crate::config::PostHook, audit: &crate::audit::AuditLog) {
    let start = std::time::Instant::now();
    let result = tokio::process::Command::new(&hook.command)
        .args(&hook.args)
        .output()
        .await;

    let (exit_code, duration_ms) = match result {
        Ok(output) => (
            output.status.code().unwrap_or(-1),
            start.elapsed().as_millis() as u64,
        ),
        Err(_) => (-1, start.elapsed().as_millis() as u64),
    };

    audit.hook_executed(&hook.trigger, &hook.command, exit_code, duration_ms);
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_token_file_trims_whitespace_and_newline() {
        let mut f = tempfile::NamedTempFile::new().expect("temp file");
        // Token files commonly carry a trailing newline (sops/echo) and
        // sometimes surrounding whitespace — the reload must trim both so
        // the value matches what GitHub expects.
        write!(f, "  ghp_exampletoken123\n").expect("write");
        let got = read_token_file(f.path()).expect("read");
        assert_eq!(got, "ghp_exampletoken123");
    }

    #[test]
    fn read_token_file_missing_path_errors() {
        let err =
            read_token_file(std::path::Path::new("/nonexistent/tend/token/path/xyz")).unwrap_err();
        assert!(err.to_string().contains("reading token from"));
    }
}
