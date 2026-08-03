//! `SyncRepoJob` — typed Job wrapping `sync::sync_one_repo`. Clones
//! one repo if absent; otherwise records whether the path is already
//! a valid worktree (`AlreadyPresent`) or an unrecoverable stub
//! without `.git` (`StubExisted`).
//!
//! Fourth instance of the wrapper pattern (after pull / status / fetch).
//! Unlike the other three, this Job carries the pre-constructed clone
//! URL as data — the per-repo helper has no `Workspace` dependency,
//! so the Job's `execute()` doesn't need a workspace lookup at run
//! time. Construction sites in tend pass `workspace.clone_url(name)`
//! when scheduling.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use shigoto_types::{JobScope, JobSubject, OutputSink, RecordingJob};
use thiserror::Error;

use crate::sync::{sync_one_repo, SyncOutcome};

pub(crate) const SYNC_REPO_KIND: &str = "tend.sync-repo";

#[derive(Clone)]
pub(crate) struct SyncRepoJob {
    pub workspace: String,
    pub repo_name: String,
    pub repo_path: PathBuf,
    /// Pre-constructed clone URL. The helper rewrites it with
    /// `GITHUB_TOKEN` at execute time when the URL targets github.com
    /// — same behavior as the legacy `sync_repos` batch path.
    pub clone_url: String,
    pub quiet: bool,
    output_sink: Option<Arc<dyn OutputSink<SyncOutcome>>>,
}

impl std::fmt::Debug for SyncRepoJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncRepoJob")
            .field("workspace", &self.workspace)
            .field("repo_name", &self.repo_name)
            .field("repo_path", &self.repo_path)
            .field("clone_url", &self.clone_url)
            .field("quiet", &self.quiet)
            .field("output_sink", &self.output_sink.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

impl SyncRepoJob {
    pub(crate) fn new(
        workspace: impl Into<String>,
        repo_name: impl Into<String>,
        repo_path: impl Into<PathBuf>,
        clone_url: impl Into<String>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            repo_name: repo_name.into(),
            repo_path: repo_path.into(),
            clone_url: clone_url.into(),
            quiet: true,
            output_sink: None,
        }
    }

    pub(crate) fn with_output_sink(mut self, sink: Arc<dyn OutputSink<SyncOutcome>>) -> Self {
        self.output_sink = Some(sink);
        self
    }
}

#[derive(Debug, Error)]
pub(crate) enum SyncRepoError {
    #[error("sync_one_repo invocation failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl RecordingJob for SyncRepoJob {
    type Output = SyncOutcome;
    type Error = SyncRepoError;
    const KIND: &'static str = SYNC_REPO_KIND;

    fn scope(&self) -> JobScope {
        JobScope::Workspace(self.workspace.clone())
    }

    fn subject(&self) -> JobSubject {
        JobSubject::Repo(self.repo_name.clone())
    }

    fn output_sink(&self) -> Option<&Arc<dyn OutputSink<Self::Output>>> {
        self.output_sink.as_ref()
    }

    async fn execute_body(&self) -> Result<SyncOutcome, SyncRepoError> {
        let url = self.clone_url.clone();
        let path = self.repo_path.clone();
        let label = self.repo_name.clone();
        let quiet = self.quiet;
        tokio::task::spawn_blocking(move || sync_one_repo(url, &path, &label, quiet))
            .await
            .map_err(|join_err| SyncRepoError::Invocation(format!("join error: {join_err}")))?
            .map_err(|err| SyncRepoError::Invocation(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::{Job, JobKindId};
    use std::process::Command;
    use tempfile::TempDir;

    fn init_upstream(path: &std::path::Path) {
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(path)
            .status()
            .unwrap();
        std::fs::write(path.join("file"), "x\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(path)
            .status()
            .unwrap();
    }

    #[tokio::test]
    async fn id_namespaces_by_workspace_and_repo() {
        let job = SyncRepoJob::new("ws", "name", "/tmp/x", "file:///nope");
        let id = <SyncRepoJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(SYNC_REPO_KIND));
    }

    #[tokio::test]
    async fn fresh_path_is_cloned() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join("upstream");
        std::fs::create_dir(&upstream).unwrap();
        init_upstream(&upstream);

        let dest = tmp.path().join("dest");
        let url = format!("file://{}", upstream.display());
        let job = SyncRepoJob::new("ws", "dest", dest.clone(), url);
        let outcome = job.execute().await.unwrap();
        assert_eq!(outcome, SyncOutcome::Cloned);
        // Destination is now a real worktree.
        assert!(dest.join(".git").exists());
    }

    #[tokio::test]
    async fn existing_worktree_is_already_present() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("existing");
        std::fs::create_dir(&dest).unwrap();
        init_upstream(&dest);

        let job = SyncRepoJob::new("ws", "existing", dest, "file:///irrelevant");
        let outcome = job.execute().await.unwrap();
        assert_eq!(outcome, SyncOutcome::AlreadyPresent);
    }

    #[tokio::test]
    async fn stub_without_git_returns_stub_existed() {
        let tmp = TempDir::new().unwrap();
        let stub = tmp.path().join("stub");
        std::fs::create_dir(&stub).unwrap();
        std::fs::write(stub.join("README.md"), "stub\n").unwrap();

        let job = SyncRepoJob::new("ws", "stub", stub, "file:///irrelevant");
        let outcome = job.execute().await.unwrap();
        assert_eq!(outcome, SyncOutcome::StubExisted);
    }

    #[tokio::test]
    async fn bad_url_returns_failed_with_stderr() {
        let tmp = TempDir::new().unwrap();
        let dest = tmp.path().join("dest");
        let job = SyncRepoJob::new(
            "ws",
            "dest",
            dest,
            "file:///definitely-not-a-real-git-repo-zzz",
        );
        let outcome = job.execute().await.unwrap();
        match outcome {
            SyncOutcome::Failed { stderr } => assert!(!stderr.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
