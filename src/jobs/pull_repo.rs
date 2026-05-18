//! `PullRepoJob` — typed Job wrapping `sync::pull_one_repo` for a single
//! repo. Per-repo (not per-workspace) so the scheduler controls
//! concurrency at the repo grain — the natural unit of work for
//! fast-forward pulls, which are independent across repos.
//!
//! The Job's `execute()` runs the existing `pull_one_repo` helper and
//! lifts its `PullOutcome` into the Job's `Output`. Errors only flow
//! through the `Error` channel for I/O failures the helper itself
//! couldn't classify (e.g. failure to spawn `git`); a `Failed { stderr }`
//! outcome from git is a typed success-with-failure-outcome, not an
//! Error — that distinction lets the scheduler decide retries from the
//! Output rather than conflating "couldn't run git at all" with
//! "git ran and rejected the fast-forward."

use std::path::PathBuf;

use async_trait::async_trait;
use shigoto_types::{Job, JobId, JobKindId, JobScope, JobSubject};
use thiserror::Error;

use crate::sync::{pull_one_repo, PullOutcome};

/// Canonical kind id for every PullRepoJob. Tend may register the same
/// kind across all workspaces — the scheduler distinguishes individual
/// jobs by their full `JobId` (scope + subject), not by kind alone.
pub(crate) const PULL_REPO_KIND: &str = "tend.pull-repo";

/// One repo's pull. Captures the workspace label (for scope) and the
/// absolute path on disk (so the Job is self-contained and doesn't have
/// to re-resolve `base_dir` on each tick).
#[derive(Debug, Clone)]
pub(crate) struct PullRepoJob {
    /// Logical workspace this repo belongs to. Used as the JobScope
    /// namespace so a single scheduler can carry repos from multiple
    /// workspaces without subject collisions.
    pub workspace: String,
    /// Repo name (the directory's leaf name). The JobSubject.
    pub repo_name: String,
    /// Absolute path on disk. Pre-resolved so the Job is pure data
    /// (no I/O in the constructor) and the scheduler can serialize it
    /// later if/when we cross the in-memory boundary.
    pub repo_path: PathBuf,
    /// Suppress the inline "updated:" / "dirty (skipped):" prints. The
    /// scheduler-driven path typically wants quiet=true since the
    /// `TransitionEmitter` carries the audit trail.
    pub quiet: bool,
}

impl PullRepoJob {
    pub(crate) fn new(
        workspace: impl Into<String>,
        repo_name: impl Into<String>,
        repo_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            repo_name: repo_name.into(),
            repo_path: repo_path.into(),
            quiet: true,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum PullRepoError {
    /// Couldn't even invoke the pull machinery (e.g. spawning git
    /// failed, or the workspace path couldn't be canonicalized).
    /// Distinct from a `PullOutcome::Failed` which is a *typed
    /// success* — the helper ran git and got a non-zero exit.
    #[error("pull_one_repo invocation failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl Job for PullRepoJob {
    type Output = PullOutcome;
    type Error = PullRepoError;

    fn id(&self) -> JobId {
        JobId {
            scope: JobScope::Workspace(self.workspace.clone()),
            kind: JobKindId::new(PULL_REPO_KIND),
            subject: JobSubject::Repo(self.repo_name.clone()),
        }
    }

    fn kind(&self) -> JobKindId {
        JobKindId::new(PULL_REPO_KIND)
    }

    async fn execute(&self) -> Result<PullOutcome, PullRepoError> {
        let path = self.repo_path.clone();
        let quiet = self.quiet;
        let label = self.repo_name.clone();
        // `pull_one_repo` is sync (it spawns git via std::process::Command).
        // Hop to a blocking thread so we don't stall the tokio runtime if
        // a large monorepo's git pull happens to take a moment.
        tokio::task::spawn_blocking(move || pull_one_repo(&path, quiet, &label))
            .await
            .map_err(|join_err| PullRepoError::Invocation(format!("join error: {join_err}")))?
            .map_err(|err| PullRepoError::Invocation(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Stand up a one-commit git repo at `path`. Used as the upstream
    /// of every test repo; we clone from it so the local repo has a
    /// real `origin` and `git pull --ff-only` is well-defined.
    fn init_upstream(path: &std::path::Path) {
        Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "commit.gpgsign", "false"]).current_dir(path).status().unwrap();
        std::fs::write(path.join("file"), "one\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(path).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "one"]).current_dir(path).status().unwrap();
    }

    /// Clone `upstream` into `dest`. Same identity config so commits
    /// made downstream of this would be valid.
    fn clone_from(upstream: &std::path::Path, dest: &std::path::Path) {
        Command::new("git")
            .args(["clone", "-q", upstream.to_str().unwrap(), dest.to_str().unwrap()])
            .status()
            .unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(dest).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(dest).status().unwrap();
        Command::new("git").args(["config", "commit.gpgsign", "false"]).current_dir(dest).status().unwrap();
    }

    #[tokio::test]
    async fn id_namespaces_by_workspace_and_repo() {
        let job = PullRepoJob::new("pleme-io", "tend", "/tmp/x");
        let id = <PullRepoJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(PULL_REPO_KIND));
        match id.scope {
            JobScope::Workspace(ref w) => assert_eq!(w, "pleme-io"),
            _ => panic!("expected Workspace scope"),
        }
        match id.subject {
            JobSubject::Repo(ref r) => assert_eq!(r, "tend"),
            _ => panic!("expected Repo subject"),
        }
    }

    #[tokio::test]
    async fn missing_repo_reports_missing_skipped() {
        let tmp = TempDir::new().unwrap();
        let job = PullRepoJob::new("test-ws", "does-not-exist", tmp.path().join("does-not-exist"));
        let out = job.execute().await.unwrap();
        assert_eq!(out, PullOutcome::MissingSkipped);
    }

    #[tokio::test]
    async fn clean_repo_with_no_upstream_changes_is_up_to_date() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream.git");
        let local = tmp.path().join("local");
        std::fs::create_dir(&upstream).unwrap();
        init_upstream(&upstream);
        clone_from(&upstream, &local);

        let job = PullRepoJob::new("test-ws", "local", local.clone());
        let out = job.execute().await.unwrap();
        assert_eq!(out, PullOutcome::UpToDate);
    }

    #[tokio::test]
    async fn upstream_advance_yields_updated() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        let local = tmp.path().join("local");
        std::fs::create_dir(&upstream).unwrap();
        init_upstream(&upstream);
        clone_from(&upstream, &local);

        // Advance upstream by one commit.
        std::fs::write(upstream.join("file"), "two\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&upstream).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "two"]).current_dir(&upstream).status().unwrap();

        let job = PullRepoJob::new("test-ws", "local", local.clone());
        let out = job.execute().await.unwrap();
        assert_eq!(out, PullOutcome::Updated);
    }

    #[tokio::test]
    async fn dirty_repo_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        let local = tmp.path().join("local");
        std::fs::create_dir(&upstream).unwrap();
        init_upstream(&upstream);
        clone_from(&upstream, &local);

        // Make local dirty.
        std::fs::write(local.join("dirty"), "dirty\n").unwrap();

        let job = PullRepoJob::new("test-ws", "local", local.clone());
        let out = job.execute().await.unwrap();
        assert_eq!(out, PullOutcome::DirtySkipped);
    }
}
