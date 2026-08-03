//! `DiscoverOrgJob` — typed Job wrapping `provider::discover_github_repos_cached`.
//! First Job kind that isn't a per-repo reconcile primitive: scope is
//! `Workspace`, subject is `Org` (the GitHub org being discovered).
//! Output is a `Vec<String>` of repo names; the cache layer below
//! decides whether to hit the API or return cached data.
//!
//! M2 from the original tend plan (SHIGOTO.md §IV.4): "discovery TTL —
//! re-hit GitHub each cycle." The TTL itself is in `cache::read`'s
//! freshness check; this Job makes the discovery operation auditable
//! through the scheduler's transition log + InMemorySink.
//!
//! Use cases:
//! - Daemon path: pre-reconcile, discover the workspace's current repo
//!   set so SyncRepoJobs are created for newly-published repos and
//!   archived ones drop out.
//! - MCP query: "list repos in org X" → schedule DiscoverOrgJob,
//!   read result from InMemorySink, return.

use std::sync::Arc;

use async_trait::async_trait;
use shigoto_types::{JobScope, JobSubject, OutputSink, RecordingJob};
use thiserror::Error;

use crate::provider::{discover_github_repo_states_cached, RepoState};

pub(crate) const DISCOVER_ORG_KIND: &str = "tend.discover-org";

#[derive(Clone)]
pub(crate) struct DiscoverOrgJob {
    pub workspace: String,
    pub org: String,
    /// When true, bypasses the cache and always hits the GitHub API.
    /// Matches the existing `discover_github_repos_cached(refresh)`
    /// flag so consumers can express "I really want fresh data".
    pub refresh: bool,
    output_sink: Option<Arc<dyn OutputSink<Vec<RepoState>>>>,
}

impl std::fmt::Debug for DiscoverOrgJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoverOrgJob")
            .field("workspace", &self.workspace)
            .field("org", &self.org)
            .field("refresh", &self.refresh)
            .field("output_sink", &self.output_sink.as_ref().map(|_| "<sink>"))
            .finish()
    }
}

impl DiscoverOrgJob {
    pub(crate) fn new(workspace: impl Into<String>, org: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            org: org.into(),
            refresh: false,
            output_sink: None,
        }
    }

    pub(crate) fn with_refresh(mut self, refresh: bool) -> Self {
        self.refresh = refresh;
        self
    }

    pub(crate) fn with_output_sink(mut self, sink: Arc<dyn OutputSink<Vec<RepoState>>>) -> Self {
        self.output_sink = Some(sink);
        self
    }
}

#[derive(Debug, Error)]
pub(crate) enum DiscoverOrgError {
    #[error("discover_github_repos_cached failed: {0}")]
    Invocation(String),
}

#[async_trait]
impl RecordingJob for DiscoverOrgJob {
    type Output = Vec<RepoState>;
    type Error = DiscoverOrgError;
    const KIND: &'static str = DISCOVER_ORG_KIND;

    fn scope(&self) -> JobScope {
        JobScope::Workspace(self.workspace.clone())
    }

    fn subject(&self) -> JobSubject {
        JobSubject::Org(self.org.clone())
    }

    fn output_sink(&self) -> Option<&Arc<dyn OutputSink<Self::Output>>> {
        self.output_sink.as_ref()
    }

    async fn execute_body(&self) -> Result<Vec<RepoState>, DiscoverOrgError> {
        // discover_github_repo_states_cached is async-native — no
        // spawn_blocking needed (it uses reqwest under the hood).
        discover_github_repo_states_cached(&self.org, self.refresh)
            .await
            .map_err(|err| DiscoverOrgError::Invocation(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::{Job, JobKindId};

    #[tokio::test]
    async fn id_scopes_to_workspace_and_subject_to_org() {
        let job = DiscoverOrgJob::new("test-ws", "my-org");
        let id = <DiscoverOrgJob as Job>::id(&job);
        assert_eq!(id.kind, JobKindId::new(DISCOVER_ORG_KIND));
        match id.scope {
            JobScope::Workspace(ref w) => assert_eq!(w, "test-ws"),
            _ => panic!("expected Workspace scope"),
        }
        match id.subject {
            JobSubject::Org(ref o) => assert_eq!(o, "my-org"),
            _ => panic!("expected Org subject"),
        }
    }

    #[tokio::test]
    async fn with_refresh_overrides_default() {
        let job = DiscoverOrgJob::new("ws", "org").with_refresh(true);
        assert!(job.refresh);
    }

    // Note: end-to-end execution of DiscoverOrgJob hits the real GitHub
    // API. That's covered by tend's existing integration tests around
    // discover_github_repos_cached. Here we only test the substrate-side
    // wiring (id construction, builder methods, RecordingJob conformance).
}
