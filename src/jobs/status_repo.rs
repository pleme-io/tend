//! `StatusRepoJob` — read-only classification of one repo as
//! Missing/Dirty/Clean. Wraps `sync::check_one_repo_status`. The
//! cheapest Job in the catalogue: no network, no mutations, no
//! locks. Used both as a building block for typed reconcile loops and
//! as the canonical input to `shigoto_test::idempotence_quickcheck`
//! (M0.9p) — running it twice on the same path with no intervening
//! mutation must yield the same status.
//!
//! Same wrapper shape as [`PullRepoJob`][crate::jobs::pull_repo]:
//! the Job is pure data (`workspace`, `repo_name`, `repo_path`),
//! `execute()` defers to a sync.rs helper on a blocking thread, and
//! the Job's `Output` is the typed enum returned by the helper.
//! This is the third instance of the pattern — at the next one, the
//! shape is worth extracting into a macro.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use shigoto_types::{JobScope, JobSubject, OutputSink, RecordingJob};
use thiserror::Error;

use crate::sync::{check_one_repo_status, RepoStatus};

/// Canonical kind id for every StatusRepoJob.
pub(crate) const STATUS_REPO_KIND: &str = "tend.status-repo";

#[derive(Clone)]
pub(crate) struct StatusRepoJob {
    pub workspace: String,
    pub repo_name: String,
    pub repo_path: PathBuf,
    output_sink: Option<Arc<dyn OutputSink<RepoStatus>>>,
}

impl std::fmt::Debug for StatusRepoJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusRepoJob")
            .field("workspace", &self.workspace)
            .field("repo_name", &self.repo_name)
            .field("repo_path", &self.repo_path)
            .field("output_sink", &self.output_sink.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

impl StatusRepoJob {
    pub(crate) fn new(
        workspace: impl Into<String>,
        repo_name: impl Into<String>,
        repo_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            repo_name: repo_name.into(),
            repo_path: repo_path.into(),
            output_sink: None,
        }
    }

    pub(crate) fn with_output_sink(mut self, sink: Arc<dyn OutputSink<RepoStatus>>) -> Self {
        self.output_sink = Some(sink);
        self
    }
}

#[derive(Debug, Error)]
pub(crate) enum StatusRepoError {
    #[error("check_one_repo_status invocation failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl RecordingJob for StatusRepoJob {
    type Output = RepoStatus;
    type Error = StatusRepoError;
    const KIND: &'static str = STATUS_REPO_KIND;

    fn scope(&self) -> JobScope {
        JobScope::Workspace(self.workspace.clone())
    }

    fn subject(&self) -> JobSubject {
        JobSubject::Repo(self.repo_name.clone())
    }

    fn output_sink(&self) -> Option<&Arc<dyn OutputSink<Self::Output>>> {
        self.output_sink.as_ref()
    }

    async fn execute_body(&self) -> Result<RepoStatus, StatusRepoError> {
        let path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || check_one_repo_status(&path))
            .await
            .map_err(|join_err| StatusRepoError::Invocation(format!("join error: {join_err}")))?
            .map_err(|err| StatusRepoError::Invocation(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::{Job, JobKindId};
    use std::process::Command;
    use tempfile::TempDir;

    /// A repo with NO remote. This is deliberately the bare shape —
    /// `git init` + one commit — because it is exactly the `ferrite-zig`
    /// case: a history that exists on one machine and has never been
    /// pushed. Before 2026-07-28 this fixture reported `Clean`, which is
    /// how the whole detection gap survived in a green test suite.
    fn init_repo_without_remote(path: &std::path::Path) {
        Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "commit.gpgsign", "false"]).current_dir(path).status().unwrap();
        std::fs::write(path.join("file"), "x\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(path).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(path).status().unwrap();
    }

    /// The realistic fixture: a repo that is actually backed by a
    /// remote. Anything asserting `Clean`/`Dirty` must use this — those
    /// verdicts are only reachable once a remote has been observed.
    fn init_repo(path: &std::path::Path) {
        init_repo_without_remote(path);
        let upstream = path.join("..").join("upstream.git");
        std::fs::create_dir_all(&upstream).unwrap();
        Command::new("git").args(["init", "-q", "--bare", "-b", "main"]).current_dir(&upstream).status().unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", &upstream.to_string_lossy()])
            .current_dir(path)
            .status()
            .unwrap();
    }

    #[tokio::test]
    async fn id_namespaces_by_workspace_and_repo() {
        let job = StatusRepoJob::new("ws", "name", "/tmp/x");
        let id = <StatusRepoJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(STATUS_REPO_KIND));
    }

    #[tokio::test]
    async fn missing_repo_returns_missing() {
        let tmp = TempDir::new().unwrap();
        let job = StatusRepoJob::new("ws", "missing", tmp.path().join("missing"));
        let status = job.execute().await.unwrap();
        assert_eq!(status, RepoStatus::Missing);
    }

    #[tokio::test]
    async fn clean_repo_returns_clean() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let job = StatusRepoJob::new("ws", "repo", repo);
        let status = job.execute().await.unwrap();
        // NOTE: `matches!`, not `assert_eq!(.., RepoStatus::Clean)` —
        // this module *cannot* construct a `Clean`. Its `RemoteWitness`
        // field is private to `sync.rs`, so nothing outside that module
        // can fabricate a clean verdict; it can only recognise one the
        // observation produced. That is the seal, demonstrated.
        assert!(
            matches!(status, RepoStatus::Clean(_)),
            "remote-backed clean repo should be Clean, got {status:?}"
        );
    }

    /// THE regression guard for the 2026-07-28 detection gap: a repo
    /// with no remote must report `NoRemote`, not `Clean`. Its whole
    /// history is on one disk; `git status --porcelain` says nothing and
    /// `git log --branches --not --remotes` says "0 unpushed" forever,
    /// because there is nothing to be ahead OF.
    #[tokio::test]
    async fn remote_less_repo_returns_no_remote_not_clean() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("orphan");
        std::fs::create_dir(&repo).unwrap();
        init_repo_without_remote(&repo);
        let job = StatusRepoJob::new("ws", "orphan", repo);
        let status = job.execute().await.unwrap();
        assert_eq!(status, RepoStatus::NoRemote);
    }

    #[tokio::test]
    async fn dirty_repo_returns_dirty() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        std::fs::write(repo.join("dirt"), "dirt\n").unwrap();
        let job = StatusRepoJob::new("ws", "repo", repo);
        let status = job.execute().await.unwrap();
        assert_eq!(status, RepoStatus::Dirty);
    }

    /// Idempotence-by-hand: running StatusRepoJob twice with no
    /// intervening mutation must yield the same RepoStatus. This is
    /// the property `shigoto_test::idempotence_quickcheck` will
    /// auto-verify once we wire it through — having a concrete
    /// per-Job test pinned here too means a regression is caught
    /// directly in tend's suite, not only in the cross-crate harness.
    #[tokio::test]
    async fn status_is_idempotent_on_unchanged_repo() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo);
        let job = StatusRepoJob::new("ws", "repo", repo);
        let first = job.execute().await.unwrap();
        let second = job.execute().await.unwrap();
        let third = job.execute().await.unwrap();
        assert_eq!(first, second);
        assert_eq!(second, third);
    }
}
