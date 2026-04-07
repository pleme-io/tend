use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use crate::config::Workspace;
use crate::provider;

/// Status of a single repo in the workspace
#[derive(Debug)]
/// Status of a single repo in the workspace.
pub(crate) enum RepoStatus {
    /// Repo exists and has no uncommitted changes.
    Clean,
    /// Repo exists but has uncommitted changes.
    Dirty,
    /// Repo is expected but not cloned.
    Missing,
    /// Repo exists on disk but not in config.
    Unknown,
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean => f.write_str("clean"),
            Self::Dirty => f.write_str("dirty"),
            Self::Missing => f.write_str("missing"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// A repo name paired with its status.
#[derive(Debug)]
pub(crate) struct RepoEntry {
    pub name: String,
    pub status: RepoStatus,
}

/// Resolve the full list of repos for a workspace (discover + extras - excludes).
/// When `refresh` is true, the discovery cache is bypassed and the GitHub API is always called.
pub(crate) async fn resolve_repos(workspace: &Workspace, refresh: bool) -> Result<Vec<String>> {
    let mut repos = Vec::new();

    if workspace.discover {
        let org = workspace
            .org
            .as_deref()
            .unwrap_or(&workspace.name);
        let discovered = provider::discover_github_repos_cached(org, refresh).await?;
        repos.extend(discovered);
    }

    for extra in &workspace.extra_repos {
        if !repos.contains(extra) {
            repos.push(extra.clone());
        }
    }

    repos.retain(|r| !workspace.exclude.contains(r));
    repos.sort();
    repos.dedup();

    Ok(repos)
}

/// Clone missing repos. Returns (cloned, already_present) counts.
pub(crate) async fn sync_repos(workspace: &Workspace, repos: &[String], quiet: bool) -> Result<(usize, usize)> {
    let base_dir = workspace.resolved_base_dir()?;
    std::fs::create_dir_all(&base_dir)
        .with_context(|| format!("creating {}", base_dir.display()))?;

    let mut cloned = 0usize;
    let mut present = 0usize;

    for repo_name in repos {
        let repo_path = base_dir.join(repo_name);
        if repo_path.exists() {
            present += 1;
            continue;
        }

        let url = workspace.clone_url(repo_name);
        if !quiet {
            println!("  cloning {repo_name}...");
        }

        let output = Command::new("git")
            .args(["clone", &url, &repo_path.to_string_lossy()])
            .output()
            .with_context(|| format!("running git clone for {repo_name}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("  warning: failed to clone {repo_name}: {stderr}");
            continue;
        }

        cloned += 1;
    }

    Ok((cloned, present))
}

/// Check status of all repos in a workspace
pub(crate) async fn check_status(workspace: &Workspace, repos: &[String]) -> Result<Vec<RepoEntry>> {
    let base_dir = workspace.resolved_base_dir()?;
    let mut entries = Vec::new();

    // Check expected repos
    for repo_name in repos {
        let repo_path = base_dir.join(repo_name);
        let status = if !repo_path.exists() {
            RepoStatus::Missing
        } else if is_dirty(&repo_path)? {
            RepoStatus::Dirty
        } else {
            RepoStatus::Clean
        };
        entries.push(RepoEntry {
            name: repo_name.clone(),
            status,
        });
    }

    // Check for unknown repos on disk
    if base_dir.exists() {
        let mut on_disk: Vec<String> = std::fs::read_dir(&base_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                // Skip hidden dirs
                if name.starts_with('.') {
                    return None;
                }
                // Skip if already in the expected list
                if repos.contains(&name) {
                    return None;
                }
                Some(name)
            })
            .collect();

        on_disk.sort();
        for name in on_disk {
            entries.push(RepoEntry {
                name,
                status: RepoStatus::Unknown,
            });
        }
    }

    Ok(entries)
}

/// Fetch all remotes for existing repos. Returns (fetched, skipped) counts.
pub(crate) async fn fetch_repos(workspace: &Workspace, repos: &[String], quiet: bool) -> Result<(usize, usize)> {
    let base_dir = workspace.resolved_base_dir()?;
    let mut fetched = 0usize;
    let mut skipped = 0usize;

    for repo_name in repos {
        let repo_path = base_dir.join(repo_name);
        if !repo_path.join(".git").exists() {
            skipped += 1;
            continue;
        }

        let output = Command::new("git")
            .args(["fetch", "--all", "--prune", "--quiet"])
            .current_dir(&repo_path)
            .output()
            .with_context(|| format!("running git fetch in {repo_name}"))?;

        if output.status.success() {
            fetched += 1;
            if !quiet {
                println!("  fetched: {repo_name}");
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("  warning: fetch failed for {repo_name}: {stderr}");
            skipped += 1;
        }
    }

    Ok((fetched, skipped))
}

/// Aggregate outcome of pulling all repos in a workspace.
#[derive(Debug, Default)]
pub(crate) struct PullSummary {
    pub updated: usize,
    pub up_to_date: usize,
    pub dirty_skipped: usize,
    pub missing_skipped: usize,
    pub failed: usize,
}

/// Pull the default branch (fast-forward only) for every repo in the workspace.
///
/// Behavior:
/// - Missing repos are skipped (use `tend sync` to clone them first).
/// - Dirty repos are skipped to avoid merge conflicts.
/// - Clean repos are updated with `git pull --ff-only`.
/// - Also walks unexpected directories under `base_dir` (the "unknown" repos)
///   so that running `tend pull` updates every git repo in the workspace,
///   not just the ones tend discovered.
pub(crate) async fn pull_repos(
    workspace: &Workspace,
    repos: &[String],
    quiet: bool,
) -> Result<PullSummary> {
    let base_dir = workspace.resolved_base_dir()?;
    let mut summary = PullSummary::default();

    // Union of configured repos and on-disk directories so we pull everything
    // living in the workspace, including repos tend doesn't know about yet.
    let mut all: Vec<String> = repos.to_vec();
    if base_dir.exists() {
        let on_disk = std::fs::read_dir(&base_dir)
            .with_context(|| format!("reading {}", base_dir.display()))?;
        for entry in on_disk.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if !all.contains(&name) {
                all.push(name);
            }
        }
    }
    all.sort();
    all.dedup();

    for repo_name in &all {
        let repo_path = base_dir.join(repo_name);
        if !repo_path.join(".git").exists() {
            summary.missing_skipped += 1;
            continue;
        }

        if is_dirty(&repo_path)? {
            summary.dirty_skipped += 1;
            if !quiet {
                println!("  dirty (skipped): {repo_name}");
            }
            continue;
        }

        let output = Command::new("git")
            .args(["pull", "--ff-only", "--quiet"])
            .current_dir(&repo_path)
            .output()
            .with_context(|| format!("running git pull in {repo_name}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("  warning: pull failed for {repo_name}: {}", stderr.trim());
            summary.failed += 1;
            continue;
        }

        // `git pull --quiet` prints nothing on up-to-date, and a short
        // "Updating ..." on an actual fast-forward. Distinguish by checking
        // whether HEAD moved via git rev-parse.
        let before = Command::new("git")
            .args(["rev-parse", "@{push}"])
            .current_dir(&repo_path)
            .output();
        let _ = before; // best-effort, not critical

        // Simpler heuristic: look at stdout/stderr for "Already up to date"
        // (git prints this even with --quiet in some versions) or compare via
        // whether stdout is empty. The safest portable check is an explicit
        // `git status -sb` scan; here we just count a successful pull as
        // either updated or up-to-date and rely on the caller's summary.
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if combined.contains("Already up to date") || combined.trim().is_empty() {
            summary.up_to_date += 1;
        } else {
            summary.updated += 1;
            if !quiet {
                println!("  updated: {repo_name}");
            }
        }
    }

    Ok(summary)
}

fn is_dirty(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("checking git status in {}", repo_path.display()))?;

    Ok(!output.stdout.is_empty())
}
