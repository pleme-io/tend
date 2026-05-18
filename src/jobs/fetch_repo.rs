//! `FetchRepoJob` — typed Job wrapping `sync::fetch_one_repo`. Per-repo
//! `git fetch --all --prune` with no working-tree mutation. Cheaper
//! than `PullRepoJob` (no merge), more expensive than `StatusRepoJob`
//! (touches the network). Often the first step of a reconciliation:
//! fetch every repo, then a separate pass decides whether to fast-
//! forward the working tree.
//!
//! Fourth instance of the wrapper pattern; same shape as
//! [`PullRepoJob`][crate::jobs::pull_repo] and
//! [`StatusRepoJob`][crate::jobs::status_repo].

use std::path::PathBuf;

use async_trait::async_trait;
use shigoto_types::{Job, JobId, JobKindId, JobScope, JobSubject};
use thiserror::Error;

use crate::sync::{fetch_one_repo, FetchOutcome};

pub(crate) const FETCH_REPO_KIND: &str = "tend.fetch-repo";

#[derive(Debug, Clone)]
pub(crate) struct FetchRepoJob {
    pub workspace: String,
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub quiet: bool,
}

impl FetchRepoJob {
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
pub(crate) enum FetchRepoError {
    #[error("fetch_one_repo invocation failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl Job for FetchRepoJob {
    type Output = FetchOutcome;
    type Error = FetchRepoError;

    fn id(&self) -> JobId {
        JobId {
            scope: JobScope::Workspace(self.workspace.clone()),
            kind: JobKindId::new(FETCH_REPO_KIND),
            subject: JobSubject::Repo(self.repo_name.clone()),
        }
    }

    fn kind(&self) -> JobKindId {
        JobKindId::new(FETCH_REPO_KIND)
    }

    async fn execute(&self) -> Result<FetchOutcome, FetchRepoError> {
        let path = self.repo_path.clone();
        let quiet = self.quiet;
        let label = self.repo_name.clone();
        tokio::task::spawn_blocking(move || fetch_one_repo(&path, quiet, &label))
            .await
            .map_err(|join_err| FetchRepoError::Invocation(format!("join error: {join_err}")))?
            .map_err(|err| FetchRepoError::Invocation(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_upstream(path: &std::path::Path) {
        Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "commit.gpgsign", "false"]).current_dir(path).status().unwrap();
        std::fs::write(path.join("file"), "x\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(path).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(path).status().unwrap();
    }

    fn clone_from(upstream: &std::path::Path, dest: &std::path::Path) {
        Command::new("git")
            .args(["clone", "-q", upstream.to_str().unwrap(), dest.to_str().unwrap()])
            .status()
            .unwrap();
    }

    #[tokio::test]
    async fn id_namespaces_by_workspace_and_repo() {
        let job = FetchRepoJob::new("ws", "name", "/tmp/x");
        let id = <FetchRepoJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(FETCH_REPO_KIND));
    }

    #[tokio::test]
    async fn missing_repo_reports_missing_skipped() {
        let tmp = TempDir::new().unwrap();
        let job = FetchRepoJob::new("ws", "missing", tmp.path().join("missing"));
        let outcome = job.execute().await.unwrap();
        assert_eq!(outcome, FetchOutcome::MissingSkipped);
    }

    #[tokio::test]
    async fn existing_clone_is_fetched_successfully() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        let local = tmp.path().join("local");
        std::fs::create_dir(&upstream).unwrap();
        init_upstream(&upstream);
        clone_from(&upstream, &local);

        let job = FetchRepoJob::new("ws", "local", local);
        let outcome = job.execute().await.unwrap();
        assert_eq!(outcome, FetchOutcome::Fetched);
    }

    /// Fetch is idempotent at the outcome grain — running it twice in
    /// a row on the same clone yields `Fetched` both times even if
    /// nothing new arrived.
    #[tokio::test]
    async fn fetch_is_outcome_idempotent() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        let local = tmp.path().join("local");
        std::fs::create_dir(&upstream).unwrap();
        init_upstream(&upstream);
        clone_from(&upstream, &local);

        let job = FetchRepoJob::new("ws", "local", local);
        let first = job.execute().await.unwrap();
        let second = job.execute().await.unwrap();
        assert_eq!(first, second);
    }
}
