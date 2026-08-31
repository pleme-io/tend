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
    /// The remote a `clean` verdict was derived against — the machine-
    /// readable projection of `RepoStatus::Clean`'s `RemoteWitness`.
    /// `None` for every other state, so a consumer can never read
    /// `state: "clean"` without also being able to see *what it was
    /// clean relative to*. `state: "no-remote"` is the case this exists
    /// to make visible.
    pub clean_against_remote: Option<String>,
}

impl StatusJsonRow {
    pub(crate) fn new(entry: &RepoEntry, base_dir: &std::path::Path) -> Self {
        let path = if matches!(entry.status, RepoStatus::Missing) {
            String::new()
        } else {
            base_dir.join(&entry.name).to_string_lossy().into_owned()
        };
        let clean_against_remote = match &entry.status {
            RepoStatus::Clean(witness) => Some(witness.remote().to_string()),
            _ => None,
        };
        Self {
            name: entry.name.clone(),
            path,
            state: entry.status.to_string(),
            clean_against_remote,
        }
    }
}

/// Print colored status table for all repos in a workspace.
pub(crate) fn print_status(workspace_name: &str, entries: &[RepoEntry]) {
    let clean = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Clean(_)))
        .count();
    let dirty = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Dirty))
        .count();
    let stuck = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Stuck))
        .count();
    let no_remote = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::NoRemote))
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
            RepoStatus::Clean(_) => "ok".green().to_string(),
            RepoStatus::Dirty => "!!".yellow().to_string(),
            RepoStatus::Stuck => "RB".red().bold().to_string(),
            // Loudest marker in the table: an unbacked repo is the one
            // state no local action can fix and the one tend used to
            // call "ok".
            RepoStatus::NoRemote => "!R".red().bold().to_string(),
            RepoStatus::Missing => "--".red().to_string(),
            RepoStatus::Unknown => "??".cyan().to_string(),
        };
        let note = match &entry.status {
            RepoStatus::NoRemote => "  <- history exists on this machine only"
                .red()
                .bold()
                .to_string(),
            _ => String::new(),
        };
        println!("  [{icon}] {:<40} {}{note}", entry.name, entry.status);
    }

    println!();
    println!(
        "  {} clean, {} dirty, {} stuck, {} no-remote, {} missing, {} unknown",
        clean.to_string().green(),
        dirty.to_string().yellow(),
        stuck.to_string().red().bold(),
        no_remote.to_string().red().bold(),
        missing.to_string().red(),
        unknown.to_string().cyan(),
    );
}

/// Print sync summary — cloned / already-present / FAILED.
///
/// ── ★ "all N repos present" WAS A CLAIM ABOUT REPOS NOT ON DISK ──────
/// Failed clones counted toward neither bucket, so a workspace where every
/// clone failed arrived here as `cloned = 0` and printed an affirmative
/// completeness claim. The failure count is printed unconditionally when
/// non-zero, and the all-present line now requires that nothing failed —
/// "nothing needed cloning" and "nothing COULD be cloned" were the same
/// sentence.
pub(crate) fn print_sync_summary(
    workspace_name: &str,
    cloned: usize,
    present: usize,
    failed: usize,
) {
    if cloned == 0 && failed == 0 {
        println!("{}: all {} repos present", workspace_name.bold(), present);
        return;
    }
    let mut parts = Vec::new();
    if cloned > 0 {
        parts.push(format!("cloned {}", cloned.to_string().green()));
    }
    if present > 0 {
        parts.push(format!("{present} already present"));
    }
    if failed > 0 {
        parts.push(format!("{} FAILED", failed.to_string().red().bold()));
    }
    println!("{}: {}", workspace_name.bold(), parts.join(", "));
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
        "{}: {} updated, {} up-to-date, {} dirty skipped, {} no-remote, {} empty, {} missing, {} failed",
        workspace_name.bold(),
        summary.updated.to_string().green(),
        summary.up_to_date.to_string().cyan(),
        summary.dirty_skipped.to_string().yellow(),
        summary.no_remote_skipped.to_string().red().bold(),
        summary.empty_skipped.to_string().yellow(),
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
    println!("[{}] {} sleeping {}s", now, "daemon:".bold(), interval);
}

/// Print the header for a flake update chain.
pub(crate) fn print_flake_chain_header(
    workspace_name: &str,
    changed: &str,
    steps: &[crate::flake::UpdateStep],
) {
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

/// Print a step the chain stepped OVER, with the reason.
///
/// Deliberately loud (`!!`, yellow) and deliberately not fatal: the whole
/// point is that the run continues, so this line is the only record that a
/// repo was left behind. A silent skip would be worse than the abort it
/// replaces — the operator would believe the fleet was updated.
pub(crate) fn print_flake_step_blocked(repo: &str, why: &str) {
    println!("  [{}] {} BLOCKED — {}", "!!".yellow(), repo, why);
}

/// Print indicator that flake.lock was unchanged.
pub(crate) fn print_flake_step_no_changes(repo: &str) {
    println!("  [{}] {} flake.lock unchanged", "==".cyan(), repo);
}

/// Print chain completion summary.
///
/// ── ★ PRINT WHAT WAS MEASURED, NOT WHAT WAS ATTEMPTED ────────────────
/// The caller used to pass `chain.len()` — the number of steps TRIED — into
/// a parameter named `updated`, so blocked, unchanged and skipped steps all
/// rendered as updates. A run in which every repo was blocked printed
/// "done: 12 updated".
///
/// `blocked` is printed even when zero updates happened, because "no repos
/// needed updating" and "no repo could be updated" are opposite facts that
/// looked identical on this line.
pub(crate) fn print_flake_chain_complete(updated: usize, blocked: usize) {
    if updated == 0 && blocked == 0 {
        println!("\n  {}", "no repos needed updating".cyan());
    } else {
        let mut line = String::new();
        if updated > 0 {
            line.push_str(&updated.to_string().green().to_string());
            line.push_str(" updated");
        }
        if blocked > 0 {
            if !line.is_empty() {
                line.push_str(", ");
            }
            line.push_str(&blocked.to_string().yellow().to_string());
            line.push_str(" BLOCKED");
        }
        println!("\n  {} {}", "done:".green().bold(), line);
    }
}

/// Print watch cycle summary.
pub(crate) fn print_watch_summary(workspace_name: &str, summary: &watch::WatchSummary) {
    if summary.new_versions == 0
        && summary.file_changes == 0
        && summary.flake_input_updates == 0
        && summary.flake_refreshed == 0
    {
        println!(
            "{}: watched {} repos, no new versions",
            workspace_name.bold(),
            summary.checked,
        );
    } else {
        let mut parts = Vec::new();
        if summary.new_versions > 0 {
            parts.push(format!(
                "{} new versions",
                summary.new_versions.to_string().green()
            ));
        }
        if summary.file_changes > 0 {
            parts.push(format!(
                "{} file changes",
                summary.file_changes.to_string().green()
            ));
        }
        if summary.flake_input_updates > 0 {
            parts.push(format!(
                "{} flake input updates",
                summary.flake_input_updates.to_string().green()
            ));
        }
        if summary.flake_refreshed > 0 {
            parts.push(format!(
                "{} flake refreshed",
                summary.flake_refreshed.to_string().green()
            ));
        }
        println!(
            "{}: watched {} repos, {} detected",
            workspace_name.bold(),
            summary.checked,
            parts.join(", "),
        );
    }
    if summary.errors > 0 {
        println!("  {} repos had errors", summary.errors.to_string().yellow(),);
    }
}

/// Print a flake refresh skip with reason.
pub(crate) fn print_flake_refresh_skip(repo: &str, reason: &str) {
    println!("  [{}] {} ({})", "--".cyan(), repo, reason,);
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
    eprintln!("  [{}] {} {}", "!!".red(), repo, err,);
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
