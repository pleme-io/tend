//! `PrebuildRepoJob` — typed Job wrapping `prebuild::prebuild_one` for a
//! single repo. Per-repo (not per-cycle) so the scheduler controls
//! concurrency at the repo grain — the same shape as the other five tend
//! Jobs ([`PullRepoJob`][crate::jobs::pull_repo],
//! [`FetchRepoJob`][crate::jobs::fetch_repo], …). This is the sixth (and
//! final) instance of the wrapper pattern; it retires the last
//! hand-rolled orchestrator in tend — the `JoinSet`+`Semaphore` fan-out
//! that used to live in [`prebuild::run_cycle`].
//!
//! Unlike the lighter Jobs (which carry only `workspace`/`repo`/`path`),
//! a prebuild needs the whole per-cycle shared state to do its work:
//! the resolved options, the logged-in cache list, the per-cycle closure
//! dedup table, the audit log, the effectful `CacheFillEnv` seam, and
//! the persistent seen-rev cache. Each of those is a cheap-to-clone
//! `Arc`, so the Job is still pure data — no I/O at construction. The
//! scheduler's per-kind `BudgetSpec::max_concurrent` replaces the old
//! `Semaphore`; running `prebuild_one` inside `spawn_blocking` replaces
//! the old per-task `spawn_blocking` hop.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use shigoto_types::{JobScope, JobSubject, OutputSink, RecordingJob};
use thiserror::Error;

use crate::audit::AuditLog;
use crate::prebuild::{prebuild_one, BuildOutcome, PrebuildOptions, SeenCache};
use crate::prebuild_cache::{CacheFillEnv, CacheTarget, ClosureDedup};

/// Canonical kind id for every PrebuildRepoJob. Registered once across
/// the whole cycle — the scheduler distinguishes individual jobs by
/// their full `JobId` (scope + subject), not by kind alone.
pub(crate) const PREBUILD_REPO_KIND: &str = "tend.prebuild-repo";

/// One repo's prebuild within a single cycle. Carries the per-cycle
/// shared state by `Arc` so the closure that runs `prebuild_one` (and
/// the post-build seen-cache update) is self-contained inside
/// `spawn_blocking`.
#[derive(Clone)]
pub(crate) struct PrebuildRepoJob {
    /// Absolute path to the repo on disk. Pre-resolved so the Job is
    /// pure data and the build closure doesn't re-enumerate.
    pub repo: PathBuf,
    /// Repo name (the directory's leaf name). The JobSubject.
    pub repo_name: String,
    /// Logical workspace this repo belongs to. The JobScope namespace.
    pub workspace: String,
    /// The repo's previously-built rev (from the seen-cache snapshot).
    /// `None` ⇒ never built ⇒ always a candidate. `prebuild_one`
    /// short-circuits to `NoChange` when this equals HEAD.
    pub prev_rev: Option<String>,
    /// Resolved per-cycle options (selector/systems/repro/quiet/…).
    pub opts: Arc<PrebuildOptions>,
    /// The cycle's logged-in fan-out cache list.
    pub caches: Arc<Vec<CacheTarget>>,
    /// The cycle's shared closure-dedup table (one push per (cache,path)).
    pub dedup: Arc<Mutex<ClosureDedup>>,
    /// The cycle's audit log handle.
    pub audit: Arc<AuditLog>,
    /// The effectful seam (real nix/attic/git, or a mock in tests).
    pub env: Arc<dyn CacheFillEnv>,
    /// The persistent seen-rev cache. Updated in-place after a
    /// successful build so the next cycle skips this repo.
    pub seen: Arc<Mutex<SeenCache>>,
    /// On-disk path the seen-cache is persisted to.
    pub seen_path: PathBuf,
    /// Optional typed sink for the BuildOutcome. When set, `execute`
    /// records each successful outcome via `sink.record` so the cycle
    /// can drain a typed summary after the scheduler ticks. `None`
    /// drops outputs (the scheduler's default).
    output_sink: Option<Arc<dyn OutputSink<BuildOutcome>>>,
}

impl std::fmt::Debug for PrebuildRepoJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrebuildRepoJob")
            .field("repo", &self.repo)
            .field("repo_name", &self.repo_name)
            .field("workspace", &self.workspace)
            .field("prev_rev", &self.prev_rev)
            .field("caches", &self.caches.len())
            .field("seen_path", &self.seen_path)
            .field("output_sink", &self.output_sink.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

impl PrebuildRepoJob {
    /// Construct a Job from the per-cycle shared state. Every argument
    /// is a cheap-to-clone `Arc` (or owned small value), so this is pure
    /// data — no I/O. The sink is attached separately via
    /// [`with_output_sink`](Self::with_output_sink).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        repo: PathBuf,
        repo_name: impl Into<String>,
        workspace: impl Into<String>,
        prev_rev: Option<String>,
        opts: Arc<PrebuildOptions>,
        caches: Arc<Vec<CacheTarget>>,
        dedup: Arc<Mutex<ClosureDedup>>,
        audit: Arc<AuditLog>,
        env: Arc<dyn CacheFillEnv>,
        seen: Arc<Mutex<SeenCache>>,
        seen_path: PathBuf,
    ) -> Self {
        Self {
            repo,
            repo_name: repo_name.into(),
            workspace: workspace.into(),
            prev_rev,
            opts,
            caches,
            dedup,
            audit,
            env,
            seen,
            seen_path,
            output_sink: None,
        }
    }

    /// Attach a typed sink that will receive each successful
    /// `BuildOutcome`. Use `shigoto_emit::InMemorySink<BuildOutcome>`
    /// to collect a cycle summary.
    pub(crate) fn with_output_sink(mut self, sink: Arc<dyn OutputSink<BuildOutcome>>) -> Self {
        self.output_sink = Some(sink);
        self
    }
}

#[derive(Debug, Error)]
pub(crate) enum PrebuildRepoError {
    /// Couldn't run the prebuild machinery (the blocking task panicked
    /// or `prebuild_one` returned an `anyhow::Error` — e.g. git
    /// rev-parse failed, or a flake-show parse error). A soft skip
    /// (`BuildOutcome::NoDefault`) or a no-op (`BuildOutcome::NoChange`)
    /// is a *typed success* and flows through `Output`, not here.
    #[error("prebuild_one invocation failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl RecordingJob for PrebuildRepoJob {
    type Output = BuildOutcome;
    type Error = PrebuildRepoError;
    const KIND: &'static str = PREBUILD_REPO_KIND;

    fn scope(&self) -> JobScope {
        JobScope::Workspace(self.workspace.clone())
    }

    fn subject(&self) -> JobSubject {
        JobSubject::Repo(self.repo_name.clone())
    }

    fn output_sink(&self) -> Option<&Arc<dyn OutputSink<Self::Output>>> {
        self.output_sink.as_ref()
    }

    async fn execute_body(&self) -> Result<BuildOutcome, PrebuildRepoError> {
        // Clone the cheap Arc handles + owned shapes into the blocking
        // closure. `prebuild_one` is synchronous (it shells out to
        // nix/attic/git via the env seam), so it must run off the
        // reactor — same hop the legacy run_cycle task performed.
        let repo = self.repo.clone();
        let prev_rev = self.prev_rev.clone();
        let opts = Arc::clone(&self.opts);
        let caches = Arc::clone(&self.caches);
        let dedup = Arc::clone(&self.dedup);
        let audit = Arc::clone(&self.audit);
        let env = Arc::clone(&self.env);
        let seen = Arc::clone(&self.seen);
        let seen_path = self.seen_path.clone();

        tokio::task::spawn_blocking(move || {
            let outcome = prebuild_one(
                &*env,
                &repo,
                prev_rev.as_deref(),
                &opts,
                &caches,
                &dedup,
                &audit,
            )
            .map_err(|err| PrebuildRepoError::Invocation(err.to_string()))?;

            // Post-build seen-cache update — replicates run_cycle's
            // exact ordering/semantics. Only on `Built`: re-read HEAD
            // after the build so an upstream push that landed mid-build
            // isn't recorded against the stale rev; record it; persist
            // best-effort (a save failure must not fail the build).
            if let BuildOutcome::Built { .. } = &outcome {
                if let Ok(new_rev) = crate::prebuild::git_head_rev(&repo) {
                    let mut s = seen.lock().expect("seen cache mutex poisoned");
                    s.revs.insert(repo.display().to_string(), new_rev);
                    let _ = s.save_to(&seen_path);
                }
            }

            Ok(outcome)
        })
        .await
        .map_err(|join_err| PrebuildRepoError::Invocation(join_err.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prebuild::test_support::MinimalEnv;
    use crate::prebuild_cache::CacheFillEnv;
    use shigoto_types::{Job, JobKindId};
    use std::process::Command;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Assemble a job over `repo` with the supplied env + prev_rev. Empty
    /// caches keep the build off the network in every test path.
    fn job_over(
        env: Arc<dyn CacheFillEnv>,
        repo: PathBuf,
        prev_rev: Option<String>,
        seen_path: PathBuf,
    ) -> PrebuildRepoJob {
        let repo_name = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        PrebuildRepoJob::new(
            repo,
            repo_name,
            "ws",
            prev_rev,
            Arc::new(PrebuildOptions { quiet: true, ..Default::default() }),
            Arc::new(Vec::new()),
            Arc::new(Mutex::new(ClosureDedup::new())),
            Arc::new(AuditLog::default_path()),
            env,
            Arc::new(Mutex::new(SeenCache::default())),
            seen_path,
        )
    }

    #[tokio::test]
    async fn id_namespaces_by_workspace_and_repo() {
        let env: Arc<dyn CacheFillEnv> = Arc::new(MinimalEnv::default());
        let job = job_over(env, PathBuf::from("/tmp/x/name"), None, PathBuf::from("/tmp/seen"));
        let id = <PrebuildRepoJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(PREBUILD_REPO_KIND));
        assert_eq!(id.subject, JobSubject::Repo("name".to_string()));
        assert_eq!(id.scope, JobScope::Workspace("ws".to_string()));
    }

    /// When `prev_rev` already equals the repo's HEAD, `prebuild_one`
    /// short-circuits to `NoChange` without ever invoking the build
    /// seam. Uses a real tempdir git repo so HEAD is a genuine rev;
    /// `MinimalEnv` would `panic!` if `build` were reached, proving the
    /// no-build path.
    #[tokio::test]
    async fn no_change_when_prev_rev_matches_head() {
        let Some((dir, rev)) = init_one_commit_repo() else {
            eprintln!("git unavailable; skipping no_change test");
            return;
        };
        let env: Arc<dyn CacheFillEnv> = Arc::new(MinimalEnv::panic_on_build());
        let seen_path = dir.join("seen.json");
        let job = job_over(env, dir.clone(), Some(rev), seen_path);
        let outcome = job.execute().await.unwrap();
        assert_eq!(outcome, BuildOutcome::NoChange);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `prev_rev = None` forces the build path; the mock env returns a
    /// canonical out-path, so the outcome is `Built` and the seen-cache
    /// is updated to the repo's real HEAD inside the blocking closure.
    #[tokio::test]
    async fn builds_and_updates_seen_when_rev_moves() {
        let Some((dir, rev)) = init_one_commit_repo() else {
            eprintln!("git unavailable; skipping builds_and_updates_seen test");
            return;
        };
        let env: Arc<dyn CacheFillEnv> = Arc::new(MinimalEnv::default());
        let seen_path = dir.join("seen.json");
        let job = job_over(env, dir.clone(), None, seen_path.clone());
        let seen = Arc::clone(&job.seen);
        let outcome = job.execute().await.unwrap();
        match outcome {
            BuildOutcome::Built { pushed, .. } => {
                assert_eq!(pushed, 0, "empty caches → nothing pushed");
            }
            other => panic!("expected Built, got {other:?}"),
        }
        // The post-build seen-update recorded the repo's HEAD.
        let recorded = seen
            .lock()
            .unwrap()
            .revs
            .get(&dir.display().to_string())
            .cloned();
        assert_eq!(recorded.as_deref(), Some(rev.as_str()));
        assert!(seen_path.exists(), "seen cache persisted to disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Initialise a minimal git repo with one commit; return (dir,
    /// head_rev). Mirrors prebuild.rs's own test helper.
    fn init_one_commit_repo() -> Option<(PathBuf, String)> {
        let tmp = TempDir::new().ok()?;
        let dir = tmp.keep();
        let run = |args: &[&str]| -> bool {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .ok()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None;
        }
        let _ = run(&["config", "user.email", "tend-test@local"]);
        let _ = run(&["config", "user.name", "tend-test"]);
        let _ = run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README"), "hi").ok()?;
        if !run(&["add", "README"]) || !run(&["commit", "-q", "-m", "init"]) {
            return None;
        }
        let rev = crate::prebuild::git_head_rev(&dir).ok()?;
        Some((dir, rev))
    }
}
