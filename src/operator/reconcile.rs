//! Reconcile loops for `FlakeUpdatePolicy` and `FlakeUpdateProposal`.
//!
//! Each Controller has its own reconcile fn; the two share a
//! `Context` with the kube client, parsed tend config, and an HTTP
//! client for upstream HEAD lookups.

use anyhow::Result;
use chrono::Utc;
use kube::{
    api::{Api, ObjectMeta, Patch, PatchParams, PostParams},
    runtime::controller::Action,
    Client, Resource, ResourceExt,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use super::apply;
use super::crds::{
    Condition, FlakeUpdatePlan, FlakeUpdatePlanSpec, FlakeUpdatePolicy,
    FlakeUpdatePolicyStatus, FlakeUpdateProposal, FlakeUpdateProposalSpec, ProposalPhase,
    UpdateMode,
};
use super::discovery::{self, ReqwestHeadResolver};
use super::gates;
use super::metrics::metrics;
use super::workspace::resolve_repo_dir;
use crate::config::Config;
use crate::flake_lock::ExtendedLockFile;

/// Default cadence for clean reconciles. 30 minutes (was 5 minutes
/// before the under-the-radar redesign). Conditional requests with
/// ETag mean the typical poll is free anyway, so cadence here only
/// gates "did we miss a real change?" — half-hour is plenty for the
/// fleet update use case where humans aren't waiting on millisecond
/// reaction. Jittered ±30% so cycles never align on the wallclock.
const REQUEUE_OK: Duration = Duration::from_secs(1800);

/// Backoff after a transient error. 5 minutes (was 30 seconds). The
/// previous 30s aggressive retry made transient failures look like
/// scraper hammering — a 5-minute pause gives upstream time to settle
/// and is still tight enough that a real outage is noticed quickly.
const REQUEUE_FAST: Duration = Duration::from_secs(300);

pub struct Context {
    pub client: Client,
    pub tend_config: Arc<Config>,
    pub http: reqwest::Client,
    pub github_token: Option<String>,
    /// Per-repo working-tree mutex registry. The policy reconciler's
    /// discovery-side fetch+reset and the proposal reconciler's
    /// apply-side fetch+reset can both fire on the same `repo_dir`
    /// — without serialization they collide on `.git/index.lock`.
    /// Each git operation acquires the mutex keyed on the absolute
    /// repo path before touching the working tree.
    pub repo_locks: Arc<tokio::sync::Mutex<
        std::collections::HashMap<std::path::PathBuf, Arc<tokio::sync::Mutex<()>>>,
    >>,
    /// File-backed HEAD-lookup cache. Survives pod restarts via
    /// `flush()` to a JSON file on the workspace PVC. Each entry
    /// carries an ETag so conditional requests return 304 (free)
    /// when nothing changed.
    pub head_cache: Arc<super::head_cache::PersistentHeadCache>,
    /// Sliding-window rate budget shared across every domain
    /// controller. Enforces a per-pod hourly cap on outgoing requests
    /// (default 100/hr — 2% of GitHub's authenticated quota) and
    /// adapts down when GitHub signals pressure via X-RateLimit-Remaining.
    pub budget: Arc<super::budget::RequestBudget>,
}

impl Context {
    /// Get-or-insert the mutex for `repo_dir`. Returns a clone — the
    /// caller awaits `.lock()` to serialize against other operations
    /// on the same path.
    pub async fn repo_lock(
        &self,
        repo_dir: &std::path::Path,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut registry = self.repo_locks.lock().await;
        registry
            .entry(repo_dir.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    #[error("kube error: {0}")]
    Kube(#[from] kube::Error),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

// ─── FlakeUpdatePolicy reconciler ───────────────────────────────────

pub async fn reconcile_policy(
    policy: Arc<FlakeUpdatePolicy>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    let ns = policy.namespace().unwrap_or_else(|| "default".into());
    let name = policy.name_any();
    info!(ns = %ns, policy = %name, "reconciling FlakeUpdatePolicy");

    let repo_dir = resolve_repo_dir(&ctx.tend_config, &policy.spec.repo)?;
    let exists = repo_dir.try_exists().unwrap_or(false);
    let canon = repo_dir.canonicalize().ok();
    info!(
        repo_dir = %repo_dir.display(),
        exists,
        canonical = ?canon,
        workspaces_known = ctx.tend_config.workspaces.len(),
        "resolved repo dir"
    );
    if !exists {
        return write_policy_failure(
            &ctx,
            &policy,
            format!(
                "repo dir does not exist: {} (canonical: {:?}, try_exists: {:?})",
                repo_dir.display(),
                canon,
                repo_dir.try_exists()
            ),
        )
        .await
        .map(|_| Action::requeue(REQUEUE_OK));
    }

    // Discovery must read flake.lock at *origin*/main, not whatever
    // the pod's clone has cached. Without this, a failed apply leaves
    // a local-only commit with the proposed rev pinned — discovery
    // then sees no advance and stops generating proposals for that
    // input. The reset is safe: the pod clone is a scratch workspace,
    // we rebuild from origin every cycle.
    //
    // Lock the working tree against concurrent ops (the proposal
    // reconciler's apply path also fetch+resets here). Holding the
    // lock for the full duration of read+parse means discovery can't
    // race apply on `.git/index.lock`.
    let lock_arc = ctx.repo_lock(&repo_dir).await;
    let _wt_guard = lock_arc.lock().await;

    if let Err(e) = super::git_ops::fetch_and_reset_to_origin(
        &repo_dir,
        "main",
        ctx.github_token.as_deref(),
    )
    .await
    {
        return write_policy_failure(
            &ctx,
            &policy,
            format!("fetch+reset to origin/main: {e:#}"),
        )
        .await
        .map(|_| Action::requeue(REQUEUE_FAST));
    }

    let lock_path = repo_dir.join("flake.lock");
    let lock_contents = match tokio::fs::read_to_string(&lock_path).await {
        Ok(s) => s,
        Err(e) => {
            return write_policy_failure(
                &ctx,
                &policy,
                format!("reading {}: {e}", lock_path.display()),
            )
            .await
            .map(|_| Action::requeue(REQUEUE_OK));
        }
    };

    let lock = ExtendedLockFile::parse(&lock_contents)
        .map_err(|e| ReconcileError::Other(anyhow::anyhow!("parse flake.lock: {e}")))?;

    // Source of truth for "what inputs the user actually declared":
    // flake.nix, parsed via rnix. flake.lock can hold stale entries
    // (input removed from flake.nix but `nix flake update` not yet
    // run). Without this filter, discovery would create proposals for
    // ghosts that can never apply. We degrade gracefully — if
    // flake.nix is unreadable or unparsable, we log a warn and fall
    // back to the lock's view (preserves prior behavior).
    let flake_nix_path = repo_dir.join("flake.nix");
    let declared_inputs = match super::flake_nix::read_flake_inputs(&flake_nix_path) {
        Ok(set) => Some(set),
        Err(e) => {
            tracing::warn!(
                path = %flake_nix_path.display(),
                error = %e,
                "flake.nix unreadable/unparsable; discovery degrading to lock-only view",
            );
            None
        }
    };

    // Generate the typed plan BEFORE any HTTP traffic. Plan CRD
    // becomes the durable audit trail: which inputs we'll check, why,
    // what budget remains. Operators can `kubectl describe fpl` to
    // see what discovery is about to do — auditability +
    // reproducibility.
    let plan_output = {
        let cache_guard = ctx.head_cache.lock().await;
        super::planner::plan(
            &policy,
            &lock,
            declared_inputs.as_ref(),
            &cache_guard,
            &ctx.budget,
        )
    };
    // Emit the plan CR. Best effort — failure to persist the plan
    // shouldn't abort the cycle, but operators lose audit on this run.
    if let Err(e) = upsert_plan(&ctx, &policy, &plan_output.spec).await {
        tracing::warn!(error = %format!("{e:#}"), "FlakeUpdatePlan upsert failed");
    }

    let resolver = ReqwestHeadResolver {
        client: ctx.http.clone(),
        token: ctx.github_token.clone(),
        budget: ctx.budget.clone(),
    };
    // Hold the persistent cache across cycles. Each entry has an
    // ETag; conditional requests turn most "is X advanced?" polls
    // into 304 (free) responses. Lock for the duration of discovery
    // so concurrent reconciles can't double-fetch the same upstream
    // while the in-flight cache lookup is still in progress.
    let outcome = {
        let mut head_cache_guard = ctx.head_cache.lock().await;
        discovery::discover_advances_filtered(
            &lock,
            &resolver,
            declared_inputs.as_ref(),
            &mut head_cache_guard,
        )
        .await
        .map_err(|e| ReconcileError::Other(anyhow::anyhow!("discovery: {e}")))?
    };
    // Persist cache to disk after each discovery cycle so a pod
    // restart doesn't pay full quota on the next first cycle. Best
    // effort — flush failure is logged but doesn't abort the cycle.
    if let Err(e) = ctx.head_cache.flush().await {
        tracing::warn!(error = %format!("{e:#}"), "head cache flush failed");
    }

    // Split the outcome — the partial advances are still upsert-worthy
    // (they're real upstream advances we observed before the halt),
    // and the halt reason becomes a status condition so operators see
    // why the cycle didn't complete fully. Rate-limit halts also
    // request a longer requeue so we don't push the registry harder.
    let (advances, halt_reason, requeue) = match outcome {
        discovery::DiscoveryOutcome::Advances(a) => (a, None, REQUEUE_OK),
        discovery::DiscoveryOutcome::Halted { partial, reason } => {
            // 30 min backoff on rate limit; reset_at would be cleaner
            // but we don't want to over-engineer the wakeup math now.
            let r = if matches!(reason, super::upstream::RegistryError::RateLimited { .. }) {
                Duration::from_secs(1800)
            } else {
                REQUEUE_OK
            };
            (partial, Some(reason), r)
        }
    };

    // Build a typed dependency DAG from the lock's follows edges and
    // partition the *advancing* inputs into waves. Wave 0 = inputs
    // with no advancing parents; Wave N = inputs whose advancing
    // parents all live in waves [0..N). This gives operators a
    // deterministic ordering for rollout — substrate's bump shows up
    // in Wave 0, downstream consumers in Wave 1+.
    //
    // The wave is recorded as `status.dag_wave` on each proposal at
    // create time so a future rollout controller can sequence wave
    // promotion. Today's proposal controller verifies + applies in
    // parallel; the wave field is informational until that lands.
    let repo_key = format!(
        "{}/{}",
        policy.spec.repo.workspace, policy.spec.repo.repo
    );
    let mut dag = super::dag::FleetDag::new();
    // Seed every advancing input as a node first so leaves (inputs
    // with no `follows` siblings) still receive a wave assignment.
    let advancing_set: std::collections::BTreeSet<super::dag::DagNodeId> = advances
        .iter()
        .map(|a| super::dag::DagNodeId::new(&repo_key, &a.input))
        .collect();
    for id in &advancing_set {
        dag.ensure_node(id.clone());
    }
    // Restrict the DAG to root-direct inputs only — `lock.edges()`
    // returns the full transitive graph, which can contain cycles
    // among deep cargo-style transitive entries (`crate2nix_stable_8`
    // diamonds, etc.) that have no bearing on what *the user* can
    // bump. For wave assignment we only care about precedence between
    // root-direct local input names.
    //
    // Direction inversion: `lock.edges()` returns `(dependent →
    // dependency)` (X.inputs.Y = Y means X depends on Y). For wave
    // assignment we want `(dependency → dependent)` so the dependency
    // lands in an earlier wave. Swap the pair before adding.
    let local_of_node: std::collections::BTreeMap<String, String> = lock
        .root_input_nodes()
        .into_iter()
        .map(|(local, node)| (node, local))
        .collect();
    if let Ok(edges) = super::flake_lock_adapter::FlakeLockAdapter
        .edges_from_raw(&lock_contents)
    {
        for (dependent_node, dependency_node) in edges {
            if dependent_node == lock.root || dependency_node == lock.root {
                continue;
            }
            let (Some(dependent_local), Some(dependency_local)) = (
                local_of_node.get(&dependent_node),
                local_of_node.get(&dependency_node),
            ) else {
                // One endpoint isn't a root-direct input — out of
                // scope for proposal-level wave assignment.
                continue;
            };
            // Inverted: dependency precedes dependent.
            dag.add_edge(
                super::dag::DagNodeId::new(&repo_key, dependency_local),
                super::dag::DagNodeId::new(&repo_key, dependent_local),
            );
        }
    }
    let wave_of: BTreeMap<String, u32> = match dag.waves(Some(&advancing_set)) {
        Ok(waves) => waves
            .into_iter()
            .enumerate()
            .flat_map(|(idx, w)| {
                w.into_iter().map(move |id| (id.input, idx as u32))
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "DAG planning failed; proposals get wave=None");
            BTreeMap::new()
        }
    };

    let proposals: Api<FlakeUpdateProposal> = Api::namespaced(ctx.client.clone(), &ns);
    for adv in &advances {
        let mode = mode_for_input(&policy, &adv.input);
        if matches!(mode, UpdateMode::Locked | UpdateMode::Forbidden) {
            continue;
        }
        let auto_approve = matches!(mode, UpdateMode::Auto);
        let dag_wave = wave_of.get(&adv.input).copied();
        upsert_proposal(&proposals, &policy, adv, auto_approve, dag_wave).await?;
    }

    // Refresh status: tracked inputs from current flake.lock.
    let tracked: Vec<String> = lock
        .nodes
        .keys()
        .filter(|k| *k != &lock.root)
        .cloned()
        .collect();

    let mut conditions = if let Some(reason) = halt_reason {
        vec![Condition {
            r#type: "Reconciled".into(),
            status: "False".into(),
            reason: Some(reason.condition_reason().to_string()),
            message: Some(format!("discovery halted: {}", reason.condition_message())),
            last_transition_time: Some(Utc::now()),
        }]
    } else {
        vec![ok_condition("Reconciled", "policy reconciled cleanly")]
    };

    // Carry over last_transition_time when status didn't flip — kube-rs
    // Controller re-fires reconcile on every status write, so without
    // byte-stable status writes we'd loop forever (every cycle bumps
    // resourceVersion → watch fires → reconcile → patch → loop).
    let prev_status = policy.status.as_ref();
    let prev_conditions = prev_status.map(|s| s.conditions.as_slice()).unwrap_or(&[]);
    super::status::stabilize_conditions(&mut conditions, prev_conditions);

    let new_status = FlakeUpdatePolicyStatus {
        tracked_inputs: tracked,
        last_observed_lock_hash: Some(lock_fingerprint(&lock_contents)),
        conditions,
        observed_generation: policy.metadata.generation.unwrap_or(0),
    };

    if prev_status != Some(&new_status) {
        let policies: Api<FlakeUpdatePolicy> = Api::namespaced(ctx.client.clone(), &ns);
        let patch = serde_json::json!({ "status": new_status });
        policies
            .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
    }

    // Jittered requeue — randomize 0-30% so multiple pods (and the
    // policy/proposal reconcilers within one pod) don't synchronize
    // into a tight cron-shaped polling pattern. GitHub's behavioral
    // abuse detector flags request bursts that align on the wallclock;
    // jitter scrambles that signal.
    Ok(Action::requeue(super::budget::jittered(requeue, 0.30)))
}

pub fn policy_error_policy(
    _policy: Arc<FlakeUpdatePolicy>,
    err: &ReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    metrics().reconcile_errors_total.with_label_values(&["FlakeUpdatePolicy"]).inc();
    warn!(error = %err, "FlakeUpdatePolicy reconcile error; retrying");
    Action::requeue(super::budget::jittered(REQUEUE_FAST, 0.30))
}

fn mode_for_input(policy: &FlakeUpdatePolicy, input: &str) -> UpdateMode {
    policy
        .spec
        .inputs
        .get(input)
        .copied()
        .unwrap_or(policy.spec.default_mode)
}

/// Persist the plan as a `FlakeUpdatePlan` CR. One plan per policy
/// per cycle — the resource name is derived from the policy + a
/// truncated timestamp so the plan history is browseable but
/// doesn't accumulate unbounded (Kubernetes GC handles cleanup via
/// owner reference + a future TTL controller).
async fn upsert_plan(
    ctx: &Context,
    policy: &FlakeUpdatePolicy,
    spec: &FlakeUpdatePlanSpec,
) -> Result<(), ReconcileError> {
    let ns = policy.namespace().unwrap_or_else(|| "default".into());
    let plans: Api<FlakeUpdatePlan> = Api::namespaced(ctx.client.clone(), &ns);
    let policy_name = policy.name_any();
    // Plan name: <policy>-<unix_timestamp_minute>. Granular enough
    // that high-frequency reconciles don't collide; coarse enough
    // that operators see a manageable list.
    let bucket = spec.generated_at.timestamp() / 60;
    let plan_name = format!("{policy_name}-{bucket}");
    if plans.get_opt(&plan_name).await?.is_some() {
        // Same minute, same policy — overwrite spec to capture the
        // latest plan (later reconciles in the same minute generally
        // reflect more current state).
        let patch = serde_json::json!({ "spec": spec });
        plans
            .patch(
                &plan_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
        return Ok(());
    }
    let plan = FlakeUpdatePlan {
        metadata: ObjectMeta {
            name: Some(plan_name),
            namespace: Some(ns),
            labels: Some(
                [("fleet.pleme.io/policy".into(), policy_name)]
                    .into_iter()
                    .collect(),
            ),
            owner_references: Some(vec![policy.controller_owner_ref(&()).unwrap()]),
            ..Default::default()
        },
        spec: spec.clone(),
        status: None,
    };
    plans.create(&PostParams::default(), &plan).await?;
    Ok(())
}

async fn upsert_proposal(
    api: &Api<FlakeUpdateProposal>,
    policy: &FlakeUpdatePolicy,
    adv: &discovery::CandidateAdvance,
    auto_approve: bool,
    dag_wave: Option<u32>,
) -> Result<(), ReconcileError> {
    let policy_name = policy.name_any();
    let policy_ns = policy.namespace().unwrap_or_else(|| "default".into());
    let prop_name = proposal_name(&policy_name, &adv.input, &adv.to.rev);

    if let Some(existing) = api.get_opt(&prop_name).await? {
        // Promote `approved=false → true` when the policy mode changes
        // from Gated to Auto for an input whose proposal already exists.
        // Never downgrade — human approvals should stick even if the
        // policy later changes back to Gated. Without this patch, an
        // operator updating policy mode in-place wouldn't take effect
        // until the proposal aged out and got recreated.
        if auto_approve && !existing.spec.approved {
            let patch = serde_json::json!({ "spec": { "approved": true } });
            api.patch(&prop_name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
        }
        return Ok(());
    }

    let mut proposal = FlakeUpdateProposal {
        metadata: ObjectMeta {
            name: Some(prop_name.clone()),
            namespace: Some(policy_ns.clone()),
            labels: Some(
                [
                    ("fleet.pleme.io/policy".into(), policy_name.clone()),
                    ("fleet.pleme.io/input".into(), adv.input.clone()),
                ]
                .into_iter()
                .collect(),
            ),
            owner_references: Some(vec![policy.controller_owner_ref(&()).unwrap()]),
            ..Default::default()
        },
        spec: FlakeUpdateProposalSpec {
            repo: policy.spec.repo.clone(),
            input: adv.input.clone(),
            from: adv.from.clone(),
            to: adv.to.clone(),
            discovered_at: Utc::now(),
            policy_namespace: policy_ns,
            policy_name: policy_name.clone(),
            approved: auto_approve,
        },
        status: None,
    };
    let _ = &mut proposal; // silence unused mut when wave is None
    api.create(&PostParams::default(), &proposal).await?;
    // Stamp the wave on status after creation — status is a subresource
    // so it can't be set via the initial create. Skip when None (no
    // DAG signal available, e.g. cycle in lock or empty advancing set).
    if let Some(wave) = dag_wave {
        let patch = serde_json::json!({ "status": { "dagWave": wave } });
        api.patch_status(&prop_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
    }
    Ok(())
}

fn proposal_name(policy_name: &str, input: &str, to_rev: &str) -> String {
    let short = if to_rev.len() > 12 { &to_rev[..12] } else { to_rev };
    let raw = format!("{policy_name}-{input}-{short}");
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect()
}

async fn write_policy_failure(
    ctx: &Context,
    policy: &FlakeUpdatePolicy,
    msg: String,
) -> Result<(), ReconcileError> {
    let ns = policy.namespace().unwrap_or_else(|| "default".into());
    let name = policy.name_any();
    let api: Api<FlakeUpdatePolicy> = Api::namespaced(ctx.client.clone(), &ns);
    let cond = Condition {
        r#type: "Reconciled".into(),
        status: "False".into(),
        reason: Some("ReconcileError".into()),
        message: Some(msg),
        last_transition_time: Some(Utc::now()),
    };
    let patch = serde_json::json!({
        "status": {
            "conditions": [cond],
            "observedGeneration": policy.metadata.generation.unwrap_or(0),
        }
    });
    api.patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

// ─── FlakeUpdateProposal reconciler ─────────────────────────────────

pub async fn reconcile_proposal(
    proposal: Arc<FlakeUpdateProposal>,
    ctx: Arc<Context>,
) -> Result<Action, ReconcileError> {
    use ProposalPhase::*;
    let ns = proposal.namespace().unwrap_or_else(|| "default".into());
    let name = proposal.name_any();
    let phase = proposal
        .status
        .as_ref()
        .map(|s| s.phase)
        .unwrap_or(Pending);
    info!(ns = %ns, proposal = %name, phase = ?phase, "reconciling FlakeUpdateProposal");

    let proposals: Api<FlakeUpdateProposal> = Api::namespaced(ctx.client.clone(), &ns);

    match phase {
        Pending => {
            if proposal.spec.approved {
                set_phase(&proposals, &name, Verifying).await?;
                Ok(Action::requeue(Duration::from_secs(1)))
            } else {
                Ok(Action::requeue(REQUEUE_OK))
            }
        }
        Verifying => {
            let policy_api: Api<FlakeUpdatePolicy> =
                Api::namespaced(ctx.client.clone(), &ns);
            let policy = policy_api.get(&proposal.spec.policy_name).await?;
            let repo_dir = resolve_repo_dir(&ctx.tend_config, &proposal.spec.repo)?;
            // Differential gating: gates pass when the proposed pin
            // doesn't introduce new failures vs the current lockfile.
            // Pre-existing failures (broken modules, flaky tests) don't
            // block bumps that aren't responsible for them.
            let override_ctx = gates::GateContext::FlakeOverride {
                input: proposal.spec.input.clone(),
                flake_ref: gates::override_flake_ref(&proposal.spec.to),
            };
            let results = gates::run_all_differential(
                &policy.spec.gates,
                &repo_dir,
                &override_ctx,
            )
                .await
                .map_err(|e| ReconcileError::Other(anyhow::anyhow!("gates: {e}")))?;
            for r in &results {
                let outcome = if r.passed { "passed" } else { "failed" };
                metrics()
                    .gates_total
                    .with_label_values(&[&r.name, outcome])
                    .inc();
            }
            // Skipped gates (platform mismatch, no remote builder yet)
            // are non-blocking — same as Passed for proposal progress.
            // Only an actual Failed gate stops the proposal.
            let any_failed = results.iter().any(|r| !r.passed && !r.skipped);
            let next = if any_failed { Failed } else { Verified };
            let patch = serde_json::json!({
                "status": {
                    "phase": next,
                    "gateResults": results,
                    "observedGeneration": proposal.metadata.generation.unwrap_or(0),
                }
            });
            proposals
                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
            metrics()
                .proposals_total
                .with_label_values(&[phase_label(next)])
                .inc();
            Ok(Action::requeue(Duration::from_secs(1)))
        }
        Verified => {
            set_phase(&proposals, &name, Applying).await?;
            Ok(Action::requeue(Duration::from_secs(1)))
        }
        Applying => {
            let repo_dir = resolve_repo_dir(&ctx.tend_config, &proposal.spec.repo)?;
            // Working-tree mutex against the policy reconciler's
            // discovery-side fetch+reset on the same path. Without it,
            // both reconcilers race on `.git/index.lock` and apply
            // dies with "Another git process seems to be running".
            let lock_arc = ctx.repo_lock(&repo_dir).await;
            let _wt_guard = lock_arc.lock().await;
            let outcome = match apply::apply_pin(
                &repo_dir,
                &proposal.spec.input,
                &proposal.spec.to,
                ctx.github_token.as_deref(),
            )
            .await
            {
                Ok(o) => o,
                Err(e) => {
                    metrics().applies_total.with_label_values(&["failed"]).inc();
                    // {:#} renders the full error chain on a single
                    // line — outer context + every `.context()` wrap +
                    // the underlying source. Without `#`, anyhow's
                    // Display only emits the outermost message, hiding
                    // the actual failure (e.g. "git push: Permission
                    // denied" gets reduced to "commit + push").
                    let full_error = format!("{e:#}");
                    let patch = serde_json::json!({
                        "status": {
                            "phase": Failed,
                            "error": full_error,
                            "observedGeneration": proposal.metadata.generation.unwrap_or(0),
                        }
                    });
                    proposals
                        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                        .await?;
                    metrics().proposals_total.with_label_values(&["Failed"]).inc();
                    return Ok(Action::requeue(REQUEUE_OK));
                }
            };
            metrics().applies_total.with_label_values(&["landed"]).inc();
            let patch = serde_json::json!({
                "status": {
                    "phase": Applied,
                    "appliedCommit": outcome.commit,
                    "observedGeneration": proposal.metadata.generation.unwrap_or(0),
                }
            });
            proposals
                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
            metrics().proposals_total.with_label_values(&["Applied"]).inc();
            Ok(Action::await_change())
        }
        Applied | Failed | Stale => Ok(Action::await_change()),
    }
}

pub fn proposal_error_policy(
    _proposal: Arc<FlakeUpdateProposal>,
    err: &ReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    metrics().reconcile_errors_total.with_label_values(&["FlakeUpdateProposal"]).inc();
    warn!(error = %err, "FlakeUpdateProposal reconcile error; retrying");
    Action::requeue(REQUEUE_FAST)
}

async fn set_phase(
    api: &Api<FlakeUpdateProposal>,
    name: &str,
    phase: ProposalPhase,
) -> Result<(), ReconcileError> {
    let patch = serde_json::json!({ "status": { "phase": phase } });
    api.patch_status(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    metrics()
        .proposals_total
        .with_label_values(&[phase_label(phase)])
        .inc();
    Ok(())
}

fn phase_label(p: ProposalPhase) -> &'static str {
    match p {
        ProposalPhase::Pending => "Pending",
        ProposalPhase::Verifying => "Verifying",
        ProposalPhase::Verified => "Verified",
        ProposalPhase::Applying => "Applying",
        ProposalPhase::Applied => "Applied",
        ProposalPhase::Failed => "Failed",
        ProposalPhase::Stale => "Stale",
    }
}

fn ok_condition(reason: &str, msg: &str) -> Condition {
    Condition {
        r#type: "Reconciled".into(),
        status: "True".into(),
        reason: Some(reason.into()),
        message: Some(msg.into()),
        last_transition_time: Some(Utc::now()),
    }
}

/// Cheap, stable fingerprint for a flake.lock — used purely as a
/// "did this change since last reconcile?" signal in policy status.
/// Not a security primitive.
fn lock_fingerprint(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_name_is_deterministic_and_dns_safe() {
        let n = proposal_name("my-policy", "substrate", "abc123def456789");
        assert_eq!(n, "my-policy-substrate-abc123def456");
        assert!(n.chars().all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit()));
    }

    #[test]
    fn proposal_name_lowercases_and_sanitizes() {
        let n = proposal_name("My_Policy", "Sub.strate", "ABC");
        assert_eq!(n, "my-policy-sub-strate-abc");
    }
}
