use colored::Colorize;

use crate::sync::{PullSummary, RepoEntry, RepoStatus};
use crate::watch;

/// One machine-readable `tend status --json` row — the typed contract
/// izumi's `tend-repos` board source parses (`state` is the lowercase
/// status word; `path` is the on-disk working copy, empty when the repo is
/// missing so consumers know there is no cwd to land in yet).
#[derive(Debug, serde::Serialize)]
pub(crate) struct StatusJsonRow {
    pub name: String,
    pub path: String,
    pub state: String,
}

impl StatusJsonRow {
    pub(crate) fn new(entry: &RepoEntry, base_dir: &std::path::Path) -> Self {
        let path = if matches!(entry.status, RepoStatus::Missing) {
            String::new()
        } else {
            base_dir.join(&entry.name).to_string_lossy().into_owned()
        };
        Self {
            name: entry.name.clone(),
            path,
            state: entry.status.to_string(),
        }
    }
}

/// Print colored status table for all repos in a workspace.
pub(crate) fn print_status(workspace_name: &str, entries: &[RepoEntry]) {
    let clean = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Clean))
        .count();
    let dirty = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Dirty))
        .count();
    let stuck = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Stuck))
        .count();
    let missing = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Missing))
        .count();
    let unknown = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Unknown))
        .count();

    println!("{}", format!("workspace: {workspace_name}").bold());
    println!();

    for entry in entries {
        let icon = match &entry.status {
            RepoStatus::Clean => "ok".green().to_string(),
            RepoStatus::Dirty => "!!".yellow().to_string(),
            RepoStatus::Stuck => "RB".red().bold().to_string(),
            RepoStatus::Missing => "--".red().to_string(),
            RepoStatus::Unknown => "??".cyan().to_string(),
        };
        println!("  [{icon}] {:<40} {}", entry.name, entry.status);
    }

    println!();
    println!(
        "  {} clean, {} dirty, {} stuck, {} missing, {} unknown",
        clean.to_string().green(),
        dirty.to_string().yellow(),
        stuck.to_string().red().bold(),
        missing.to_string().red(),
        unknown.to_string().cyan(),
    );
}

/// Print sync summary (cloned vs already-present counts).
pub(crate) fn print_sync_summary(workspace_name: &str, cloned: usize, present: usize) {
    if cloned == 0 {
        println!(
            "{}: all {} repos present",
            workspace_name.bold(),
            present
        );
    } else {
        println!(
            "{}: cloned {} new, {} already present",
            workspace_name.bold(),
            cloned.to_string().green(),
            present
        );
    }
}

/// Print the full list of repos in a workspace.
pub(crate) fn print_repo_list(workspace_name: &str, repos: &[String]) {
    println!("{} ({} repos):", workspace_name.bold(), repos.len());
    for repo in repos {
        println!("  {repo}");
    }
}

/// Print discovered repos for a GitHub org.
pub(crate) fn print_discover_results(org: &str, repos: &[String]) {
    println!(
        "discovered {} repos in {}:",
        repos.len().to_string().green(),
        org.bold()
    );
    for repo in repos {
        println!("  {repo}");
    }
}

/// Print daemon cycle start banner with timestamp.
pub(crate) fn print_daemon_cycle_start(cycle: u64) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!(
        "[{}] {} cycle {}",
        now,
        "daemon:".bold(),
        cycle.to_string().cyan()
    );
}

/// Print daemon cycle completion with workspace count.
pub(crate) fn print_daemon_cycle_done(cycle: u64, workspaces: usize) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!(
        "[{}] {} cycle {} done ({} workspaces)",
        now,
        "daemon:".bold(),
        cycle,
        workspaces.to_string().green()
    );
}

/// Print pull summary (updated, up-to-date, dirty-skipped, etc.).
pub(crate) fn print_pull_summary(workspace_name: &str, summary: &PullSummary) {
    println!(
        "{}: {} updated, {} up-to-date, {} dirty skipped, {} missing, {} failed",
        workspace_name.bold(),
        summary.updated.to_string().green(),
        summary.up_to_date.to_string().cyan(),
        summary.dirty_skipped.to_string().yellow(),
        summary.missing_skipped.to_string().red(),
        summary.failed.to_string().red(),
    );
}

/// Print fetch summary (fetched vs skipped counts).
pub(crate) fn print_fetch_summary(workspace_name: &str, fetched: usize, skipped: usize) {
    if fetched == 0 && skipped == 0 {
        return;
    }
    println!(
        "{}: fetched {}, skipped {}",
        workspace_name.bold(),
        fetched.to_string().green(),
        skipped.to_string().yellow(),
    );
}

/// Print a daemon-level error for a workspace.
pub(crate) fn print_daemon_error(workspace_name: &str, err: &anyhow::Error) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    eprintln!(
        "[{}] {}: {} {}",
        now,
        "error".red().bold(),
        workspace_name,
        err
    );
}

/// Print daemon sleep interval.
pub(crate) fn print_daemon_sleeping(interval: u64) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!(
        "[{}] {} sleeping {}s",
        now,
        "daemon:".bold(),
        interval
    );
}

/// Print the header for a flake update chain.
pub(crate) fn print_flake_chain_header(workspace_name: &str, changed: &str, steps: &[crate::flake::UpdateStep]) {
    println!("{}", format!("workspace: {workspace_name}").bold());
    println!("  changed: {}", changed.cyan());
    println!("  chain ({} steps):", steps.len().to_string().green());
    for (i, step) in steps.iter().enumerate() {
        println!(
            "    {}. {} → nix flake update {}",
            i + 1,
            step.repo.bold(),
            step.inputs.join(" ")
        );
    }
    println!();
}

/// Print a flake update step starting.
pub(crate) fn print_flake_step_start(step: usize, total: usize, repo: &str, inputs: &[String]) {
    println!(
        "  [{}/{}] {} nix flake update {}",
        step,
        total,
        repo.bold(),
        inputs.join(" ")
    );
}

/// Print a successful flake update step.
pub(crate) fn print_flake_step_done(repo: &str) {
    println!("  [{}] {} committed and pushed", "ok".green(), repo);
}

/// Print a dry-run skip indicator.
pub(crate) fn print_flake_step_dry_run() {
    println!("  [{}] (dry-run, skipped)", ">>".yellow());
}

/// Print indicator that flake.lock was unchanged.
pub(crate) fn print_flake_step_no_changes(repo: &str) {
    println!("  [{}] {} flake.lock unchanged", "==".cyan(), repo);
}

/// Print chain completion summary.
pub(crate) fn print_flake_chain_complete(updated: usize) {
    if updated == 0 {
        println!("\n  {}", "no repos needed updating".cyan());
    } else {
        println!(
            "\n  {} {} updated",
            "done:".green().bold(),
            updated.to_string().green()
        );
    }
}

/// Print watch cycle summary.
pub(crate) fn print_watch_summary(workspace_name: &str, summary: &watch::WatchSummary) {
    if summary.new_versions == 0 && summary.file_changes == 0 && summary.flake_input_updates == 0 && summary.flake_refreshed == 0 {
        println!(
            "{}: watched {} repos, no new versions",
            workspace_name.bold(),
            summary.checked,
        );
    } else {
        let mut parts = Vec::new();
        if summary.new_versions > 0 {
            parts.push(format!("{} new versions", summary.new_versions.to_string().green()));
        }
        if summary.file_changes > 0 {
            parts.push(format!("{} file changes", summary.file_changes.to_string().green()));
        }
        if summary.flake_input_updates > 0 {
            parts.push(format!("{} flake input updates", summary.flake_input_updates.to_string().green()));
        }
        if summary.flake_refreshed > 0 {
            parts.push(format!("{} flake refreshed", summary.flake_refreshed.to_string().green()));
        }
        println!(
            "{}: watched {} repos, {} detected",
            workspace_name.bold(),
            summary.checked,
            parts.join(", "),
        );
    }
    if summary.errors > 0 {
        println!(
            "  {} repos had errors",
            summary.errors.to_string().yellow(),
        );
    }
}

/// Print a flake refresh skip with reason.
pub(crate) fn print_flake_refresh_skip(repo: &str, reason: &str) {
    println!(
        "  [{}] {} ({})",
        "--".cyan(),
        repo,
        reason,
    );
}

/// Print a successful flake refresh.
pub(crate) fn print_flake_refresh_updated(repo: &str) {
    println!("  [{}] {} refreshed and pushed", "ok".green(), repo.bold());
}

/// Print indicator that flake.lock was unchanged after refresh.
pub(crate) fn print_flake_refresh_no_changes(repo: &str) {
    println!("  [{}] {} flake.lock unchanged", "==".cyan(), repo);
}

/// Print a flake refresh error.
pub(crate) fn print_flake_refresh_error(repo: &str, err: &str) {
    eprintln!(
        "  [{}] {} {}",
        "!!".red(),
        repo,
        err,
    );
}

/// Print a newly detected version.
pub(crate) fn print_watch_new_version(repo: &str, version: &str, tag: &str) {
    println!(
        "  [{}] {} {} (tag: {})",
        "new".green(),
        repo.bold(),
        version,
        tag,
    );
}
