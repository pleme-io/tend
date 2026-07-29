//! `RemediateRemoteUrlJob` — heal a repo whose `origin` URL embeds a
//! credential or disagrees with the workspace's declared
//! `clone_method`, by rewriting it to the canonical URL.
//!
//! The reaction half of the remote-URL drift class detected in
//! [`crate::remote_url`]. Detection alone would leave the operator to
//! hand-fix N repos every time a token rotates — the 2026-07-29 sweep
//! found 25 repos in that state, none of which any tend pass had ever
//! looked at.
//!
//! Two properties make this safe to run unattended, and both are the
//! reason this drift class gets an auto-reaction while
//! `RepoHasNoRemote` deliberately does not:
//!
//! 1. **The target never moves.** The replacement URL is built from
//!    the `org/repo` coordinate parsed out of the URL being replaced
//!    (`remote_url::GitHubSlug`), so the remote keeps pointing at
//!    exactly the repo it already pointed at. Only the transport and
//!    the credential change. There is no reading of this operation
//!    under which a repo gets retargeted at someone else's history.
//! 2. **Nothing is destroyed.** `git remote set-url` rewrites one line
//!    of `.git/config`. No refs, no objects, no working tree. If the
//!    rewrite is wrong the operator sets it back; nothing has been
//!    lost in the meantime.
//!
//! The job re-observes and re-classifies at execution time rather than
//! trusting the `DriftEvent` that scheduled it. The event is redacted
//! by construction (it cannot carry the URL — that is the whole point
//! of `remote_url::Credential`), and re-reading closes the window
//! between detection and remediation in which the operator may have
//! already fixed it by hand.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use shigoto_types::{JobScope, JobSubject, OutputSink, RecordingJob};
use thiserror::Error;

use crate::config::CloneMethod;
use crate::remote_url::{classify, RemoteUrlVerdict};
use crate::sync::RemoteWitness;

pub(crate) const REMEDIATE_REMOTE_KIND: &str = "tend.remediate-remote-url";

/// Typed result of one remediation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemediateRemoteOutcome {
    /// The remote was rewritten to the canonical URL. `slug` names the
    /// coordinate; it is safe to log because it never contained the
    /// credential.
    Rewritten { remote: String, slug: String },
    /// Re-classification found nothing to do — the operator fixed it
    /// between detection and this run, or a prior reaction in the same
    /// cycle already healed it. Not an error; the desired state holds.
    AlreadyConforming,
    /// Repo is gone or has no remote. Both have their own typed drift;
    /// this job declines to invent one.
    NotApplicable,
    /// `git remote set-url` exited non-zero.
    Failed { stderr: String },
}

#[derive(Clone)]
pub(crate) struct RemediateRemoteUrlJob {
    pub workspace: String,
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub clone_method: CloneMethod,
    output_sink: Option<Arc<dyn OutputSink<RemediateRemoteOutcome>>>,
}

impl std::fmt::Debug for RemediateRemoteUrlJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemediateRemoteUrlJob")
            .field("workspace", &self.workspace)
            .field("repo_name", &self.repo_name)
            .field("repo_path", &self.repo_path)
            .field("clone_method", &self.clone_method)
            .field("output_sink", &self.output_sink.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

impl RemediateRemoteUrlJob {
    pub(crate) fn new(
        workspace: impl Into<String>,
        repo_name: impl Into<String>,
        repo_path: impl Into<PathBuf>,
        clone_method: CloneMethod,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            repo_name: repo_name.into(),
            repo_path: repo_path.into(),
            clone_method,
            output_sink: None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn with_output_sink(
        mut self,
        sink: Arc<dyn OutputSink<RemediateRemoteOutcome>>,
    ) -> Self {
        self.output_sink = Some(sink);
        self
    }
}

#[derive(Debug, Error)]
pub(crate) enum RemediateRemoteError {
    #[error("remote remediation failed: {0}")]
    Invocation(String),
}

/// Re-observe, re-classify, and rewrite if still drifted.
///
/// Blocking; the Job wrapper runs it on a blocking thread.
fn remediate_one(
    repo_path: &std::path::Path,
    clone_method: &CloneMethod,
) -> anyhow::Result<RemediateRemoteOutcome> {
    if !repo_path.join(".git").exists() {
        return Ok(RemediateRemoteOutcome::NotApplicable);
    }

    let Some(witness) = RemoteWitness::observe(repo_path)? else {
        return Ok(RemediateRemoteOutcome::NotApplicable);
    };

    let verdict = classify(witness.url(), clone_method);
    let slug = match &verdict {
        RemoteUrlVerdict::EmbeddedCredential { slug, .. }
        | RemoteUrlVerdict::ProtocolMismatch { slug, .. } => slug.clone(),
        // Conforming needs nothing. OutOfScope is a remote tend has no
        // authority to rewrite — a non-GitHub remote, or a GitHub URL
        // we could not parse into a coordinate. Rewriting the latter
        // would mean guessing where it should point, which is the one
        // thing this job is built never to do.
        RemoteUrlVerdict::Conforming | RemoteUrlVerdict::OutOfScope => {
            return Ok(RemediateRemoteOutcome::AlreadyConforming);
        }
    };

    let target = slug.canonical_url(clone_method);
    let out = Command::new("git")
        .args(["remote", "set-url", witness.remote(), &target])
        .current_dir(repo_path)
        .output()?;

    if !out.status.success() {
        return Ok(RemediateRemoteOutcome::Failed {
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }

    Ok(RemediateRemoteOutcome::Rewritten {
        remote: witness.remote().to_string(),
        slug: slug.slug(),
    })
}

#[async_trait]
impl RecordingJob for RemediateRemoteUrlJob {
    type Output = RemediateRemoteOutcome;
    type Error = RemediateRemoteError;
    const KIND: &'static str = REMEDIATE_REMOTE_KIND;

    fn scope(&self) -> JobScope {
        JobScope::Workspace(self.workspace.clone())
    }

    fn subject(&self) -> JobSubject {
        JobSubject::Repo(self.repo_name.clone())
    }

    fn output_sink(&self) -> Option<&Arc<dyn OutputSink<Self::Output>>> {
        self.output_sink.as_ref()
    }

    async fn execute_body(&self) -> Result<RemediateRemoteOutcome, RemediateRemoteError> {
        let path = self.repo_path.clone();
        let method = self.clone_method.clone();
        tokio::task::spawn_blocking(move || remediate_one(&path, &method))
            .await
            .map_err(|join_err| RemediateRemoteError::Invocation(format!("join error: {join_err}")))?
            .map_err(|err| RemediateRemoteError::Invocation(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::{Job, JobKindId};
    use tempfile::TempDir;

    /// A repo whose `origin` is `url`, with no network involvement.
    fn repo_with_remote(dir: &std::path::Path, url: &str) {
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .status()
            .unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", url])
            .current_dir(dir)
            .status()
            .unwrap();
    }

    fn origin_url(dir: &std::path::Path) -> String {
        let out = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn embedded_credential_is_stripped() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(
            tmp.path(),
            "https://x-access-token:ghp_SECRETVALUE0000@github.com/akeylesslabs/k8s.git",
        );

        let outcome = remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();
        assert_eq!(
            outcome,
            RemediateRemoteOutcome::Rewritten {
                remote: "origin".into(),
                slug: "akeylesslabs/k8s".into(),
            }
        );
        assert_eq!(origin_url(tmp.path()), "git@github.com:akeylesslabs/k8s.git");
    }

    /// The point of the whole exercise: after healing, the secret is
    /// gone from `.git/config` entirely.
    #[test]
    fn secret_is_absent_from_config_after_healing() {
        let secret = "ghp_SECRETVALUE00000000000000";
        let tmp = TempDir::new().unwrap();
        repo_with_remote(
            tmp.path(),
            &format!("https://x-access-token:{secret}@github.com/org/repo.git"),
        );

        remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();

        let config = std::fs::read_to_string(tmp.path().join(".git/config")).unwrap();
        assert!(
            !config.contains(secret),
            "secret survived remediation in .git/config:\n{config}"
        );
    }

    #[test]
    fn protocol_mismatch_converges_to_declared_method() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(tmp.path(), "https://github.com/pleme-io/tend.git");

        remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();
        assert_eq!(origin_url(tmp.path()), "git@github.com:pleme-io/tend.git");
    }

    #[test]
    fn declared_https_converges_the_other_way() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(tmp.path(), "git@github.com:pleme-io/tend.git");

        remediate_one(tmp.path(), &CloneMethod::Https).unwrap();
        assert_eq!(origin_url(tmp.path()), "https://github.com/pleme-io/tend.git");
    }

    #[test]
    fn conforming_remote_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(tmp.path(), "git@github.com:pleme-io/tend.git");

        let outcome = remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();
        assert_eq!(outcome, RemediateRemoteOutcome::AlreadyConforming);
        assert_eq!(origin_url(tmp.path()), "git@github.com:pleme-io/tend.git");
    }

    /// A non-GitHub remote must survive untouched — tend has no
    /// authority to rewrite a fork remote pointing elsewhere.
    #[test]
    fn non_github_remote_is_never_rewritten() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(tmp.path(), "git@gitlab.com:org/repo.git");

        let outcome = remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();
        assert_eq!(outcome, RemediateRemoteOutcome::AlreadyConforming);
        assert_eq!(origin_url(tmp.path()), "git@gitlab.com:org/repo.git");
    }

    /// Remediation must be a fixed point: a second pass reports
    /// AlreadyConforming and changes nothing, so a repo cannot
    /// oscillate across reconcile cycles.
    #[test]
    fn remediation_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(
            tmp.path(),
            "https://x-access-token:ghp_AAAA@github.com/org/repo.git",
        );

        let first = remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();
        let after_first = origin_url(tmp.path());
        let second = remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();

        assert!(matches!(first, RemediateRemoteOutcome::Rewritten { .. }));
        assert_eq!(second, RemediateRemoteOutcome::AlreadyConforming);
        assert_eq!(origin_url(tmp.path()), after_first);
    }

    /// The remediated URL must equal what a fresh clone would produce,
    /// so a healed repo is indistinguishable from a newly synced one.
    #[test]
    fn healed_url_matches_workspace_clone_url() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(
            tmp.path(),
            "https://x-access-token:ghp_AAAA@github.com/pleme-io/tend.git",
        );
        remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();

        let mut ws = crate::config::Workspace::test_default("pleme-io");
        ws.org = Some("pleme-io".into());
        ws.clone_method = CloneMethod::Ssh;
        assert_eq!(origin_url(tmp.path()), ws.clone_url("tend"));
    }

    #[test]
    fn missing_repo_is_not_applicable() {
        let tmp = TempDir::new().unwrap();
        let outcome = remediate_one(&tmp.path().join("nope"), &CloneMethod::Ssh).unwrap();
        assert_eq!(outcome, RemediateRemoteOutcome::NotApplicable);
    }

    #[test]
    fn repo_without_remote_is_not_applicable() {
        let tmp = TempDir::new().unwrap();
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(tmp.path())
            .status()
            .unwrap();

        let outcome = remediate_one(tmp.path(), &CloneMethod::Ssh).unwrap();
        assert_eq!(outcome, RemediateRemoteOutcome::NotApplicable);
    }

    #[tokio::test]
    async fn id_namespaces_by_workspace_and_repo() {
        let job = RemediateRemoteUrlJob::new("ws", "name", "/tmp/x", CloneMethod::Ssh);
        let id = <RemediateRemoteUrlJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(REMEDIATE_REMOTE_KIND));
        assert_eq!(id.subject, JobSubject::Repo("name".into()));
    }

    #[tokio::test]
    async fn job_execute_heals_through_the_async_surface() {
        let tmp = TempDir::new().unwrap();
        repo_with_remote(
            tmp.path(),
            "https://x-access-token:ghp_AAAA@github.com/org/repo.git",
        );

        let job = RemediateRemoteUrlJob::new("ws", "repo", tmp.path(), CloneMethod::Ssh);
        let outcome = job.execute().await.unwrap();
        assert!(matches!(outcome, RemediateRemoteOutcome::Rewritten { .. }));
        assert_eq!(origin_url(tmp.path()), "git@github.com:org/repo.git");
    }
}
