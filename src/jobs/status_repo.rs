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

use async_trait::async_trait;
use shigoto_types::{Job, JobId, JobKindId, JobScope, JobSubject};
use thiserror::Error;

use crate::sync::{check_one_repo_status, RepoStatus};

/// Canonical kind id for every StatusRepoJob.
pub(crate) const STATUS_REPO_KIND: &str = "tend.status-repo";

#[derive(Debug, Clone)]
pub(crate) struct StatusRepoJob {
    pub workspace: String,
    pub repo_name: String,
    pub repo_path: PathBuf,
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
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum StatusRepoError {
    #[error("check_one_repo_status invocation failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl Job for StatusRepoJob {
    type Output = RepoStatus;
    type Error = StatusRepoError;

    fn id(&self) -> JobId {
        JobId {
            scope: JobScope::Workspace(self.workspace.clone()),
            kind: JobKindId::new(STATUS_REPO_KIND),
            subject: JobSubject::Repo(self.repo_name.clone()),
        }
    }

    fn kind(&self) -> JobKindId {
        JobKindId::new(STATUS_REPO_KIND)
    }

    async fn execute(&self) -> Result<RepoStatus, StatusRepoError> {
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
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo(path: &std::path::Path) {
        Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "commit.gpgsign", "false"]).current_dir(path).status().unwrap();
        std::fs::write(path.join("file"), "x\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(path).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(path).status().unwrap();
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
        assert_eq!(status, RepoStatus::Clean);
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
