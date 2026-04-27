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
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use super::apply;
use super::crds::{
    Condition, FlakeUpdatePolicy, FlakeUpdatePolicyStatus, FlakeUpdateProposal,
    FlakeUpdateProposalSpec, ProposalPhase, UpdateMode,
};
use super::discovery::{self, ReqwestHeadResolver};
use super::gates;
use super::metrics::metrics;
use super::workspace::resolve_repo_dir;
use crate::config::Config;
use crate::flake_lock::ExtendedLockFile;

const REQUEUE_OK: Duration = Duration::from_secs(300);
const REQUEUE_FAST: Duration = Duration::from_secs(30);

pub struct Context {
    pub client: Client,
    pub tend_config: Arc<Config>,
    pub http: reqwest::Client,
    pub github_token: Option<String>,
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

    let resolver = ReqwestHeadResolver {
        client: ctx.http.clone(),
        token: ctx.github_token.clone(),
    };
    let outcome = discovery::discover_advances(&lock, &resolver)
        .await
        .map_err(|e| ReconcileError::Other(anyhow::anyhow!("discovery: {e}")))?;

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

    let proposals: Api<FlakeUpdateProposal> = Api::namespaced(ctx.client.clone(), &ns);
    for adv in &advances {
        let mode = mode_for_input(&policy, &adv.input);
        if matches!(mode, UpdateMode::Locked | UpdateMode::Forbidden) {
            continue;
        }
        let auto_approve = matches!(mode, UpdateMode::Auto);
        upsert_proposal(&proposals, &policy, adv, auto_approve).await?;
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

    Ok(Action::requeue(requeue))
}

pub fn policy_error_policy(
    _policy: Arc<FlakeUpdatePolicy>,
    err: &ReconcileError,
    _ctx: Arc<Context>,
) -> Action {
    metrics().reconcile_errors_total.with_label_values(&["FlakeUpdatePolicy"]).inc();
    warn!(error = %err, "FlakeUpdatePolicy reconcile error; retrying");
    Action::requeue(REQUEUE_FAST)
}

fn mode_for_input(policy: &FlakeUpdatePolicy, input: &str) -> UpdateMode {
    policy
        .spec
        .inputs
        .get(input)
        .copied()
        .unwrap_or(policy.spec.default_mode)
}

async fn upsert_proposal(
    api: &Api<FlakeUpdateProposal>,
    policy: &FlakeUpdatePolicy,
    adv: &discovery::CandidateAdvance,
    auto_approve: bool,
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

    let proposal = FlakeUpdateProposal {
        metadata: ObjectMeta {
            name: Some(prop_name),
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
    api.create(&PostParams::default(), &proposal).await?;
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
