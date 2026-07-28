//! Typed reactions — bind drift kinds to handler Jobs.
//!
//! When a reconcile cycle derives a `DriftEvent` from typed outcomes,
//! the operator often wants something done about it: a dirty tree
//! could be stashed and re-pulled, a pull failure on a missing ref
//! could be remediated by an explicit fetch, etc. This module is the
//! binding layer between detected drift and scheduled handler Jobs.
//!
//! Per SHIGOTO.md §IV.4 M6: "typed reactions — bind drift kinds to
//! actions." The binding is a pure function `DriftEvent → Option<Job>`
//! — no Reaction trait, no policy registry, just data → Job. Future
//! commits add more reaction arms by extending the match in
//! [`react_to_drift`] without changing the framework.
//!
//! Reactions fire once per reconcile cycle. A reaction Job that fails
//! produces another DriftEvent (via the normal scheduler→drift path)
//! — but reactions don't recursively spawn reactions in the same
//! cycle. The operator runs the next reconcile to retry. Bounded
//! one-step reactivity is what we want; cascading reactions invite
//! infinite-loop pathology and the user can always escalate manually
//! via `tend reconcile` again.

use std::path::PathBuf;
use std::sync::Arc;

use shigoto_types::ErasedJob;

use crate::config::Workspace;
use crate::drift::DriftEvent;
use crate::jobs::fetch_repo::FetchRepoJob;

/// Map a single `DriftEvent` to an optional handler `Job`.
///
/// Returns `None` when the drift either has no automatic remedy or
/// is purely informational (e.g. `JobUnhealed` — scheduler retry
/// policy already handles it; no separate reaction needed).
///
/// New reaction arms add a match case here. Each case constructs a
/// concrete Job impl, boxes as `Arc<dyn ErasedJob>` (the scheduler's
/// register_job surface), and returns. The Job's id() must be unique
/// — typically derived from the drift event's repo + a kind id like
/// "reaction.fetch-after-pull-failed".
pub(crate) fn react_to_drift(
    event: &DriftEvent,
    workspace: &Workspace,
) -> Option<Arc<dyn ErasedJob>> {
    match event {
        // BranchRenamed (upstream renamed/removed the configured ref):
        // a plain `git fetch --all --prune` updates refs so the next
        // reconcile cycle can pick up the new HEAD. Typed via
        // `classify_pull_failure` from the canonical git error message.
        DriftEvent::PullFailedBranchRenamed {
            workspace: ws,
            repo_name,
            ..
        } => {
            let repo_path = workspace_repo_path(workspace, ws, repo_name)?;
            let job = FetchRepoJob::new(ws, repo_name, repo_path);
            Some(Arc::new(job))
        }

        // Legacy bare `PullFailed` may still appear from old audit-log
        // replay or from a future failure shape not yet typed. Apply
        // the same string-sniff heuristic to keep behavior backwards-
        // compatible while the typed surface fills in.
        DriftEvent::PullFailed {
            workspace: ws,
            repo_name,
            stderr,
        } if stderr.contains("no such ref") || stderr.contains("not fetched") => {
            let repo_path = workspace_repo_path(workspace, ws, repo_name)?;
            let job = FetchRepoJob::new(ws, repo_name, repo_path);
            Some(Arc::new(job))
        }

        // RepoHasNoRemote: report only, permanently. This is NOT a
        // "not implemented yet" gap — auto-creating or auto-pushing a
        // remote is forbidden by design. Where the remote should point
        // is an operator decision with destructive potential: the
        // obvious guess (`git@github.com:<org>/<repo>.git`) can already
        // hold unrelated content, and pushing to it would either fail
        // confusingly or, force-pushed, destroy a history. `tend doctor`
        // names the decision; the operator makes it.
        DriftEvent::RepoHasNoRemote { .. } => None,

        // Other drift variants currently have no automatic reaction.
        // They surface in `tend report` for operator action. The
        // SAFE-CONVERGENCE M2 milestone will add:
        //   - PullFailedNoUpstream  → AutoAct(SetUpstream)
        //   - PullFailedDiverged    → Escalate (need operator decision)
        //   - PullFailedRepoMissing → Quarantine (mark workspace entry)
        //   - PullFailedTransient   → NoOp (next cycle retries naturally)
        //   - StubDirectoryFound    : operator decides adopt-or-remove
        //                              (future M8 doctor commands)
        //   - DirtyTreeBlocksPull   : operator decides stash/commit/discard
        //                              (auto-stashing risks data loss)
        //   - SyncFailed            : clone failures usually need
        //                              credential / network fixes
        //   - JobUnhealed           : scheduler retry policy handles it
        _ => None,
    }
}

/// Resolve a repo's absolute path from workspace context. Best-effort:
/// returns None when the workspace name doesn't match (defensive — the
/// reaction was generated from a receipt that should already be
/// scoped to this workspace).
fn workspace_repo_path(workspace: &Workspace, ws_name: &str, repo: &str) -> Option<PathBuf> {
    if workspace.name != ws_name {
        return None;
    }
    workspace.resolved_base_dir().ok().map(|d| d.join(repo))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::Job;

    fn workspace_at(name: &str, base: &str) -> Workspace {
        let mut ws = Workspace::test_default(name);
        ws.base_dir = base.into();
        ws
    }

    #[test]
    fn pull_failed_no_such_ref_triggers_fetch_reaction() {
        let event = DriftEvent::PullFailed {
            workspace: "ws".into(),
            repo_name: "repo1".into(),
            stderr: "Your configuration specifies to merge with the ref 'refs/heads/main' from the remote, but no such ref was fetched.".into(),
        };
        let ws = workspace_at("ws", "/tmp");
        let reaction = react_to_drift(&event, &ws);
        assert!(reaction.is_some(), "no-such-ref should trigger reaction");

        // The reaction is a FetchRepoJob; we can verify by checking
        // its kind id via the ErasedJob trait surface.
        let job = reaction.unwrap();
        assert_eq!(job.kind().0, crate::jobs::fetch_repo::FETCH_REPO_KIND);
    }

    #[test]
    fn pull_failed_other_stderr_no_reaction() {
        // Auth failure, network issue, anything that isn't ref-shaped
        // → no automatic reaction.
        let event = DriftEvent::PullFailed {
            workspace: "ws".into(),
            repo_name: "repo1".into(),
            stderr: "fatal: could not read Username for 'https://github.com': terminal prompts disabled".into(),
        };
        let ws = workspace_at("ws", "/tmp");
        assert!(react_to_drift(&event, &ws).is_none());
    }

    #[test]
    fn dirty_tree_no_reaction() {
        // Auto-stashing user changes is too risky to do by default.
        let event = DriftEvent::DirtyTreeBlocksPull {
            workspace: "ws".into(),
            repo_name: "repo1".into(),
        };
        let ws = workspace_at("ws", "/tmp");
        assert!(react_to_drift(&event, &ws).is_none());
    }

    #[test]
    fn stub_directory_no_reaction() {
        let event = DriftEvent::StubDirectoryFound {
            workspace: "ws".into(),
            repo_name: "stub".into(),
            path: PathBuf::from("/tmp/stub"),
        };
        let ws = workspace_at("ws", "/tmp");
        // Future M8 doctor commands will handle this; for now,
        // operator-action only.
        assert!(react_to_drift(&event, &ws).is_none());
    }

    /// Detection is the deliverable; remediation is not. tend must
    /// never invent a remote for a repo that has none — see the match
    /// arm for why this is a permanent policy, not a stub.
    #[test]
    fn repo_has_no_remote_never_auto_remediates() {
        let event = DriftEvent::RepoHasNoRemote {
            workspace: "ws".into(),
            repo_name: "ferrite-zig".into(),
        };
        let ws = workspace_at("ws", "/tmp");
        assert!(
            react_to_drift(&event, &ws).is_none(),
            "tend must not auto-create or auto-push a remote — where it \
             points is an operator decision"
        );
    }

    #[test]
    fn cross_workspace_event_yields_none() {
        // Defensive: a drift event scoped to workspace "other" shouldn't
        // produce a reaction in workspace "ws".
        let event = DriftEvent::PullFailed {
            workspace: "other".into(),
            repo_name: "repo1".into(),
            stderr: "no such ref".into(),
        };
        let ws = workspace_at("ws", "/tmp");
        assert!(
            react_to_drift(&event, &ws).is_none(),
            "cross-workspace event must not produce a reaction"
        );
    }
}
