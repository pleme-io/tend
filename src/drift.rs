//! Typed drift events.
//!
//! Every reconcile cycle's typed Job outcomes (PullOutcome / SyncOutcome /
//! FetchOutcome / RepoStatus) encode whether each repo is in its
//! expected state or has *drifted* away from it. Drift detection used
//! to live spread across `print_*` formatters + the audit log; this
//! module centralizes it as a typed primitive — `DriftEvent` — that
//! consumers (report, MCP queries, K8s controllers, future
//! convergence loops) can read once and act on consistently.
//!
//! Per SHIGOTO.md §IV.4 M5: "typed DriftEvent surface + audit emission."
//! The derivation `outcomes → events` is in [`derive_from_receipt`];
//! the sink trait lets backends (audit JSONL, in-memory, NATS publish,
//! etc.) capture events for downstream use.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use shigoto_types::{JobId, JobPhase, JobSubject};

use crate::reconcile::ReconcileReceipt;
use crate::sync::{PullOutcome, SyncOutcome};

/// Typed drift event. Each variant carries the minimum data a
/// consumer needs to identify *what* drifted and *which* artifact
/// is involved. The `repo_name` field is consistent across variants
/// so a report can group by repo.
///
/// New variants are added when a new drift class shows up in
/// typed outcomes — extending this enum is the canonical way to
/// promote "we noticed X" from an ad-hoc log line into typed
/// observability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum DriftEvent {
    /// Path on disk exists but has no `.git` entry. Operator must
    /// remove or adopt; `tend sync` won't clobber a stub.
    StubDirectoryFound {
        workspace: String,
        repo_name: String,
        path: PathBuf,
    },
    /// Working tree has uncommitted changes. Pull skipped this cycle;
    /// operator's call whether to stash/commit/discard.
    DirtyTreeBlocksPull {
        workspace: String,
        repo_name: String,
    },
    /// `git pull` returned non-zero. Often a remote-branch divergence
    /// or a refs-not-fetched issue. Carries the trimmed stderr for
    /// triage.
    PullFailed {
        workspace: String,
        repo_name: String,
        stderr: String,
    },
    /// `git clone` returned non-zero. Wrong URL, auth failure, or
    /// network issue.
    SyncFailed {
        workspace: String,
        repo_name: String,
        stderr: String,
    },
    /// A scheduled Job didn't reach Succeeded by the end of the
    /// cycle. Could be Deadlettered (retries exhausted), Retrying
    /// (will try again next cycle), or WaitingForOperator. The
    /// JobId names which Job + JobPhase carries the terminal state.
    JobUnhealed {
        job_id: JobId,
        phase: JobPhase,
    },
    /// A repo exists locally but doesn't appear in the latest org
    /// discovery output. Possible causes: archived on GitHub (excluded
    /// from active discovery), deleted upstream, renamed upstream, or
    /// a local-only repo the operator added without going through
    /// `tend sync`. The drift is informational — the substrate
    /// doesn't auto-remove local repos.
    LocalRepoNotInDiscovery {
        workspace: String,
        repo_name: String,
    },
}

impl DriftEvent {
    /// Which workspace this event is scoped to. `Some` for repo-
    /// level events; `None` for JobUnhealed where the JobId might
    /// carry the scope already.
    pub fn workspace(&self) -> Option<&str> {
        match self {
            Self::StubDirectoryFound { workspace, .. }
            | Self::DirtyTreeBlocksPull { workspace, .. }
            | Self::PullFailed { workspace, .. }
            | Self::SyncFailed { workspace, .. }
            | Self::LocalRepoNotInDiscovery { workspace, .. } => Some(workspace.as_str()),
            Self::JobUnhealed { job_id, .. } => match &job_id.scope {
                shigoto_types::JobScope::Workspace(w) => Some(w.as_str()),
                shigoto_types::JobScope::Repo { workspace, .. } => Some(workspace.as_str()),
                shigoto_types::JobScope::Global => None,
            },
        }
    }
}

/// Receivers of `DriftEvent`. Synchronous by design — emitters call
/// `record` from the reconcile path which is already async; the sink
/// implementations either buffer (`InMemoryDriftSink`) or write
/// synchronously to disk (`AuditFileDriftSink`).
pub(crate) trait DriftSink: Send + Sync {
    fn record(&self, event: &DriftEvent);
}

/// No-op sink. Default for ad-hoc paths that don't want drift
/// captured.
#[derive(Debug, Default)]
pub(crate) struct NullDriftSink;

impl DriftSink for NullDriftSink {
    fn record(&self, _event: &DriftEvent) {}
}

/// Buffer events in memory. Used by tests + by `tend report`'s
/// receipt-time aggregation.
#[derive(Debug, Default)]
pub(crate) struct InMemoryDriftSink {
    events: Mutex<Vec<DriftEvent>>,
}

impl InMemoryDriftSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the events captured so far. Doesn't clear the buffer.
    pub fn snapshot(&self) -> Vec<DriftEvent> {
        self.events
            .lock()
            .expect("InMemoryDriftSink mutex poisoned")
            .clone()
    }

    /// Take all events, clearing the buffer.
    pub fn drain(&self) -> Vec<DriftEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .expect("InMemoryDriftSink mutex poisoned"),
        )
    }

    pub fn len(&self) -> usize {
        self.events
            .lock()
            .expect("InMemoryDriftSink mutex poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl DriftSink for InMemoryDriftSink {
    fn record(&self, event: &DriftEvent) {
        self.events
            .lock()
            .expect("InMemoryDriftSink mutex poisoned")
            .push(event.clone());
    }
}

/// Append every recorded event as one JSON line to `path`. Same
/// shape as the scheduler-transitions log + tend's high-level audit
/// log so operators have one tool (`jq`) for everything.
pub(crate) struct AuditFileDriftSink {
    file: Mutex<std::fs::File>,
}

impl AuditFileDriftSink {
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating drift log parent {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening drift log {}", path.display()))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

impl DriftSink for AuditFileDriftSink {
    fn record(&self, event: &DriftEvent) {
        use std::io::Write;
        if let Ok(line) = serde_json::to_string(event) {
            if let Ok(mut f) = self.file.lock() {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// Project a `ReconcileReceipt` into a flat list of `DriftEvent`s.
/// Centralizes drift detection so all consumers see the same events
/// — adding a new drift class means extending this function once,
/// not chasing N report formatters.
///
/// Empty workspaces with everything Succeeded yield an empty Vec.
pub(crate) fn derive_from_receipt(receipt: &ReconcileReceipt) -> Vec<DriftEvent> {
    let mut events = Vec::new();

    // Pull-side drift.
    for (id, outcome) in &receipt.outcomes {
        let repo_name = match &id.subject {
            JobSubject::Repo(r) => r.clone(),
            _ => continue,
        };
        match outcome {
            PullOutcome::DirtySkipped => events.push(DriftEvent::DirtyTreeBlocksPull {
                workspace: receipt.workspace.clone(),
                repo_name,
            }),
            PullOutcome::Failed { stderr } => events.push(DriftEvent::PullFailed {
                workspace: receipt.workspace.clone(),
                repo_name,
                stderr: stderr.clone(),
            }),
            PullOutcome::Updated | PullOutcome::UpToDate | PullOutcome::MissingSkipped => {}
        }
    }

    // Sync-side drift.
    for (id, outcome) in &receipt.sync_outcomes {
        let repo_name = match &id.subject {
            JobSubject::Repo(r) => r.clone(),
            _ => continue,
        };
        match outcome {
            SyncOutcome::StubExisted => events.push(DriftEvent::StubDirectoryFound {
                workspace: receipt.workspace.clone(),
                repo_name: repo_name.clone(),
                // Path is implicit from workspace.base_dir + repo_name
                // at receipt time; reconstruct here from the JobId's
                // subject. Full PathBuf would require carrying the
                // base_dir through the receipt, which we don't.
                path: PathBuf::from(&repo_name),
            }),
            SyncOutcome::Failed { stderr } => events.push(DriftEvent::SyncFailed {
                workspace: receipt.workspace.clone(),
                repo_name,
                stderr: stderr.clone(),
            }),
            SyncOutcome::Cloned | SyncOutcome::AlreadyPresent => {}
        }
    }

    // Scheduler-level drift: any Job that didn't reach Succeeded.
    for (id, phase) in &receipt.failed_jobs {
        events.push(DriftEvent::JobUnhealed {
            job_id: id.clone(),
            phase: phase.clone(),
        });
    }

    // Cross-reference drift: when DiscoverOrgJob ran (so we have an
    // authoritative active-repo list), flag any sync_outcomes whose
    // repo doesn't appear in discovery. The discovery_outcomes map
    // holds the typed RepoState list per workspace; collect names
    // into a set, then check every sync subject against it.
    //
    // No-op when discovery didn't run this cycle (cache fresh + gate
    // skipped, or workspace.discover=false) — discovery_outcomes is
    // empty, so we don't emit false positives saying "every local
    // repo is missing upstream."
    if !receipt.discovery_outcomes.is_empty() {
        let active_names: std::collections::HashSet<&str> = receipt
            .discovery_outcomes
            .values()
            .flat_map(|states| states.iter().map(|s| s.name.as_str()))
            .collect();
        for (id, _) in &receipt.sync_outcomes {
            let repo_name = match &id.subject {
                JobSubject::Repo(r) => r.clone(),
                _ => continue,
            };
            if !active_names.contains(repo_name.as_str()) {
                events.push(DriftEvent::LocalRepoNotInDiscovery {
                    workspace: receipt.workspace.clone(),
                    repo_name,
                });
            }
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::{JobKindId, JobScope};
    use std::collections::HashMap;

    fn pull_id(repo: &str) -> JobId {
        JobId {
            scope: JobScope::Workspace("ws".into()),
            kind: JobKindId::new("tend.pull-repo"),
            subject: JobSubject::Repo(repo.into()),
        }
    }

    fn sync_id(repo: &str) -> JobId {
        JobId {
            scope: JobScope::Workspace("ws".into()),
            kind: JobKindId::new("tend.sync-repo"),
            subject: JobSubject::Repo(repo.into()),
        }
    }

    fn empty_receipt() -> ReconcileReceipt {
        ReconcileReceipt {
            workspace: "ws".into(),
            outcomes: HashMap::new(),
            sync_outcomes: HashMap::new(),
            discovery_outcomes: HashMap::new(),
            failed_jobs: vec![],
        }
    }

    #[test]
    fn clean_receipt_yields_no_drift() {
        let mut r = empty_receipt();
        r.outcomes.insert(pull_id("r1"), PullOutcome::UpToDate);
        r.outcomes.insert(pull_id("r2"), PullOutcome::Updated);
        r.sync_outcomes
            .insert(sync_id("r1"), SyncOutcome::AlreadyPresent);
        r.sync_outcomes.insert(sync_id("r2"), SyncOutcome::Cloned);

        let events = derive_from_receipt(&r);
        assert!(events.is_empty(), "expected no drift events; got {:?}", events);
    }

    #[test]
    fn dirty_tree_becomes_drift() {
        let mut r = empty_receipt();
        r.outcomes
            .insert(pull_id("dirty"), PullOutcome::DirtySkipped);

        let events = derive_from_receipt(&r);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DriftEvent::DirtyTreeBlocksPull { workspace, repo_name } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "dirty");
            }
            other => panic!("expected DirtyTreeBlocksPull, got {other:?}"),
        }
    }

    #[test]
    fn pull_failed_carries_stderr() {
        let mut r = empty_receipt();
        r.outcomes.insert(
            pull_id("broken"),
            PullOutcome::Failed {
                stderr: "no such ref".into(),
            },
        );

        let events = derive_from_receipt(&r);
        match &events[0] {
            DriftEvent::PullFailed {
                workspace,
                repo_name,
                stderr,
            } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "broken");
                assert!(stderr.contains("no such ref"));
            }
            other => panic!("expected PullFailed, got {other:?}"),
        }
    }

    #[test]
    fn stub_directory_becomes_drift() {
        let mut r = empty_receipt();
        r.sync_outcomes
            .insert(sync_id("stub"), SyncOutcome::StubExisted);

        let events = derive_from_receipt(&r);
        assert_eq!(events.len(), 1);
        match &events[0] {
            DriftEvent::StubDirectoryFound {
                workspace,
                repo_name,
                ..
            } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "stub");
            }
            other => panic!("expected StubDirectoryFound, got {other:?}"),
        }
    }

    #[test]
    fn unhealed_jobs_become_drift() {
        let mut r = empty_receipt();
        r.failed_jobs
            .push((pull_id("dead"), JobPhase::Deadlettered));

        let events = derive_from_receipt(&r);
        match &events[0] {
            DriftEvent::JobUnhealed { phase, .. } => {
                assert!(matches!(phase, JobPhase::Deadlettered));
            }
            other => panic!("expected JobUnhealed, got {other:?}"),
        }
    }

    /// Cross-reference drift: a local repo that doesn't appear in
    /// the latest discovery is flagged as LocalRepoNotInDiscovery.
    #[test]
    fn local_repo_not_in_discovery_becomes_drift() {
        use crate::provider::RepoState;
        let mut r = empty_receipt();

        // Local "ghost" repo exists (sync sees AlreadyPresent) but
        // discovery doesn't list it.
        r.sync_outcomes
            .insert(sync_id("ghost"), SyncOutcome::AlreadyPresent);
        // Discovery has a different repo "alive" — the active list.
        r.discovery_outcomes.insert(
            JobId {
                scope: JobScope::Workspace("ws".into()),
                kind: JobKindId::new("tend.discover-org"),
                subject: JobSubject::Org("ws".into()),
            },
            vec![RepoState {
                name: "alive".into(),
                default_branch: Some("main".into()),
                archived: false,
                fork: false,
                language: None,
            }],
        );

        let events = derive_from_receipt(&r);
        // Should have exactly one LocalRepoNotInDiscovery event for
        // "ghost." "alive" is in discovery so no drift; "ghost" isn't.
        let local_only: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, DriftEvent::LocalRepoNotInDiscovery { .. }))
            .collect();
        assert_eq!(local_only.len(), 1);
        match local_only[0] {
            DriftEvent::LocalRepoNotInDiscovery { repo_name, .. } => {
                assert_eq!(repo_name, "ghost");
            }
            _ => unreachable!(),
        }
    }

    /// When discovery didn't run this cycle (cache fresh, gate
    /// skipped, etc.), discovery_outcomes is empty — we MUST NOT emit
    /// LocalRepoNotInDiscovery for every local repo (that would mean
    /// "all repos drifted" every single cycle the cache was fresh).
    #[test]
    fn empty_discovery_suppresses_cross_reference_drift() {
        let mut r = empty_receipt();
        r.sync_outcomes
            .insert(sync_id("local1"), SyncOutcome::AlreadyPresent);
        r.sync_outcomes
            .insert(sync_id("local2"), SyncOutcome::AlreadyPresent);
        // discovery_outcomes is empty.

        let events = derive_from_receipt(&r);
        // None of the events should be LocalRepoNotInDiscovery — the
        // cross-reference path doesn't fire without discovery data.
        assert!(events
            .iter()
            .all(|e| !matches!(e, DriftEvent::LocalRepoNotInDiscovery { .. })));
    }

    #[test]
    fn in_memory_sink_captures_and_drains() {
        let sink = InMemoryDriftSink::new();
        assert!(sink.is_empty());

        let e = DriftEvent::DirtyTreeBlocksPull {
            workspace: "ws".into(),
            repo_name: "r".into(),
        };
        sink.record(&e);
        assert_eq!(sink.len(), 1);

        let drained = sink.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0], e);
        assert!(sink.is_empty(), "drain() should clear");
    }

    #[test]
    fn audit_file_sink_appends_jsonl_lines() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("drift.jsonl");
        let sink = AuditFileDriftSink::new(&path).unwrap();
        sink.record(&DriftEvent::DirtyTreeBlocksPull {
            workspace: "ws".into(),
            repo_name: "r1".into(),
        });
        sink.record(&DriftEvent::PullFailed {
            workspace: "ws".into(),
            repo_name: "r2".into(),
            stderr: "oops".into(),
        });
        // Drop sink to ensure the underlying file flushes the buffer.
        drop(sink);

        let content = std::fs::read_to_string(&path).unwrap();
        let line_count = content.lines().count();
        assert_eq!(line_count, 2);
        // Each line is well-formed JSON.
        for line in content.lines() {
            let _: DriftEvent = serde_json::from_str(line).unwrap();
        }
    }
}
