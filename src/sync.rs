use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use crate::config::Workspace;
use crate::provider;

/// Status of a single repo in the workspace
#[derive(Debug)]
pub enum RepoStatus {
    /// Repo exists and has no uncommitted changes
    Clean,
    /// Repo exists but has uncommitted changes
    Dirty,
    /// Repo is expected but not cloned
    Missing,
    /// Repo exists on disk but not in config
    Unknown,
}

#[derive(Debug)]
pub struct RepoEntry {
    pub name: String,
    pub status: RepoStatus,
}

/// Resolve the full list of repos for a workspace (discover + extras - excludes)
pub async fn resolve_repos(workspace: &Workspace) -> Result<Vec<String>> {
    let mut repos = Vec::new();

    if workspace.discover {
        let org = workspace
            .org
            .as_deref()
            .unwrap_or(&workspace.name);
        let discovered = provider::discover_github_repos(org).await?;
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
pub async fn sync_repos(workspace: &Workspace, repos: &[String], quiet: bool) -> Result<(usize, usize)> {
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
pub async fn check_status(workspace: &Workspace, repos: &[String]) -> Result<Vec<RepoEntry>> {
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

fn is_dirty(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("checking git status in {}", repo_path.display()))?;

    Ok(!output.stdout.is_empty())
}
