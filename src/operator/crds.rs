//! Custom resource definitions for the fleet update controller.
//!
//! Phase 1 covers `flake.lock`. Phases 2-4 add Helm/Cargo/Image CRDs
//! following the same shape. See `docs/OPERATOR-DESIGN.md`.

use chrono::{DateTime, Utc};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ─── Shared types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RepoRef {
    /// Workspace name from `~/.config/tend/config.yaml`.
    pub workspace: String,
    /// Path under the workspace root (e.g. `"blackmatter-shell"`).
    pub repo: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
pub enum UpdateMode {
    /// Hold at current revision; reject proposals.
    Locked,
    /// Auto-advance when upstream moves AND every gate is green.
    Auto,
    /// Generate proposal but require `spec.approved=true` before verifying.
    #[default]
    Gated,
    /// Treat upstream advancement as drift; never propose.
    Forbidden,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RolloutWindow {
    /// Cron-style window during which lock writes are permitted —
    /// e.g. `"Mon-Fri 09:00-17:00 America/New_York"`.
    pub cron: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Condition {
    pub r#type: String,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub last_transition_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GateResult {
    pub name: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub log_excerpt: Option<String>,
}

// ─── Phase 1: flake.lock CRDs ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FlakeRev {
    /// e.g. `"github:pleme-io/substrate"`
    pub url: String,
    pub rev: String,
    pub nar_hash: String,
    pub last_modified: i64,
}

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "fleet.pleme.io",
    version = "v1alpha1",
    kind = "FlakeUpdatePolicy",
    plural = "flakeupdatepolicies",
    shortname = "fup",
    status = "FlakeUpdatePolicyStatus",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct FlakeUpdatePolicySpec {
    pub repo: RepoRef,

    #[serde(default)]
    pub inputs: BTreeMap<String, UpdateMode>,

    #[serde(default)]
    pub default_mode: UpdateMode,

    /// Gate names — each resolves to a Rust dispatcher fn at reconcile
    /// time. Unknown gates surface as a status condition rather than
    /// failing API admission.
    #[serde(default)]
    pub gates: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<RolloutWindow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlakeUpdatePolicyStatus {
    /// Inputs the controller observed in the repo's flake.lock at last
    /// reconcile — surfaces what's actually being tracked.
    #[serde(default)]
    pub tracked_inputs: Vec<String>,

    pub last_observed_lock_hash: Option<String>,

    #[serde(default)]
    pub conditions: Vec<Condition>,

    #[serde(default)]
    pub observed_generation: i64,
}

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "fleet.pleme.io",
    version = "v1alpha1",
    kind = "FlakeUpdateProposal",
    plural = "flakeupdateproposals",
    shortname = "fpr",
    status = "FlakeUpdateProposalStatus",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct FlakeUpdateProposalSpec {
    pub repo: RepoRef,
    pub input: String,
    pub from: FlakeRev,
    pub to: FlakeRev,
    pub discovered_at: DateTime<Utc>,

    /// Owner ref back to the policy that produced this proposal — lets
    /// the controller GC orphan proposals when the policy changes.
    pub policy_namespace: String,
    pub policy_name: String,

    /// Required for `Gated` mode; ignored for `Auto`.
    #[serde(default)]
    pub approved: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
pub enum ProposalPhase {
    #[default]
    Pending,
    Verifying,
    Verified,
    Applying,
    Applied,
    Failed,
    /// Upstream moved past `to` rev; supersede + GC.
    Stale,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlakeUpdateProposalStatus {
    #[serde(default)]
    pub phase: ProposalPhase,

    #[serde(default)]
    pub gate_results: Vec<GateResult>,

    /// Frontier index in the in-flight DAG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dag_wave: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_commit: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    #[serde(default)]
    pub conditions: Vec<Condition>,

    #[serde(default)]
    pub observed_generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RolloutWaveProposalRef {
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RolloutWave {
    pub index: u32,
    pub proposals: Vec<RolloutWaveProposalRef>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
pub enum RolloutTrigger {
    /// Upstream advance discovered by a watcher.
    #[default]
    UpstreamAdvance,
    /// Manual replay of a prior failed rollout.
    ManualReplay,
    /// Triggered by a policy change (gates added/removed, mode flipped).
    PolicyChange,
}

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "fleet.pleme.io",
    version = "v1alpha1",
    kind = "FlakeUpdateRollout",
    plural = "flakeupdaterollouts",
    shortname = "fro",
    status = "FlakeUpdateRolloutStatus",
    namespaced
)]
#[serde(rename_all = "camelCase")]
pub struct FlakeUpdateRolloutSpec {
    /// Topologically ordered waves. Each wave's proposals run in
    /// parallel; promotion to wave N+1 requires all of wave N green.
    pub waves: Vec<RolloutWave>,

    pub trigger: RolloutTrigger,

    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlakeUpdateRolloutStatus {
    /// Current wave being verified (1-indexed; 0 = not yet started).
    #[serde(default)]
    pub current_wave: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default)]
    pub conditions: Vec<Condition>,

    #[serde(default)]
    pub observed_generation: i64,
}
