use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use crate::config::Workspace;
use crate::provider;

/// Status of a single repo in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum RepoStatus {
    /// Repo exists and has no uncommitted changes.
    Clean,
    /// Repo exists but has uncommitted changes.
    Dirty,
    /// Repo is mid rebase, merge, or cherry-pick — a conflict or an
    /// abandoned operator session may be sitting on recoverable work.
    /// Distinct from `Dirty`: a repo with one modified lockfile and a
    /// repo with an abandoned rebase both show unmerged/modified paths
    /// in `git status --porcelain`, but only the second one can silently
    /// strand real work for weeks with no fleet-wide signal. Checked
    /// before `Dirty` so it always wins.
    Stuck,
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
            Self::Stuck => f.write_str("stuck"),
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
/// Typed outcome of cloning-or-noop'ing a single repo. Shared by the
/// batch `sync_repos` driver and the per-repo `SyncRepoJob`.
///
/// `AlreadyPresent` and `StubExisted` are distinct because the latter
/// indicates a path that exists but lacks `.git` — operator intervention
/// is needed (we don't auto-clobber working data). The batch driver
/// folds both into a single "present" count for backward compat, but
/// per-repo consumers can disambiguate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    /// Path exists and is a valid `.git` worktree — nothing to do.
    AlreadyPresent,
    /// Path exists but has no `.git` — warned, not cloned.
    StubExisted,
    /// Repo was cloned fresh.
    Cloned,
    /// `git clone` exited non-zero. Carries trimmed stderr.
    Failed { stderr: String },
}

/// Inject `GITHUB_TOKEN` into an HTTPS GitHub URL for basic-auth clone.
/// Containers can't prompt for creds; the rewritten URL is what git
/// actually sees on the wire. Public repos work too — the token is
/// just unused.
fn inject_github_token(url: String) -> String {
    let Ok(token) = std::env::var("GITHUB_TOKEN") else {
        return url;
    };
    if url.starts_with("https://github.com/") {
        url.replacen(
            "https://github.com/",
            &format!("https://x-access-token:{token}@github.com/"),
            1,
        )
    } else {
        url
    }
}

/// Sync one repo. The unit shared by the batch driver and the
/// per-repo `SyncRepoJob`. `clone_url` is pre-constructed by the
/// caller (typically `workspace.clone_url(repo_name)`) so this helper
/// has no `Workspace` dependency and is testable with arbitrary URLs.
pub(crate) fn sync_one_repo(
    clone_url: String,
    repo_path: &Path,
    repo_label: &str,
    quiet: bool,
) -> Result<SyncOutcome> {
    if repo_path.exists() {
        if !is_git_worktree(repo_path) {
            eprintln!(
                "  warning: {repo_label} exists without .git — remove {} to re-clone",
                repo_path.display()
            );
            return Ok(SyncOutcome::StubExisted);
        }
        return Ok(SyncOutcome::AlreadyPresent);
    }

    let url = inject_github_token(clone_url);

    if !quiet {
        println!("  cloning {repo_label}...");
    }

    let output = Command::new("git")
        .args(["clone", &url, &repo_path.to_string_lossy()])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("running git clone for {repo_label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        eprintln!("  warning: failed to clone {repo_label}: {stderr}");
        return Ok(SyncOutcome::Failed { stderr });
    }

    Ok(SyncOutcome::Cloned)
}

pub(crate) async fn sync_repos(workspace: &Workspace, repos: &[String], quiet: bool) -> Result<(usize, usize)> {
    let base_dir = workspace.resolved_base_dir()?;
    std::fs::create_dir_all(&base_dir)
        .with_context(|| format!("creating {}", base_dir.display()))?;

    let mut cloned = 0usize;
    let mut present = 0usize;

    for repo_name in repos {
        let repo_path = base_dir.join(repo_name);
        let clone_url = workspace.clone_url(repo_name);
        match sync_one_repo(clone_url, &repo_path, repo_name, quiet)? {
            SyncOutcome::Cloned => cloned += 1,
            SyncOutcome::AlreadyPresent | SyncOutcome::StubExisted => present += 1,
            // Match the existing batch behavior: failed clones aren't counted
            // toward either bucket — they show up via the eprintln side effect.
            SyncOutcome::Failed { .. } => {}
        }
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
        let status = check_one_repo_status(&repo_path)?;
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

/// Typed outcome of fetching a single repo. Shared by `fetch_repos`'s
/// batch driver and the per-repo `FetchRepoJob` (jobs/fetch_repo.rs).
///
/// Unlike `PullOutcome` there is no "DirtySkipped" — fetch doesn't
/// touch the working tree, so dirty state is irrelevant. Missing
/// (no `.git`) and Failed (non-zero git exit) are the only non-success
/// terminals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchOutcome {
    /// Fetch succeeded. Git emitted nothing (--quiet) or a refs report.
    Fetched,
    /// Path doesn't contain a `.git` entry — fetch was skipped.
    MissingSkipped,
    /// `git fetch` exited non-zero. Carries the trimmed stderr.
    Failed { stderr: String },
}

/// Fetch one repo. The unit shared by the batch driver and the
/// per-repo Job wrapper.
pub(crate) fn fetch_one_repo(repo_path: &Path, quiet: bool, repo_label: &str) -> Result<FetchOutcome> {
    if !is_git_worktree(repo_path) {
        return Ok(FetchOutcome::MissingSkipped);
    }

    let output = Command::new("git")
        .args(["fetch", "--all", "--prune", "--quiet"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("running git fetch in {repo_label}"))?;

    if output.status.success() {
        if !quiet {
            println!("  fetched: {repo_label}");
        }
        Ok(FetchOutcome::Fetched)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        eprintln!("  warning: fetch failed for {repo_label}: {stderr}");
        Ok(FetchOutcome::Failed { stderr })
    }
}

/// Fetch all remotes for existing repos. Returns (fetched, skipped) counts.
pub(crate) async fn fetch_repos(workspace: &Workspace, repos: &[String], quiet: bool) -> Result<(usize, usize)> {
    let base_dir = workspace.resolved_base_dir()?;
    let mut fetched = 0usize;
    let mut skipped = 0usize;

    for repo_name in repos {
        let repo_path = base_dir.join(repo_name);
        match fetch_one_repo(&repo_path, quiet, repo_name)? {
            FetchOutcome::Fetched => fetched += 1,
            FetchOutcome::MissingSkipped | FetchOutcome::Failed { .. } => skipped += 1,
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

/// Typed outcome of pulling a single repo. Used by both the batch
/// `pull_repos` aggregator and the per-repo `PullRepoJob` (jobs/pull_repo.rs)
/// so the two paths share one source of truth for the pull state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PullOutcome {
    /// Fast-forward succeeded; HEAD moved.
    Updated,
    /// Pull succeeded and reported nothing to merge.
    UpToDate,
    /// Working tree had uncommitted changes — pull was skipped.
    DirtySkipped,
    /// Path doesn't contain a `.git` entry — pull was skipped.
    MissingSkipped,
    /// `git pull` exited non-zero. Carries the trimmed stderr for surfacing.
    Failed { stderr: String },
}

impl PullOutcome {
    /// Fold a single outcome into the aggregate summary.
    pub(crate) fn fold_into(&self, summary: &mut PullSummary) {
        match self {
            PullOutcome::Updated => summary.updated += 1,
            PullOutcome::UpToDate => summary.up_to_date += 1,
            PullOutcome::DirtySkipped => summary.dirty_skipped += 1,
            PullOutcome::MissingSkipped => summary.missing_skipped += 1,
            PullOutcome::Failed { .. } => summary.failed += 1,
        }
    }
}

/// Capture HEAD's commit hash for before/after comparison. Returns
/// `None` if rev-parse fails (e.g. unborn branch) so the caller can
/// fall back to the textual heuristic on degenerate repos.
fn head_sha(repo_path: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

/// Pull one repo. The unit shared by the batch driver and the
/// per-repo Job wrapper — there is no other path that runs `git pull`.
///
/// Updated/UpToDate is distinguished by comparing HEAD before and
/// after the pull rather than parsing git's output. With `--quiet`
/// git emits nothing for either branch, so a textual heuristic
/// collapses both cases to "up to date" and the caller can't tell
/// when a real fast-forward landed.
pub(crate) fn pull_one_repo(repo_path: &Path, quiet: bool, repo_label: &str) -> Result<PullOutcome> {
    if !is_git_worktree(repo_path) {
        return Ok(PullOutcome::MissingSkipped);
    }

    if is_dirty(repo_path)? {
        if !quiet {
            println!("  dirty (skipped): {repo_label}");
        }
        return Ok(PullOutcome::DirtySkipped);
    }

    let before = head_sha(repo_path);

    let output = Command::new("git")
        .args(["pull", "--ff-only", "--quiet"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("running git pull in {repo_label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        eprintln!("  warning: pull failed for {repo_label}: {stderr}");
        return Ok(PullOutcome::Failed { stderr });
    }

    let after = head_sha(repo_path);
    if before == after {
        Ok(PullOutcome::UpToDate)
    } else {
        if !quiet {
            println!("  updated: {repo_label}");
        }
        Ok(PullOutcome::Updated)
    }
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
        let outcome = pull_one_repo(&repo_path, quiet, repo_name)?;
        outcome.fold_into(&mut summary);
    }

    Ok(summary)
}

/// Classify one expected-repo path as Missing/Dirty/Clean. Shared by
/// `check_status`'s batch loop and the per-repo `StatusRepoJob`
/// (jobs/status_repo.rs). Does NOT return `Unknown` — that is a
/// workspace-level designation (config says this path shouldn't be
/// here), not a property derivable from the path alone.
///
/// A directory without `.git` is classified as `Missing` rather than
/// being passed to `is_dirty`. The latter would walk up the filesystem
/// and surface a parent repo's state (e.g. a workspace-level flake),
/// which would mis-report this stub as dirty when it's just an empty
/// hole that `tend sync` should fill.
pub(crate) fn check_one_repo_status(repo_path: &Path) -> Result<RepoStatus> {
    if !is_git_worktree(repo_path) {
        Ok(RepoStatus::Missing)
    } else if is_stuck(repo_path)? {
        Ok(RepoStatus::Stuck)
    } else if is_dirty(repo_path)? {
        Ok(RepoStatus::Dirty)
    } else {
        Ok(RepoStatus::Clean)
    }
}

/// Returns true iff `path` is an existing directory containing a `.git`
/// entry (file or directory — supports both regular clones and worktrees).
///
/// A bare directory under the workspace without `.git` is treated as an
/// uncloned stub; tend must not call `git status` against it because git
/// would walk up the filesystem and confuse the caller with a parent repo's
/// state (e.g., a workspace-level flake repo).
fn is_git_worktree(path: &Path) -> bool {
    path.is_dir() && path.join(".git").exists()
}

fn is_dirty(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("checking git status in {}", repo_path.display()))?;

    Ok(!output.stdout.is_empty())
}

/// Returns true iff `repo_path` is mid rebase, merge, or cherry-pick.
///
/// Resolves the real git-dir via `git rev-parse --git-dir` rather than
/// assuming `<repo_path>/.git` is a directory — for a worktree, `.git` is a
/// file pointing at `<main-repo>/.git/worktrees/<name>/`, which is where
/// the marker files actually live. A repo mid-rebase/merge/cherry-pick can
/// strand real committed and uncommitted work under a conflict for weeks
/// with no other signal in `git status --porcelain` distinguishing it from
/// routine drift — see `feedback_git_hygiene_stuck_rebase_detection` for
/// the incident this guards against.
fn is_stuck(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("resolving git-dir in {}", repo_path.display()))?;

    if !output.status.success() {
        return Ok(false);
    }

    let git_dir_raw = String::from_utf8_lossy(&output.stdout);
    let git_dir = git_dir_raw.trim();
    let git_dir = if Path::new(git_dir).is_absolute() {
        std::path::PathBuf::from(git_dir)
    } else {
        repo_path.join(git_dir)
    };

    Ok(git_dir.join("rebase-merge").is_dir()
        || git_dir.join("rebase-apply").is_dir()
        || git_dir.join("MERGE_HEAD").is_file()
        || git_dir.join("CHERRY_PICK_HEAD").is_file()
        || git_dir.join("BISECT_LOG").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_status_display() {
        assert_eq!(RepoStatus::Clean.to_string(), "clean");
        assert_eq!(RepoStatus::Dirty.to_string(), "dirty");
        assert_eq!(RepoStatus::Stuck.to_string(), "stuck");
        assert_eq!(RepoStatus::Missing.to_string(), "missing");
        assert_eq!(RepoStatus::Unknown.to_string(), "unknown");
    }

    #[test]
    fn test_pull_summary_default() {
        let s = PullSummary::default();
        assert_eq!(s.updated, 0);
        assert_eq!(s.up_to_date, 0);
        assert_eq!(s.dirty_skipped, 0);
        assert_eq!(s.missing_skipped, 0);
        assert_eq!(s.failed, 0);
    }

    /// Regression: a clone stub (directory with files but no `.git`) must
    /// NOT be treated as a real worktree. Previously `check_status` would
    /// call `is_dirty` on such a stub, causing git to walk up to a parent
    /// repo and mis-report unrelated state.
    #[test]
    fn is_git_worktree_requires_dotgit() {
        let tmp = std::env::temp_dir().join(format!("tend-test-{}", std::process::id()));
        let stub = tmp.join("stub-repo");
        std::fs::create_dir_all(&stub).unwrap();
        std::fs::write(stub.join("README.md"), "hello").unwrap();

        assert!(!is_git_worktree(&stub), "stub without .git must not be a worktree");
        assert!(!is_git_worktree(&tmp.join("does-not-exist")), "missing dir must not be a worktree");

        // Create `.git` dir and re-check.
        std::fs::create_dir_all(stub.join(".git")).unwrap();
        assert!(is_git_worktree(&stub), "dir with .git must be a worktree");

        // And `.git` as a file (worktree pointer) also counts.
        let wt = tmp.join("worktree-repo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../real/.git/worktrees/wt\n").unwrap();
        assert!(is_git_worktree(&wt), "dir with .git file pointer must be a worktree");

        std::fs::remove_dir_all(&tmp).ok();
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git").args(args).current_dir(repo).status().unwrap();
        assert!(status.success(), "git {args:?} failed in {}", repo.display());
    }

    fn init_repo(repo: &Path) {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
    }

    fn write_commit(repo: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(repo.join(file), content).unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", msg]);
    }

    /// Regression guard for the incident this variant exists to catch: a
    /// repo mid rebase with a real conflict must report `Stuck`, not just
    /// `Dirty` — `Dirty` alone gave zero fleet-wide signal that
    /// `engenho-promessa-controllers` had a rebase abandoned for six weeks
    /// with recoverable work underneath it.
    #[test]
    fn mid_rebase_conflict_reports_stuck_not_just_dirty() {
        let tmp = std::env::temp_dir().join(format!("tend-stuck-test-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let repo = tmp.join("repo");
        init_repo(&repo);
        write_commit(&repo, "f.txt", "base\n", "base");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        write_commit(&repo, "f.txt", "feature change\n", "feature commit");
        git(&repo, &["checkout", "-q", "main"]);
        write_commit(&repo, "f.txt", "main change\n", "main commit");
        git(&repo, &["checkout", "-q", "feature"]);
        // This rebase conflicts by construction (both branches touched f.txt).
        let _ = Command::new("git").args(["rebase", "main"]).current_dir(&repo).output().unwrap();

        assert!(is_stuck(&repo).unwrap(), "mid-rebase-with-conflict must report stuck");
        assert_eq!(check_one_repo_status(&repo).unwrap(), RepoStatus::Stuck);

        // Abort cleanly and confirm the signal clears.
        git(&repo, &["rebase", "--abort"]);
        assert!(!is_stuck(&repo).unwrap(), "aborted rebase must clear the stuck signal");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
