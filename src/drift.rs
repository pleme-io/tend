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
    /// `git pull` returned non-zero. Unclassified fallback — set by
    /// [`classify_pull_failure`] when no typed sub-class matches.
    /// Carries the trimmed stderr for triage. The typed sub-classes
    /// (`PullFailedNoUpstream`, `PullFailedBranchRenamed`,
    /// `PullFailedDiverged`, `PullFailedRepoMissing`,
    /// `PullFailedTransient`) below cover the empirically observed
    /// failure modes from the 2026-05-28 fleet sweep; new failure
    /// shapes that appear in the wild get a new variant + a classifier
    /// arm rather than living as opaque-stderr `PullFailed`.
    PullFailed {
        workspace: String,
        repo_name: String,
        stderr: String,
    },

    /// `git pull` failed because the local branch has no upstream
    /// tracking configured (`There is no tracking information for the
    /// current branch`). Auto-recoverable: `git branch
    /// --set-upstream-to=origin/<branch> <branch>` if origin has the
    /// matching branch. SAFE-CONVERGENCE M2's `AutoAct(SetUpstream)`.
    PullFailedNoUpstream {
        workspace: String,
        repo_name: String,
    },

    /// `git pull` failed because the configured upstream ref doesn't
    /// exist on the remote (`Your configuration specifies to merge
    /// with the ref 'refs/heads/X' from the remote, but no such ref
    /// was fetched`). Typically means upstream renamed the default
    /// branch (master→main, etc.) or removed the ref entirely. The
    /// existing M6 reaction kicks a `FetchRepoJob`; SAFE-CONVERGENCE
    /// M2 will add `AutoAct(RenameBranch)` for the rename case.
    PullFailedBranchRenamed {
        workspace: String,
        repo_name: String,
        expected_ref: String,
    },

    /// `git pull --ff-only` refused because local and origin diverged
    /// (`Diverging branches can't be fast-forwarded` /
    /// `Not possible to fast-forward`). Operator decision: merge,
    /// rebase, or reset. Default reaction = Escalate.
    PullFailedDiverged {
        workspace: String,
        repo_name: String,
    },

    /// Remote repo is gone (`Repository not found`, `ERROR: Repository
    /// not found`). Mark as quarantined in the workspace config until
    /// the operator decides to remove the local clone or restore the
    /// upstream. Default reaction = Escalate; SAFE-CONVERGENCE M2's
    /// `Quarantine` once the marker mechanism lands.
    PullFailedRepoMissing {
        workspace: String,
        repo_name: String,
    },

    /// Transient network or SSH-layer failure (`mux_client_request_session`,
    /// `Could not read from remote repository`, `Connection timed out`,
    /// `Connection refused`, etc.). Often clears on the next reconcile
    /// cycle — default reaction = retry next tick (no autoAct needed,
    /// the next cycle will re-attempt).
    PullFailedTransient {
        workspace: String,
        repo_name: String,
        snippet: String,
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
    /// A repo has **no git remote configured at all**. Its entire
    /// history exists on one machine and has never been pushed
    /// anywhere; every backup path the fleet has is bypassed.
    ///
    /// Reported, never auto-remediated. Where a remote *should* point
    /// is an operator decision with real destructive potential — the
    /// obvious guess (`git@github.com:<org>/<repo>.git`) can already
    /// hold unrelated content, and a force-push to it would destroy a
    /// history. Detection and reporting are the whole deliverable;
    /// `jobs::reactions` deliberately returns no handler for this.
    ///
    /// Distinct from `PullFailedNoUpstream`, which it used to be
    /// swallowed by: git emits the byte-identical
    /// `There is no tracking information for the current branch.` for
    /// both, so the stderr classifier could not tell them apart. This
    /// variant is derived from an *observation* of the remote set
    /// (`PullOutcome::NoRemoteSkipped`), not from a message.
    RepoHasNoRemote {
        workspace: String,
        repo_name: String,
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

    /// A repo's `origin` URL embeds a credential in its userinfo —
    /// `https://x-access-token:<token>@github.com/org/repo.git`.
    ///
    /// Produced by `sync::inject_github_token`, which rewrites the
    /// clone URL so container clones can authenticate without a
    /// prompt. Git persists the clone URL verbatim into `.git/config`,
    /// so the token that was live at clone time is fossilized there
    /// indefinitely. The 2026-07-29 sweep found 25 such repos across
    /// two orgs carrying three distinct tokens, one still valid.
    ///
    /// `credential` is the **redacted shape** (`x-access-token:***`),
    /// never the secret — this event is serialized into
    /// `drift-events.jsonl`, and carrying the raw URL would copy the
    /// token out of `.git/config` and into the audit log. See
    /// `remote_url::Credential`.
    ///
    /// Auto-remediated, unlike [`Self::RepoHasNoRemote`]. The
    /// distinction is that healing here does not choose *where* the
    /// remote points: the canonical URL is rebuilt from the coordinate
    /// parsed out of the offending URL, so the remote keeps its target
    /// and loses only the credential. There is no destructive reading.
    RemoteUrlEmbeddedCredential {
        workspace: String,
        repo_name: String,
        slug: String,
        credential: String,
    },

    /// A repo's `origin` URL is a clean GitHub URL whose protocol
    /// disagrees with the workspace's declared `clone_method` — e.g.
    /// HTTPS under a workspace declaring `ssh`.
    ///
    /// Convergence drift rather than a leak: nothing secret is on
    /// disk, but the repo will keep prompting for credentials (or
    /// keep depending on a credential helper) instead of using the
    /// declared transport. Healed the same way, by rewriting to the
    /// canonical URL for the declared method.
    RemoteProtocolMismatch {
        workspace: String,
        repo_name: String,
        slug: String,
        declared: String,
        actual: String,
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
            | Self::PullFailedNoUpstream { workspace, .. }
            | Self::PullFailedBranchRenamed { workspace, .. }
            | Self::PullFailedDiverged { workspace, .. }
            | Self::PullFailedRepoMissing { workspace, .. }
            | Self::PullFailedTransient { workspace, .. }
            | Self::SyncFailed { workspace, .. }
            | Self::RepoHasNoRemote { workspace, .. }
            | Self::LocalRepoNotInDiscovery { workspace, .. }
            | Self::RemoteUrlEmbeddedCredential { workspace, .. }
            | Self::RemoteProtocolMismatch { workspace, .. } => Some(workspace.as_str()),
            Self::JobUnhealed { job_id, .. } => match &job_id.scope {
                shigoto_types::JobScope::Workspace(w) => Some(w.as_str()),
                shigoto_types::JobScope::Repo { workspace, .. } => Some(workspace.as_str()),
                shigoto_types::JobScope::Global => None,
            },
        }
    }
}

/// Classify a `git pull` stderr into the most specific typed
/// `DriftEvent` variant. Patterns are matched in priority order;
/// the catch-all `PullFailed` is returned only when no typed pattern
/// matches.
///
/// The priority order is set by the empirical clustering from the
/// 2026-05-28 fleet sweep — `BranchRenamed` is the most common
/// (~18 of 23), `Diverged` is next, `NoUpstream` is third. Order
/// matters only when two predicates could match (which shouldn't
/// happen in practice, but is defensive).
pub(crate) fn classify_pull_failure(
    workspace: &str,
    repo_name: &str,
    stderr: &str,
) -> DriftEvent {
    // BranchRenamed / ref-not-fetched: try to extract the ref name
    // from the message body ("refs/heads/<X>") for traceability.
    if stderr.contains("no such ref was fetched") {
        let expected_ref = parse_expected_ref(stderr).unwrap_or_else(|| "refs/heads/main".into());
        return DriftEvent::PullFailedBranchRenamed {
            workspace: workspace.to_string(),
            repo_name: repo_name.to_string(),
            expected_ref,
        };
    }
    if stderr.contains("Repository not found")
        || stderr.contains("ERROR: Repository not found")
    {
        return DriftEvent::PullFailedRepoMissing {
            workspace: workspace.to_string(),
            repo_name: repo_name.to_string(),
        };
    }
    if stderr.contains("Diverging branches can't be fast-forwarded")
        || stderr.contains("Not possible to fast-forward")
    {
        return DriftEvent::PullFailedDiverged {
            workspace: workspace.to_string(),
            repo_name: repo_name.to_string(),
        };
    }
    // NOTE (2026-07-28): this arm is reachable ONLY for a repo that has
    // a remote. Git emits this exact message for both "has origin, this
    // branch has no tracking" and "has no remote at all" — verified
    // byte-identical — so stderr alone cannot distinguish them and this
    // classifier must not try. `pull_one_repo` observes the remote set
    // first and returns `PullOutcome::NoRemoteSkipped`, so the
    // remote-less case never reaches classification. Without that
    // upstream observation, this arm silently absorbed every remote-less
    // repo and pointed the operator at a `--set-upstream-to=origin/…`
    // remedy that cannot work when there is no origin.
    if stderr.contains("There is no tracking information for the current branch") {
        return DriftEvent::PullFailedNoUpstream {
            workspace: workspace.to_string(),
            repo_name: repo_name.to_string(),
        };
    }
    if stderr.contains("mux_client_request_session")
        || stderr.contains("Could not read from remote repository")
        || stderr.contains("Connection timed out")
        || stderr.contains("Connection refused")
        || stderr.contains("Connection reset")
    {
        let snippet = stderr
            .lines()
            .find(|l| {
                l.contains("mux_client")
                    || l.contains("Could not read")
                    || l.contains("Connection")
            })
            .unwrap_or(stderr)
            .trim()
            .to_string();
        return DriftEvent::PullFailedTransient {
            workspace: workspace.to_string(),
            repo_name: repo_name.to_string(),
            snippet,
        };
    }
    // Catch-all: unclassified failure modes still surface, but new
    // empirical patterns get a new variant + classifier arm rather
    // than living as opaque-stderr forever.
    DriftEvent::PullFailed {
        workspace: workspace.to_string(),
        repo_name: repo_name.to_string(),
        stderr: stderr.to_string(),
    }
}

/// Typed bundle of the inputs [`classify_pull_failure`] needs, so the
/// classification is reachable through shigoto's `Classifier<I, O>`
/// primitive (which classifies a single `&I`).
pub(crate) struct PullFailureContext {
    pub workspace: String,
    pub repo_name: String,
    pub stderr: String,
}

/// `classify_pull_failure` exposed as a typed
/// [`shigoto_types::classify::Classifier`] — the same delegate-to-
/// free-fn shape as the canonical
/// `shigoto_types::classify::FailureClassifier`. Lets pull-failure
/// classification compose + mock polymorphically instead of being a
/// bespoke free fn (Phase 0.2b convergence adoption — the if-chain
/// stays the single source of truth; this is the typed surface over it).
#[derive(Debug, Default, Copy, Clone)]
pub(crate) struct PullFailureClassifier;

impl shigoto_types::classify::Classifier<PullFailureContext, DriftEvent> for PullFailureClassifier {
    fn classify(&self, ctx: &PullFailureContext) -> DriftEvent {
        classify_pull_failure(&ctx.workspace, &ctx.repo_name, &ctx.stderr)
    }
}

/// Extract the expected ref from a "no such ref was fetched" message.
/// The git message has the form:
///   `Your configuration specifies to merge with the ref
///    'refs/heads/<X>' from the remote, but no such ref was fetched.`
/// Returns `Some("refs/heads/<X>")` when the quoted ref is present,
/// `None` when the message format has shifted.
fn parse_expected_ref(stderr: &str) -> Option<String> {
    let after = stderr.split("merge with the ref ").nth(1)?;
    let inside = after.split('\'').nth(1)?;
    if inside.is_empty() {
        None
    } else {
        Some(inside.to_string())
    }
}

/// Receivers of `DriftEvent`. Thin trait over the canonical
/// `shigoto_types::sink::Sink<DriftEvent>` so every tend caller
/// writing `&dyn DriftSink` keeps working unchanged after the
/// theory/CONVERGENCE-ADOPTION.md Phase 0.1 extraction. The blanket
/// impl below means any `Sink<DriftEvent>` impl auto-satisfies
/// `DriftSink` — no per-impl wiring at the consumer side.
///
/// The concrete impls (`NullDriftSink`, `InMemoryDriftSink`,
/// `AuditFileDriftSink`, plus the new `MultiDriftSink` for fan-out)
/// are now type aliases over the generic `shigoto_types::sink::*`
/// shapes — same behavior, fleet-wide reuse, ~100 lines of duplicate
/// code deleted.
pub(crate) trait DriftSink: Send + Sync {
    fn record(&self, event: &DriftEvent);
}

impl<T: shigoto_types::sink::Sink<DriftEvent> + ?Sized> DriftSink for T {
    fn record(&self, event: &DriftEvent) {
        shigoto_types::sink::Sink::record(self, event)
    }
}

pub(crate) type NullDriftSink = shigoto_types::sink::NullSink<DriftEvent>;
pub(crate) type InMemoryDriftSink = shigoto_types::sink::InMemorySink<DriftEvent>;
pub(crate) type AuditFileDriftSink = shigoto_types::sink::AuditFileSink<DriftEvent>;
#[allow(dead_code)]
pub(crate) type MultiDriftSink = shigoto_types::sink::MultiSink<DriftEvent>;

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
            PullOutcome::NoRemoteSkipped => events.push(DriftEvent::RepoHasNoRemote {
                workspace: receipt.workspace.clone(),
                repo_name,
            }),
            PullOutcome::Failed { stderr } => events.push(classify_pull_failure(
                &receipt.workspace,
                &repo_name,
                stderr,
            )),
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

/// Observe every present repo's `origin` URL and project the
/// non-conforming ones into `DriftEvent`s.
///
/// Separate from [`derive_from_receipt`] because this drift is not a
/// projection of a Job outcome — it is a direct observation of
/// `.git/config`, in the same spirit as `RepoHasNoRemote` (derived
/// from observing the remote set, not from parsing a message). A
/// fossilized credential produces no pull failure at all: the clone
/// works fine, which is precisely why the 25-repo leak went unnoticed
/// for months. Nothing in the receipt could have revealed it.
///
/// Missing repos and repos with no remote are skipped silently — both
/// already have their own typed drift (`MissingSkipped`,
/// `RepoHasNoRemote`), and re-reporting them here would double-count.
/// An unreadable repo is skipped rather than escalated: a failed
/// observation is inconclusive, and this pass must never turn a
/// transient read error into a rewrite recommendation.
pub(crate) fn derive_remote_url_drift(
    workspace: &crate::config::Workspace,
    repo_names: &[String],
) -> Vec<DriftEvent> {
    use crate::remote_url::{classify, RemoteUrlVerdict};
    use crate::sync::RemoteWitness;

    let Ok(base) = workspace.resolved_base_dir() else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for repo_name in repo_names {
        let path = base.join(repo_name);
        if !path.join(".git").exists() {
            continue;
        }
        let Ok(Some(witness)) = RemoteWitness::observe(&path) else {
            continue;
        };

        match classify(witness.url(), &workspace.clone_method) {
            RemoteUrlVerdict::EmbeddedCredential { slug, credential } => {
                events.push(DriftEvent::RemoteUrlEmbeddedCredential {
                    workspace: workspace.name.clone(),
                    repo_name: repo_name.clone(),
                    slug: slug.slug(),
                    credential: credential.shape().to_string(),
                });
            }
            RemoteUrlVerdict::ProtocolMismatch { slug, actual } => {
                events.push(DriftEvent::RemoteProtocolMismatch {
                    workspace: workspace.name.clone(),
                    repo_name: repo_name.clone(),
                    slug: slug.slug(),
                    declared: workspace.clone_method.to_string(),
                    actual: actual.to_string(),
                });
            }
            RemoteUrlVerdict::Conforming | RemoteUrlVerdict::OutOfScope => {}
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
    fn pull_failed_unclassified_carries_stderr() {
        // Stderr that matches NO typed pattern should fall through to
        // the catch-all PullFailed variant — preserves operator
        // visibility into novel failure modes while typed coverage
        // catches up.
        let mut r = empty_receipt();
        r.outcomes.insert(
            pull_id("novel"),
            PullOutcome::Failed {
                stderr: "totally novel error message".into(),
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
                assert_eq!(repo_name, "novel");
                assert!(stderr.contains("novel error"));
            }
            other => panic!("expected PullFailed, got {other:?}"),
        }
    }

    // ── classifier tests ────────────────────────────────────────────
    //
    // Each canonical git error message from the 2026-05-28 fleet sweep
    // gets a typed-variant assertion. Adding a new variant here keeps
    // the typed surface a forcing function: an unclassified message
    // surfaces as plain `PullFailed`, never silently masked.

    #[test]
    fn classifier_no_such_ref_becomes_branch_renamed() {
        let stderr = "Your configuration specifies to merge with the ref \
            'refs/heads/main'\nfrom the remote, but no such ref was fetched.";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedBranchRenamed {
                workspace,
                repo_name,
                expected_ref,
            } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "r");
                assert_eq!(expected_ref, "refs/heads/main");
            }
            other => panic!("expected PullFailedBranchRenamed, got {other:?}"),
        }
    }

    /// The typed `PullFailureClassifier` produces the same typed event
    /// as the free fn it wraps — the Phase-0.2b adoption is behaviour-
    /// preserving, just polymorphic.
    #[test]
    fn pull_failure_classifier_trait_delegates_to_free_fn() {
        use shigoto_types::classify::Classifier;
        let ctx = PullFailureContext {
            workspace: "ws".into(),
            repo_name: "r".into(),
            stderr: "ERROR: Repository not found".into(),
        };
        match PullFailureClassifier.classify(&ctx) {
            DriftEvent::PullFailedRepoMissing { workspace, repo_name } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "r");
            }
            other => panic!("expected PullFailedRepoMissing, got {other:?}"),
        }
    }

    #[test]
    fn classifier_no_such_ref_extracts_non_main_ref() {
        // akeyless-go is on branch v5; the expected ref differs.
        let stderr = "Your configuration specifies to merge with the ref \
            'refs/heads/v5'\nfrom the remote, but no such ref was fetched.";
        match classify_pull_failure("ws", "akeyless-go", stderr) {
            DriftEvent::PullFailedBranchRenamed { expected_ref, .. } => {
                assert_eq!(expected_ref, "refs/heads/v5");
            }
            other => panic!("expected PullFailedBranchRenamed with v5 ref, got {other:?}"),
        }
    }

    #[test]
    fn classifier_diverging_branches_becomes_diverged() {
        let stderr = "hint: Diverging branches can't be fast-forwarded, \
            you need to either:\nfatal: Not possible to fast-forward, aborting.";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedDiverged { workspace, repo_name } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "r");
            }
            other => panic!("expected PullFailedDiverged, got {other:?}"),
        }
    }

    #[test]
    fn classifier_no_tracking_becomes_no_upstream() {
        let stderr = "There is no tracking information for the current branch.\n\
            Please specify which branch you want to merge with.";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedNoUpstream { workspace, repo_name } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "r");
            }
            other => panic!("expected PullFailedNoUpstream, got {other:?}"),
        }
    }

    #[test]
    fn classifier_repository_not_found_becomes_repo_missing() {
        let stderr = "ERROR: Repository not found.\nfatal: Could not read from remote repository.";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedRepoMissing { workspace, repo_name } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "r");
            }
            other => panic!("expected PullFailedRepoMissing, got {other:?}"),
        }
    }

    #[test]
    fn classifier_mux_client_becomes_transient() {
        let stderr = "mux_client_request_session: session request failed: \
            Session open refused by peer";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedTransient { snippet, .. } => {
                assert!(snippet.contains("mux_client"));
            }
            other => panic!("expected PullFailedTransient, got {other:?}"),
        }
    }

    #[test]
    fn classifier_unknown_pattern_falls_through_to_pull_failed() {
        let stderr = "some entirely new failure mode we haven't seen yet";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailed { stderr: s, .. } => {
                assert_eq!(s, stderr);
            }
            other => panic!("expected fallback PullFailed, got {other:?}"),
        }
    }

    #[test]
    fn classifier_priority_repo_missing_over_transient() {
        // "Repository not found" + "Could not read" both present —
        // RepoMissing takes priority (more specific).
        let stderr = "ERROR: Repository not found.\nfatal: Could not read from remote repository.";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedRepoMissing { .. } => {}
            other => panic!("expected RepoMissing priority, got {other:?}"),
        }
    }

    #[test]
    fn parse_expected_ref_extracts_quoted_ref() {
        let msg = "Your configuration specifies to merge with the ref \
            'refs/heads/v5' from the remote, but no such ref was fetched.";
        assert_eq!(parse_expected_ref(msg), Some("refs/heads/v5".into()));
    }

    #[test]
    fn parse_expected_ref_returns_none_on_shifted_format() {
        assert_eq!(parse_expected_ref("unrelated text"), None);
        assert_eq!(
            parse_expected_ref("merge with the ref without quotes"),
            None
        );
    }

    /// A remote-less repo surfaces as its own typed finding, NOT as
    /// `PullFailedNoUpstream` (whose remedy assumes an `origin` that
    /// does not exist) and NOT silently.
    #[test]
    fn no_remote_outcome_becomes_repo_has_no_remote_drift() {
        let mut r = empty_receipt();
        r.outcomes
            .insert(pull_id("ferrite-zig"), PullOutcome::NoRemoteSkipped);

        let events = derive_from_receipt(&r);
        assert_eq!(events.len(), 1, "expected exactly one finding; got {events:?}");
        match &events[0] {
            DriftEvent::RepoHasNoRemote {
                workspace,
                repo_name,
            } => {
                assert_eq!(workspace, "ws");
                assert_eq!(repo_name, "ferrite-zig");
            }
            other => panic!("expected RepoHasNoRemote, got {other:?}"),
        }
    }

    /// The stderr classifier must NOT be taught to sniff for the
    /// remote-less case: git's message is byte-identical to the
    /// has-remote-but-no-tracking case, so any string test here would be
    /// guessing. This test pins that the no-tracking message keeps
    /// meaning "has a remote, lacks tracking" — the remote-less case is
    /// caught upstream by observation in `pull_one_repo`.
    #[test]
    fn no_tracking_stderr_still_means_no_upstream_not_no_remote() {
        let stderr = "There is no tracking information for the current branch.\n\
            Please specify which branch you want to merge with.";
        match classify_pull_failure("ws", "r", stderr) {
            DriftEvent::PullFailedNoUpstream { .. } => {}
            other => panic!("expected PullFailedNoUpstream, got {other:?}"),
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
