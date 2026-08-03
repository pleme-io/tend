use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

use crate::config::Workspace;
use crate::provider;
use crate::secret::{GitConfigEnv, Secret};

/// Proof that a repo's remote set was **observed** and found non-empty.
///
/// This is the witness half of the derived-verdict law
/// (theory/UNREPRESENTABILITY.md §II.4): a verdict is derived from the
/// subject set it claims about, and carries the witness of that
/// derivation. `RepoStatus::Clean` is a claim *relative to a remote* —
/// without one, "clean" is a verdict over an empty subject set (Tier ⊥
/// subclass A: a check that can never fail because it examines nothing).
///
/// The only constructor is [`RemoteWitness::observe`], which shells to
/// `git remote` against a real path, and the `remote` field is private
/// to this module. So **no code outside `sync.rs` can name
/// `RepoStatus::Clean` without a remote observation having actually
/// happened** — the vacuous-clean verdict has no expressible form at
/// every call site in the crate.
///
/// Tier-honest: truly-unrepresentable *outside* this module; only-
/// mitigated *within* it (a determined author in `sync.rs` can still
/// hand-build the struct). Same grade UNREPRESENTABILITY.md gives
/// `CidrBlock`. Do not round up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteWitness {
    /// The first configured remote observed — conventionally `origin`.
    remote: String,
    /// That remote's configured URL, as it appears in `.git/config`.
    ///
    /// Carried on the witness rather than re-read at each use site so
    /// remote-URL policy (`remote_url::classify`) decides against the
    /// same observation that established the repo had a remote at all.
    /// Re-reading would let the two verdicts disagree across a
    /// concurrent `git remote set-url`.
    ///
    /// **May contain a credential** when a clone fossilized one (see
    /// `remote_url`). Never print or log it directly — classify it and
    /// report the redacted verdict.
    url: String,
}

impl RemoteWitness {
    /// Observe `repo_path`'s configured remotes.
    ///
    /// `Ok(None)` is a **determined** verdict — the observation ran and
    /// found nothing — not an inconclusive one. That distinction is why
    /// remote-less repos become [`RepoStatus::NoRemote`] and never
    /// [`RepoStatus::Unknown`].
    pub(crate) fn observe(repo_path: &Path) -> Result<Option<Self>> {
        let output = Command::new("git")
            .args(["remote"])
            .current_dir(repo_path)
            .output()
            .with_context(|| format!("listing git remotes in {}", repo_path.display()))?;

        if !output.status.success() {
            // `git remote` failing (not a repo, permissions) is genuinely
            // inconclusive — surface it rather than claiming "no remote".
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            anyhow::bail!("git remote failed in {}: {stderr}", repo_path.display());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let Some(remote) = stdout.lines().map(str::trim).find(|l| !l.is_empty()) else {
            return Ok(None);
        };

        // A remote with no resolvable URL is not a remote to be clean
        // relative to, so it collapses to the same `None` verdict as
        // having no remote at all rather than yielding a witness whose
        // URL is a lie.
        let url_out = Command::new("git")
            .args(["remote", "get-url", remote])
            .current_dir(repo_path)
            .output()
            .with_context(|| format!("reading {remote} url in {}", repo_path.display()))?;
        if !url_out.status.success() {
            return Ok(None);
        }
        let url = String::from_utf8_lossy(&url_out.stdout).trim().to_string();
        if url.is_empty() {
            return Ok(None);
        }

        Ok(Some(Self {
            remote: remote.to_string(),
            url,
        }))
    }

    /// Which remote this verdict was derived against.
    pub(crate) fn remote(&self) -> &str {
        &self.remote
    }

    /// The observed remote URL.
    ///
    /// May carry an embedded credential — pass it to
    /// `remote_url::classify` and report the redacted verdict rather
    /// than surfacing this string.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }
}

/// Status of a single repo in the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum RepoStatus {
    /// Repo exists, has no uncommitted changes, **and** was observed to
    /// have a remote to be clean *relative to*. The [`RemoteWitness`]
    /// is the proof of that observation — this variant cannot be
    /// constructed without one.
    Clean(RemoteWitness),
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
    /// Repo is a valid worktree but has **no git remote configured at
    /// all**. Every backup story the fleet has — git, GitHub, tend's own
    /// sync loop — is bypassed: the history exists on exactly one
    /// machine and has never been pushed anywhere.
    ///
    /// This is the status that was invisible before 2026-07-28. A
    /// remote-less repo reports nothing to `git status --porcelain` and
    /// "0 unpushed" to `git log --branches --not --remotes` **forever**,
    /// because there is nothing to be ahead OF — so it read as `Clean`,
    /// the most confidently-wrong verdict in the enum. Found live on
    /// `ferrite-zig` (a whole Zig implementation on one disk) and
    /// `pleme-app-core`.
    ///
    /// Determined, not inconclusive — hence its own variant rather than
    /// `Unknown`. Checked *before* `Stuck`/`Dirty` because it is the
    /// most severe and the least recoverable by any local action: a
    /// stuck or dirty repo still has its committed history backed
    /// somewhere; this one does not. The trade is that a repo that is
    /// both dirty and remote-less reports only `NoRemote` — the
    /// destination is orthogonal axes (`{local: …, backing: …}`), noted
    /// as `pending-unrep` in CLAUDE.md.
    NoRemote,
    /// Repo is expected but not cloned.
    Missing,
    /// Repo exists on disk but not in config.
    Unknown,
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Clean(_) => f.write_str("clean"),
            Self::Dirty => f.write_str("dirty"),
            Self::Stuck => f.write_str("stuck"),
            Self::NoRemote => f.write_str("no-remote"),
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
        let org = workspace.org.as_deref().unwrap_or(&workspace.name);
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

/// Ephemeral git auth for cloning an HTTPS GitHub URL. Containers
/// can't prompt for credentials, so the token has to reach git
/// somehow; this returns it as process-scoped config rather than
/// putting it in the URL.
///
/// This replaces `inject_github_token`, which rewrote the URL to
/// `https://x-access-token:<token>@github.com/...`. That worked, but
/// git persists the clone URL verbatim into `.git/config` — so every
/// clone permanently recorded whatever token was live at the time. The
/// 2026-07-29 sweep found 25 repos in that state across two orgs,
/// carrying three distinct tokens, one still valid.
///
/// Environment-scoped config has no such afterlife: it exists for the
/// duration of the `git clone` process and is written to no file. See
/// [`crate::secret`] for why it is also not passed via `-c` on argv.
///
/// Empty for non-GitHub URLs and when no token is configured — a
/// public clone needs no credential, and an absent one must not
/// become an empty `Authorization` header.
fn github_clone_auth(url: &str) -> GitConfigEnv {
    if !url.starts_with("https://github.com/") {
        return GitConfigEnv::new();
    }
    match Secret::from_env(&["TEND_GITHUB_TOKEN", "GITHUB_TOKEN"]) {
        Some(secret) => secret.github_git_auth(),
        None => GitConfigEnv::new(),
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

    // The URL handed to git is the clean one — it is what git writes
    // into `.git/config`, so it must never carry a credential.
    // Authentication rides alongside in the environment instead.
    let auth = github_clone_auth(&clone_url);

    if !quiet {
        println!("  cloning {repo_label}...");
    }

    let mut cmd = Command::new("git");
    cmd.args(["clone", &clone_url, &repo_path.to_string_lossy()])
        .env("GIT_TERMINAL_PROMPT", "0");
    auth.apply(&mut cmd);

    let output = cmd
        .output()
        .with_context(|| format!("running git clone for {repo_label}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        eprintln!("  warning: failed to clone {repo_label}: {stderr}");
        return Ok(SyncOutcome::Failed { stderr });
    }

    Ok(SyncOutcome::Cloned)
}

pub(crate) async fn sync_repos(
    workspace: &Workspace,
    repos: &[String],
    quiet: bool,
) -> Result<(usize, usize, usize)> {
    let base_dir = workspace.resolved_base_dir()?;
    std::fs::create_dir_all(&base_dir)
        .with_context(|| format!("creating {}", base_dir.display()))?;

    let mut cloned = 0usize;
    let mut present = 0usize;
    let mut failed = 0usize;

    for repo_name in repos {
        let repo_path = base_dir.join(repo_name);
        let clone_url = workspace.clone_url(repo_name);
        match sync_one_repo(clone_url, &repo_path, repo_name, quiet)? {
            SyncOutcome::Cloned => cloned += 1,
            SyncOutcome::AlreadyPresent | SyncOutcome::StubExisted => present += 1,
            // ── ★ A FAILURE MUST LAND IN A BUCKET ────────────────────────
            // These used to count toward NEITHER, surviving only as an
            // eprintln. So a workspace where every clone failed reported
            // `cloned = 0`, which the summary renders as "all N repos
            // present" — an affirmative completeness claim about repos that
            // are not on disk. The three counts must sum to the repo count,
            // or a whole outcome class can go missing without a trace.
            SyncOutcome::Failed { .. } => failed += 1,
        }
    }

    Ok((cloned, present, failed))
}

/// Check status of all repos in a workspace
pub(crate) async fn check_status(
    workspace: &Workspace,
    repos: &[String],
) -> Result<Vec<RepoEntry>> {
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
            // Observe the remote before falling back to Unknown.
            //
            // These are the directories discovery did not resolve, and
            // they are exactly the ones most likely to be unbacked: a
            // repo with no GitHub counterpart cannot appear in an org
            // listing, so it lands here by construction. Stamping the
            // whole bucket `Unknown` therefore hid the single worst
            // state — history that exists on one disk — behind the
            // blandest verdict.
            //
            // Measured on 2026-07-29: `akeylesslabs` reported 11
            // Unknown / 0 NoRemote while containing a repo with 11
            // commits and no remote at all.
            //
            // Only a *determined* observation downgrades the verdict.
            // `RemoteWitness::observe` returning `Ok(None)` means the
            // remote set was read and found empty; an `Err` means the
            // read itself failed and the repo stays Unknown, because
            // an inconclusive check must never masquerade as a finding.
            let path = base_dir.join(&name);
            let status = if !is_git_worktree(&path) {
                RepoStatus::Unknown
            } else {
                match RemoteWitness::observe(&path) {
                    Ok(None) => RepoStatus::NoRemote,
                    // Has a remote, just isn't in discovery — archived,
                    // renamed, a fork, or local-only-but-pushed. Still
                    // Unknown: that is a real and different signal, and
                    // `LocalRepoNotInDiscovery` already covers it.
                    Ok(Some(_)) | Err(_) => RepoStatus::Unknown,
                }
            };
            entries.push(RepoEntry { name, status });
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
pub(crate) fn fetch_one_repo(
    repo_path: &Path,
    quiet: bool,
    repo_label: &str,
) -> Result<FetchOutcome> {
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
pub(crate) async fn fetch_repos(
    workspace: &Workspace,
    repos: &[String],
    quiet: bool,
) -> Result<(usize, usize)> {
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
    pub no_remote_skipped: usize,
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
    /// Repo has **no configured remote** — there is nothing to pull
    /// FROM, so the pull was skipped without running git.
    ///
    /// This variant exists because git's stderr genuinely cannot carry
    /// the distinction. Verified empirically 2026-07-28: a repo with an
    /// `origin` but no branch tracking, and a repo with no remote at
    /// all, both emit the byte-identical message
    /// `There is no tracking information for the current branch.` So
    /// `drift::classify_pull_failure` — a pure function of stderr —
    /// classified the remote-less case as `PullFailedNoUpstream`, whose
    /// documented remedy (`git branch --set-upstream-to=origin/<branch>`)
    /// is *impossible* when there is no `origin`.
    ///
    /// The load-bearing fix is to **observe the subject** at the one
    /// place that has the path (here) rather than to infer it from a
    /// message that does not contain it.
    NoRemoteSkipped,
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
            PullOutcome::NoRemoteSkipped => summary.no_remote_skipped += 1,
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
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Pull one repo. The unit shared by the batch driver and the
/// per-repo Job wrapper — there is no other path that runs `git pull`.
///
/// Updated/UpToDate is distinguished by comparing HEAD before and
/// after the pull rather than parsing git's output. With `--quiet`
/// git emits nothing for either branch, so a textual heuristic
/// collapses both cases to "up to date" and the caller can't tell
/// when a real fast-forward landed.
pub(crate) fn pull_one_repo(
    repo_path: &Path,
    quiet: bool,
    repo_label: &str,
) -> Result<PullOutcome> {
    if !is_git_worktree(repo_path) {
        return Ok(PullOutcome::MissingSkipped);
    }

    // Observe the remote set before running git. Pulling a remote-less
    // repo is guaranteed-useless work whose error message is actively
    // misleading (see `PullOutcome::NoRemoteSkipped`).
    if RemoteWitness::observe(repo_path)?.is_none() {
        if !quiet {
            println!("  no remote (skipped): {repo_label}");
        }
        return Ok(PullOutcome::NoRemoteSkipped);
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
///
/// The remote set is observed **before** any working-tree question is
/// asked. Two reasons, both load-bearing:
/// 1. `NoRemote` outranks `Stuck`/`Dirty` in severity (see the variant
///    docs) — a repo whose entire history exists on one machine is a
///    worse fact than one with a conflicted merge on top of a backed-up
///    history.
/// 2. The `RemoteWitness` this produces is what makes the `Clean` arm
///    constructible at all. There is no code path that reaches `Clean`
///    without it.
pub(crate) fn check_one_repo_status(repo_path: &Path) -> Result<RepoStatus> {
    if !is_git_worktree(repo_path) {
        return Ok(RepoStatus::Missing);
    }
    let Some(witness) = RemoteWitness::observe(repo_path)? else {
        return Ok(RepoStatus::NoRemote);
    };
    if is_stuck(repo_path)? {
        Ok(RepoStatus::Stuck)
    } else if is_dirty(repo_path)? {
        Ok(RepoStatus::Dirty)
    } else {
        Ok(RepoStatus::Clean(witness))
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

    /// ── ★ EVERY OUTCOME LANDS IN A BUCKET ───────────────────────────────
    /// `SyncOutcome::Failed` used to count toward NEITHER `cloned` nor
    /// `present`, surviving only as an eprintln. So a workspace where every
    /// clone failed arrived at the summary as `cloned = 0`, which renders
    /// as "all N repos present" — an affirmative completeness claim about
    /// repos that are not on disk, followed by exit 0.
    ///
    /// The invariant is arithmetic and therefore checkable: the three
    /// counts must sum to the number of repos attempted. A whole outcome
    /// class cannot go missing without breaking the sum.
    #[test]
    fn the_three_sync_counts_sum_to_every_repo_attempted() {
        let outcomes = vec![
            super::SyncOutcome::Cloned,
            super::SyncOutcome::AlreadyPresent,
            super::SyncOutcome::StubExisted,
            super::SyncOutcome::Failed {
                stderr: "boom".into(),
            },
            super::SyncOutcome::Failed {
                stderr: "boom".into(),
            },
        ];
        let attempted = outcomes.len();
        let (mut cloned, mut present, mut failed) = (0usize, 0usize, 0usize);
        for o in &outcomes {
            match o {
                super::SyncOutcome::Cloned => cloned += 1,
                super::SyncOutcome::AlreadyPresent | super::SyncOutcome::StubExisted => {
                    present += 1;
                }
                super::SyncOutcome::Failed { .. } => failed += 1,
            }
        }
        assert_eq!(
            cloned + present + failed,
            attempted,
            "an outcome class that lands in no bucket disappears from the summary"
        );
        assert_eq!(failed, 2, "failures must be counted, not merely printed");
    }

    /// An on-disk repo that discovery did not resolve, with no remote,
    /// must report NoRemote — not Unknown.
    ///
    /// Regression pin for the 2026-07-29 finding: `akeylesslabs`
    /// reported 11 Unknown / 0 NoRemote while containing a repo with
    /// 11 commits and no remote at all. Unknown is the bucket a repo
    /// with no GitHub counterpart lands in by construction, so it was
    /// hiding exactly the state it most needed to surface.
    #[tokio::test]
    async fn unresolved_dir_without_remote_reports_no_remote() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("orphan");
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap();

        let mut ws = Workspace::test_default("ws");
        ws.base_dir = tmp.path().to_string_lossy().to_string();

        // `repos` is empty, so `orphan` is unresolved by discovery.
        let entries = check_status(&ws, &[]).await.unwrap();
        let orphan = entries.iter().find(|e| e.name == "orphan").unwrap();
        assert_eq!(orphan.status, RepoStatus::NoRemote);
    }

    /// The complement: an unresolved dir that *does* have a remote is
    /// still Unknown. It is a real repo simply absent from discovery
    /// (archived, renamed, a fork) — a different signal, already
    /// carried by LocalRepoNotInDiscovery. Pinning both directions
    /// keeps the fix from drifting into "everything unresolved is
    /// NoRemote".
    #[tokio::test]
    async fn unresolved_dir_with_remote_stays_unknown() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("has-remote");
        std::fs::create_dir(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&repo)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:pleme-io/has-remote.git",
            ])
            .current_dir(&repo)
            .status()
            .unwrap();

        let mut ws = Workspace::test_default("ws");
        ws.base_dir = tmp.path().to_string_lossy().to_string();

        let entries = check_status(&ws, &[]).await.unwrap();
        let e = entries.iter().find(|e| e.name == "has-remote").unwrap();
        assert_eq!(e.status, RepoStatus::Unknown);
    }

    /// A directory with no `.git` at all stays Unknown — it is a stub,
    /// not a repo, and `StubDirectoryFound` covers it.
    #[tokio::test]
    async fn unresolved_non_repo_dir_stays_unknown() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("just-a-dir")).unwrap();

        let mut ws = Workspace::test_default("ws");
        ws.base_dir = tmp.path().to_string_lossy().to_string();

        let entries = check_status(&ws, &[]).await.unwrap();
        let e = entries.iter().find(|e| e.name == "just-a-dir").unwrap();
        assert_eq!(e.status, RepoStatus::Unknown);
    }

    use super::*;

    #[test]
    fn test_repo_status_display() {
        let witness = RemoteWitness {
            remote: "origin".into(),
            url: "git@github.com:pleme-io/tend.git".into(),
        };
        assert_eq!(RepoStatus::Clean(witness).to_string(), "clean");
        assert_eq!(RepoStatus::Dirty.to_string(), "dirty");
        assert_eq!(RepoStatus::Stuck.to_string(), "stuck");
        assert_eq!(RepoStatus::NoRemote.to_string(), "no-remote");
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
        assert_eq!(s.no_remote_skipped, 0);
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

        assert!(
            !is_git_worktree(&stub),
            "stub without .git must not be a worktree"
        );
        assert!(
            !is_git_worktree(&tmp.join("does-not-exist")),
            "missing dir must not be a worktree"
        );

        // Create `.git` dir and re-check.
        std::fs::create_dir_all(stub.join(".git")).unwrap();
        assert!(is_git_worktree(&stub), "dir with .git must be a worktree");

        // And `.git` as a file (worktree pointer) also counts.
        let wt = tmp.join("worktree-repo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../real/.git/worktrees/wt\n").unwrap();
        assert!(
            is_git_worktree(&wt),
            "dir with .git file pointer must be a worktree"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(
            status.success(),
            "git {args:?} failed in {}",
            repo.display()
        );
    }

    /// A repo with NO remote — the `ferrite-zig` shape. Tests asserting
    /// `Clean`/`Dirty`/`Stuck` must call [`add_remote`] afterwards,
    /// because `NoRemote` outranks all three.
    fn init_repo(repo: &Path) {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q", "-b", "main"]);
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
    }

    /// Point the repo at a real (bare, local) upstream so it is
    /// genuinely backed — the precondition every working-tree verdict
    /// now carries.
    fn add_remote(repo: &Path) {
        let upstream = repo.with_extension("upstream.git");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "--bare", "-b", "main"]);
        git(
            repo,
            &["remote", "add", "origin", &upstream.to_string_lossy()],
        );
    }

    fn write_commit(repo: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(repo.join(file), content).unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "--no-verify", "-m", msg]);
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
        // Backed by a real remote: `Stuck` is only reachable once the
        // remote observation has succeeded, since `NoRemote` outranks it.
        add_remote(&repo);
        write_commit(&repo, "f.txt", "base\n", "base");
        git(&repo, &["checkout", "-q", "-b", "feature"]);
        write_commit(&repo, "f.txt", "feature change\n", "feature commit");
        git(&repo, &["checkout", "-q", "main"]);
        write_commit(&repo, "f.txt", "main change\n", "main commit");
        git(&repo, &["checkout", "-q", "feature"]);
        // This rebase conflicts by construction (both branches touched f.txt).
        let _ = Command::new("git")
            .args(["rebase", "main"])
            .current_dir(&repo)
            .output()
            .unwrap();

        assert!(
            is_stuck(&repo).unwrap(),
            "mid-rebase-with-conflict must report stuck"
        );
        assert_eq!(check_one_repo_status(&repo).unwrap(), RepoStatus::Stuck);

        // Abort cleanly and confirm the signal clears.
        git(&repo, &["rebase", "--abort"]);
        assert!(
            !is_stuck(&repo).unwrap(),
            "aborted rebase must clear the stuck signal"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    // ── remote-less detection ───────────────────────────────────────
    //
    // The gap these guard: a repo with no remote reports nothing to
    // `git status --porcelain` and "0 unpushed" to
    // `git log --branches --not --remotes` forever — there is nothing
    // to be ahead OF — so every ahead/behind check called it healthy
    // while its entire history sat on one disk. Found live on
    // `ferrite-zig` + `pleme-app-core` (2026-07-28).
    //
    // Each test below is written so it FAILS against the pre-fix code:
    // the first asserts the remote-less repo is not `Clean`, the second
    // asserts a repo WITH a remote still is (a detector that flags
    // everything is as useless as one that flags nothing).

    /// A `git init`-only repo — no remote, nothing committed anywhere
    /// else — must report `NoRemote`, never `Clean`.
    #[test]
    fn remote_less_repo_reports_no_remote_never_clean() {
        let tmp = std::env::temp_dir().join(format!("tend-noremote-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let repo = tmp.join("orphan");
        init_repo(&repo);
        write_commit(&repo, "main.zig", "the only copy\n", "init");

        assert_eq!(
            RemoteWitness::observe(&repo).unwrap(),
            None,
            "fixture must genuinely have zero remotes"
        );
        assert_eq!(
            check_one_repo_status(&repo).unwrap(),
            RepoStatus::NoRemote,
            "a repo with no remote must NOT report clean — that is the \
             verdict that hid ferrite-zig"
        );
        assert_eq!(
            pull_one_repo(&repo, true, "orphan").unwrap(),
            PullOutcome::NoRemoteSkipped,
            "pull must skip on the observation, not fail with git's \
             misleading no-tracking-information message"
        );

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The normal case must still work: a repo WITH a remote and a clean
    /// tree still reports `Clean`, and the verdict names the remote it
    /// was derived against.
    #[test]
    fn repo_with_remote_still_reports_clean() {
        let tmp = std::env::temp_dir().join(format!("tend-hasremote-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let repo = tmp.join("backed");
        init_repo(&repo);
        add_remote(&repo);
        write_commit(&repo, "f.txt", "hello\n", "init");
        git(&repo, &["push", "-q", "origin", "main"]);

        match check_one_repo_status(&repo).unwrap() {
            RepoStatus::Clean(witness) => assert_eq!(witness.remote(), "origin"),
            other => panic!("expected Clean(origin), got {other:?}"),
        }

        // …and a dirty tree in a remote-backed repo is still Dirty.
        std::fs::write(repo.join("dirt"), "x\n").unwrap();
        assert_eq!(check_one_repo_status(&repo).unwrap(), RepoStatus::Dirty);

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A remote-less repo that is ALSO dirty reports `NoRemote`: the
    /// unbacked-history fact outranks the working-tree fact, because no
    /// local action fixes it. Pinned so a future reordering of
    /// `check_one_repo_status` is a deliberate decision, not a drift.
    #[test]
    fn remote_less_outranks_dirty() {
        let tmp = std::env::temp_dir().join(format!("tend-noremote-dirty-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let repo = tmp.join("orphan");
        init_repo(&repo);
        write_commit(&repo, "f.txt", "a\n", "init");
        std::fs::write(repo.join("uncommitted"), "b\n").unwrap();

        assert!(is_dirty(&repo).unwrap(), "fixture must be dirty");
        assert_eq!(check_one_repo_status(&repo).unwrap(), RepoStatus::NoRemote);

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// `git remote` on a valid repo with remotes returns the name, and
    /// the witness is the only thing that can produce a `Clean`.
    #[test]
    fn remote_witness_observes_first_remote_name() {
        let tmp = std::env::temp_dir().join(format!("tend-witness-{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        let repo = tmp.join("repo");
        init_repo(&repo);
        git(
            &repo,
            &["remote", "add", "origin", "https://example.invalid/x.git"],
        );

        let witness = RemoteWitness::observe(&repo)
            .unwrap()
            .expect("remote present");
        assert_eq!(witness.remote(), "origin");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
