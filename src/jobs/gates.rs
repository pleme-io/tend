//! Consumer-defined gates that compose with shigoto's scheduler.
//!
//! Until M0.19b the only gate in play was `AllUpstreamsTerminal`
//! (implicit; enforces Dag edges). This module is where tend-specific
//! gates land — gates that read tend's domain state (cache freshness,
//! token availability, etc.) to decide whether a Job should run.
//!
//! Pattern: each gate impl reads its decision input out of either
//! `ctx.snapshot` (FSM state of other Jobs) or external state
//! (filesystem cache, env vars, etc.). Returns:
//!   - `GateOutcome::Pass`  — gate is satisfied; the Job may advance.
//!   - `GateOutcome::Wait`  — not satisfied now; re-evaluate next tick.
//!   - `GateOutcome::Skip(SkipReason)` — refuses the Job permanently
//!                                       this cycle; Job → Skipped.
//!
//! Registration is consumer-side:
//!     scheduler.register_gate(
//!         JobKindId::new("tend.discover-org"),
//!         Arc::new(CacheFreshGate),
//!     ).await;

use std::path::PathBuf;

use shigoto_gate::{Gate, GateContext, GateOutcome};
use shigoto_types::{JobSubject, SkipReason};

use crate::cache;
use crate::placeholder;

/// Gate that Skips a `DiscoverOrgJob` when the discovery cache for the
/// Job's `JobSubject::Org` is fresh (within the TTL hard-coded in
/// `cache::DEFAULT_TTL_SECS`, currently 6 hours). When the cache is
/// stale or absent, the gate Passes so discovery actually runs.
///
/// Operational effect: registering this gate against
/// `JobKindId::new("tend.discover-org")` lets the scheduler audit
/// "we skipped discovery this cycle because the cache was fresh"
/// instead of running a no-op discovery Job that hits the cache layer
/// inside `discover_github_repos_cached`. The behavior is identical;
/// the audit-log shape differs.
///
/// This is the substrate's first user-registered gate. The
/// `AllUpstreamsTerminal` gate is implicit on every Job; this is
/// opt-in.
pub(crate) struct CacheFreshGate;

impl Gate for CacheFreshGate {
    fn name(&self) -> &'static str {
        "tend.cache-fresh"
    }

    fn evaluate(&self, ctx: &GateContext) -> GateOutcome {
        // Extract the org name from the Job's subject. Jobs with a
        // non-Org subject are out-of-scope — pass through.
        let org = match &ctx.job_id.subject {
            JobSubject::Org(o) => o,
            _ => return GateOutcome::Pass,
        };

        if cache::read(org).is_some() {
            // Cache is fresh — skip the discovery this cycle. Operator
            // can force a refresh via `tend discover <org> --refresh`
            // (CLI) or by deleting the cache file.
            GateOutcome::Skip(SkipReason::GateRejected)
        } else {
            // No cached entry OR stale — discovery should run.
            GateOutcome::Pass
        }
    }
}

/// Gate that Skips reconcile Jobs against directories marked as
/// intentional placeholders (`.tend-placeholder` marker file present
/// in the dir). Constructed per workspace because the base_dir is
/// needed to resolve the per-repo path from the Job's
/// `JobSubject::Repo(name)` to a filesystem path.
///
/// Operational effect: registering against tend.sync-repo +
/// tend.pull-repo (and friends) means a directory the operator has
/// marked with `tend placeholder <name>` is skipped every reconcile
/// cycle, surfacing in the audit log as Skipped(GateRejected) rather
/// than as a silent no-op or worse, a misclassified StubDirectoryFound
/// drift.
///
/// M3 from SHIGOTO.md §IV.4 — the typed empty-dir intent primitive.
pub(crate) struct NotPlaceholderGate {
    pub base_dir: PathBuf,
}

impl Gate for NotPlaceholderGate {
    fn name(&self) -> &'static str {
        "tend.not-placeholder"
    }

    fn evaluate(&self, ctx: &GateContext) -> GateOutcome {
        // Only meaningful for Repo-subjected Jobs. Other subjects
        // pass through (defensive — registering against a kind that
        // never has Repo subjects shouldn't deadlock that kind).
        let repo_name = match &ctx.job_id.subject {
            JobSubject::Repo(r) => r,
            _ => return GateOutcome::Pass,
        };

        let path = self.base_dir.join(repo_name);
        if placeholder::is_placeholder(&path) {
            GateOutcome::Skip(SkipReason::GateRejected)
        } else {
            GateOutcome::Pass
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_dag::Dag;
    use shigoto_types::{JobId, JobKindId, JobPhase, JobScope, Snapshot};
    use std::collections::HashMap;

    fn snapshot_empty() -> Snapshot {
        Snapshot {
            phases: HashMap::new(),
        }
    }

    fn discover_org_id(org: &str) -> JobId {
        JobId {
            scope: JobScope::Workspace("ws".into()),
            kind: JobKindId::new(super::super::discover_org::DISCOVER_ORG_KIND),
            subject: JobSubject::Org(org.into()),
        }
    }

    /// Cache miss → Pass (discovery should run).
    #[test]
    fn absent_cache_passes() {
        // Use a sentinel org name that's extremely unlikely to exist
        // in the operator's cache directory. We don't write a cache
        // entry, so `cache::read` returns None → Pass.
        let id = discover_org_id("gate-test-missing-org-xyzzy-12345");
        let snap = snapshot_empty();
        let dag = Dag::new();
        let ctx = GateContext {
            job_id: &id,
            snapshot: &snap,
            dag: &dag,
        };
        let outcome = CacheFreshGate.evaluate(&ctx);
        assert_eq!(outcome, GateOutcome::Pass);
    }

    /// Cache fresh → Skip. Writes a cache entry then evaluates.
    #[test]
    fn fresh_cache_skips() {
        let org = "gate-test-fresh-org-zzz9988";
        // Best-effort cleanup: leave no stale state if a prior run
        // partially succeeded.
        let path = cache::tend_cache_root()
            .join("discovery")
            .join(format!("{org}.json"));
        let _ = std::fs::remove_file(&path);

        cache::DiscoveryCache::write(org, &["repo1".into(), "repo2".into()]).unwrap();

        let id = discover_org_id(org);
        let snap = snapshot_empty();
        let dag = Dag::new();
        let ctx = GateContext {
            job_id: &id,
            snapshot: &snap,
            dag: &dag,
        };
        let outcome = CacheFreshGate.evaluate(&ctx);
        assert_eq!(outcome, GateOutcome::Skip(SkipReason::GateRejected));

        // Clean up the cache entry to avoid polluting the operator's
        // real cache dir.
        let _ = std::fs::remove_file(&path);
    }

    /// A Job that isn't subject-Org passes through. The gate is only
    /// meaningful for `DiscoverOrgJob`; registering it against another
    /// kind shouldn't deadlock that kind by always skipping.
    #[test]
    fn non_org_subject_passes() {
        let id = JobId {
            scope: JobScope::Workspace("ws".into()),
            kind: JobKindId::new("some-other-kind"),
            subject: JobSubject::Repo("repo".into()),
        };
        let snap = snapshot_empty();
        let dag = Dag::new();
        let ctx = GateContext {
            job_id: &id,
            snapshot: &snap,
            dag: &dag,
        };
        let outcome = CacheFreshGate.evaluate(&ctx);
        assert_eq!(outcome, GateOutcome::Pass);
    }

    // Silence dead-code warnings for the field in JobPhase imports used
    // only via match arms in real production code paths.
    #[allow(dead_code)]
    fn _phase_assertion(p: JobPhase) -> JobPhase {
        p
    }

    // ── NotPlaceholderGate tests ────────────────────────────────────

    use tempfile::TempDir;

    fn pull_id(repo: &str) -> JobId {
        JobId {
            scope: JobScope::Workspace("ws".into()),
            kind: JobKindId::new("tend.pull-repo"),
            subject: JobSubject::Repo(repo.into()),
        }
    }

    fn empty_ctx<'a>(id: &'a JobId, snap: &'a Snapshot, dag: &'a Dag) -> GateContext<'a> {
        GateContext {
            job_id: id,
            snapshot: snap,
            dag,
        }
    }

    #[test]
    fn unmarked_dir_passes_placeholder_gate() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("repo1")).unwrap();
        let gate = NotPlaceholderGate {
            base_dir: tmp.path().to_path_buf(),
        };
        let id = pull_id("repo1");
        let snap = snapshot_empty();
        let dag = Dag::new();
        assert_eq!(
            gate.evaluate(&empty_ctx(&id, &snap, &dag)),
            GateOutcome::Pass
        );
    }

    #[test]
    fn marked_dir_skips_placeholder_gate() {
        let tmp = TempDir::new().unwrap();
        crate::placeholder::mark_placeholder(&tmp.path().join("scratch")).unwrap();
        let gate = NotPlaceholderGate {
            base_dir: tmp.path().to_path_buf(),
        };
        let id = pull_id("scratch");
        let snap = snapshot_empty();
        let dag = Dag::new();
        assert_eq!(
            gate.evaluate(&empty_ctx(&id, &snap, &dag)),
            GateOutcome::Skip(SkipReason::GateRejected)
        );
    }

    #[test]
    fn non_repo_subject_passes_placeholder_gate() {
        let tmp = TempDir::new().unwrap();
        let gate = NotPlaceholderGate {
            base_dir: tmp.path().to_path_buf(),
        };
        let id = JobId {
            scope: JobScope::Workspace("ws".into()),
            kind: JobKindId::new("some-other-kind"),
            subject: JobSubject::Org("ws".into()),
        };
        let snap = snapshot_empty();
        let dag = Dag::new();
        assert_eq!(
            gate.evaluate(&empty_ctx(&id, &snap, &dag)),
            GateOutcome::Pass
        );
    }
}
