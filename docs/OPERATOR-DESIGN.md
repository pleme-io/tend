# tend operator — Fleet Update Controller

> **Status:** design — no code yet. Captures architectural decisions
> from the 2026-04-26 design session so they survive the conversation
> and inform implementation.

## Motivating problem

Every pleme-io repo pins upstream software at fixed revisions across
multiple lock surfaces:

| Surface | Where | Pin shape |
|---|---|---|
| Flake inputs | `flake.lock` | `(url, rev, narHash)` |
| Cargo deps | `Cargo.lock` | `(name, version, source, checksum)` |
| Helm charts | `HelmRelease.spec.chart.spec.version` | semver constraint string |
| Container images | `HelmRelease.spec.values.image.tag` | tag (`latest` / `amd64-SHA` / semver) |
| Pangea gems | `Gemfile.lock` | gem version |

Today, advancing any of those is operator-driven and unsynchronized.
Failures cascade across repos before anyone notices. The substrate
regression of 2026-04-26 is the canonical example: bumping
`substrate@71281c6 → 3483839` in `nix/flake.lock` introduced a
`module-trio.nix:321: attribute 'enable' missing` error that broke
`darwinConfigurations.cid.system` evaluation. The bump landed in
`nix/flake.lock` and was rolled back manually only after a build
failed on rio. Every other consumer of substrate (~30 repos) was at
risk of inheriting the same broken commit on their next flake update.

The fleet needs **declarative control over how each pinned-version
surface is allowed to evolve**, with verification gates running before
any lock-file write commits the fleet to a new revision.

## Solution shape

A Rust K8s controller built into `tend` itself as a `tend operator`
subcommand. Lives on rio as a HelmRelease alongside pangea-operator.
Owns per-domain CRDs that declare desirability policy + verification
gates per-repo, derives the dependency DAG from existing lock files,
walks the DAG topologically with verification at each frontier, and
writes back to lock files only after the entire frontier is verified.

### Why extend tend, not spin a sibling

tend already owns:

- workspace introspection (`~/.config/tend/config.yaml`, GitHub API repo discovery)
- daemon shape (`tend daemon`, 300s cycle, parallel per-workspace `tokio::JoinSet`)
- flake.lock parsing (`src/flake_lock.rs`, 192 LOC)
- flake input handling (`src/flake.rs`, 1156 LOC)
- planner machinery (`src/planner.rs`)
- watch cache (`src/watch.rs`, `src/watch_cache.rs`, `src/head_cache.rs`)
- release swarm (`src/release_swarm.rs`) — already tracks upstream advances

A sibling operator would re-implement ~30% of that. Pillar 12
(generation over composition) says extend the typed Rust tool that
already has the workspace knowledge.

## Topology source of truth: derived

The DAG falls out of existing lock files. No `FleetTopology` CRD.
Adapters parse each format and emit edges:

- `flake.lock` — `nodes[*].inputs[*].follows` graph
- `Cargo.lock` — `package[].dependencies[]`
- `HelmRelease` YAML — `chart.spec.sourceRef` + bundled image tags
- container image tags — registry index + chart references

Cross-domain edges (e.g. blackmatter-pleme skills depend on
tatara-lisp's CRD shape but no flake input expresses this) are NOT
captured initially. If real-world data shows hidden edges matter,
add `FleetTopologyOverride` CR later — don't pre-build it.

## Vertical slice sequencing

| Phase | Domain | Why this order |
|---|---|---|
| 1 | `flake.lock` | Densest case, every pleme-io repo has one, JSON-parseable, follows-graph explicit. Catches the substrate regression. |
| 2 | `HelmRelease` chart versions | Direct gitops application; catches "chart bump broke FluxCD reconcile" before plo. |
| 3 | `Cargo.lock` | Most complex (semver, features, transitive deps); do after architecture is proven. |
| 4 | Container image tags | Least standardized (`latest` / `amd64-SHA` / semver coexist); needs per-registry logic. |

Each phase ships a complete vertical slice (CRD + parser + watcher +
verification dispatch + apply path) on rio before the next starts.

## CRDs (Phase 1: flake)

All CRDs use `#[derive(TataraDomain)]` so the Lisp surface comes for
free (`(deffleetflakepolicy …)`, etc.). Six lines of ceremony per CRD
per the `tatara/docs/rust-lisp.md` cookbook.

### FlakeUpdatePolicy

```rust
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema, TataraDomain)]
#[kube(group = "fleet.pleme.io", version = "v1alpha1", kind = "FlakeUpdatePolicy",
       status = "FlakeUpdatePolicyStatus", shortname = "fup", namespaced)]
#[tatara(keyword = "deffleetflakepolicy")]
pub struct FlakeUpdatePolicySpec {
    pub repo: RepoRef,

    /// Per-input desirability. Keys match flake.lock node names.
    #[serde(default)]
    pub inputs: BTreeMap<String, UpdateMode>,

    /// Default for inputs not explicitly listed.
    #[serde(default)]
    pub default_mode: UpdateMode,

    /// Gate names — each resolves to a Rust dispatcher fn at reconcile
    /// time. Unknown gates surface as a status condition rather than
    /// failing API admission. Examples:
    ///   "nix-build:darwinConfigurations.cid.system"
    ///   "nix-flake-check"
    ///   "cargo-test"
    ///   "forge-ci"
    ///   "iac-test-runner"
    #[serde(default)]
    pub gates: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<RolloutWindow>,
}
```

### FlakeUpdateProposal

```rust
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema, TataraDomain)]
#[kube(group = "fleet.pleme.io", version = "v1alpha1", kind = "FlakeUpdateProposal",
       status = "FlakeUpdateProposalStatus", shortname = "fpr", namespaced)]
#[tatara(keyword = "deffleetflakeproposal")]
pub struct FlakeUpdateProposalSpec {
    pub repo: RepoRef,
    pub input: String,         // flake input name, e.g. "substrate"
    pub from: FlakeRev,
    pub to: FlakeRev,
    pub discovered_at: DateTime<Utc>,

    /// Owner ref back to the policy that produced this proposal —
    /// lets the controller GC orphan proposals when the policy changes.
    pub policy: ObjectReference,

    /// Required for `Gated` mode; ignored for `Auto`. Approval timestamp
    /// goes in status. (If audit-of-who-approved-when ever matters, add
    /// `status.approved_by` + `status.approved_at` populated by an
    /// admission webhook on spec change. Don't pre-build.)
    #[serde(default)]
    pub approved: bool,
}
```

### FlakeUpdateRollout

In-flight DAG visibility. Created when a frontier wave starts; deleted
when the wave fully applies or fails.

```rust
#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema, TataraDomain)]
#[kube(group = "fleet.pleme.io", version = "v1alpha1", kind = "FlakeUpdateRollout",
       status = "FlakeUpdateRolloutStatus", shortname = "fro", namespaced)]
#[tatara(keyword = "deffleetflakerollout")]
pub struct FlakeUpdateRolloutSpec {
    /// Snapshot of the DAG when the rollout started. Each entry is a
    /// (repo, input) tuple plus the `to` rev being targeted; ordered
    /// by topological wave.
    pub waves: Vec<RolloutWave>,
    pub trigger: RolloutTrigger,    // upstream advance, manual replay, etc.
    pub started_at: DateTime<Utc>,
}

pub struct RolloutWave {
    pub index: u32,
    pub proposals: Vec<ObjectReference>,  // FlakeUpdateProposal refs
}
```

### Supporting types

```rust
pub struct RepoRef {
    pub workspace: String,    // matches ~/.config/tend/config.yaml
    pub repo: String,         // path under workspace root
}

pub enum UpdateMode {
    Locked,        // hold; reject proposals
    Auto,          // advance when upstream moves AND every gate green
    Gated,         // generate proposal but require spec.approved=true
    Forbidden,     // never propose; treat upstream advancement as drift
}

pub struct RolloutWindow {
    pub cron: String,    // "Mon-Fri 09:00-17:00 America/New_York"
}

pub struct FlakeRev {
    pub url: String,             // "github:pleme-io/substrate"
    pub rev: String,
    pub nar_hash: String,
    pub last_modified: i64,
}

pub enum ProposalPhase {
    Pending,            // waiting for approval (Gated) or DAG slot (Auto)
    Verifying,          // gates running
    Verified,           // ready to apply
    Applying,           // commit + push in flight
    Applied,            // landed on main, FluxCD picked up
    Failed,             // gate failed; held back
    Stale,              // upstream moved past `to` rev; supersede + GC
}
```

## LockFormat trait

```rust
/// Implemented once per lock format. The DAG planner consumes only
/// this trait — no domain-specific logic leaks into the orchestration
/// layer.
pub trait LockFormat: Send + Sync {
    type Pin: Clone + Serialize + DeserializeOwned + Send + Sync;

    fn lock_path(&self) -> &'static Path;
    fn parse(&self, contents: &str) -> Result<BTreeMap<String, Self::Pin>>;
    fn edges(&self, parsed: &BTreeMap<String, Self::Pin>) -> Vec<(String, String)>;
    fn write_pin(&self, contents: &str, target: &str, new: &Self::Pin) -> Result<String>;
}
```

Phase 1 implements `FlakeLockAdapter`. Phase 2-4 add Helm/Cargo/Image
adapters. The DAG planner is generic over `T: LockFormat`, so the
orchestration code is written once.

## Reuse inventory

What the operator does NOT need to reinvent:

| Need | Reused from |
|---|---|
| Workspace introspection | tend `src/config.rs`, `src/sync.rs` |
| Repo discovery via GitHub API | tend `src/github.rs` |
| Daemon lifecycle | tend `src/daemon.rs`, `tsunagu` library |
| Flake.lock parsing | tend `src/flake_lock.rs` (extend) |
| Flake input handling | tend `src/flake.rs` |
| Watch cache | tend `src/watch_cache.rs`, `src/head_cache.rs` |
| Upstream release tracking | tend `src/release_swarm.rs` (extend) |
| K8s reconciler scaffold | `kube-rs` pattern from pangea-operator |
| CRD authoring | `#[derive(TataraDomain)]` from tatara |
| Config (typed YAML, hot-reload) | `shikumi` |
| HTTP client (registry polling) | `todoku` |
| Auth (GitHub/crates.io/GHCR tokens) | `kenshou` |
| Fast-match (event → policy) | `hayai` |
| MCP tool surface | `kaname` (auto-generated for free) |
| DB persistence | SeaORM + `shinka` migrations |
| Sandboxed nix builds | `pangea-jit-builders` (spot Linux capacity) |
| Cluster-level verification | `iac-test-runner`, `kenshi` ephemeral clusters |
| CI gate dispatch | `forge ci` |
| Notifications | `tsuuchi` → ntfy (already wired in `alertmanager-ntfy`) |
| Apply path (commit + push) | `gh` CLI / GitHub MCP / tatara-script kubectl-apply |
| FluxCD reconcile of merged result | existing FluxCD on rio |
| Attestation chain | `tameshi` BLAKE3 + `sekiban` admission webhook |
| Dashboards | `PangeaDashboard` CRs + `pangea-grafana` Ruby DSL |
| Helm chart packaging | `helmworks` → `pleme-charts` OCI registry |
| Repo scaffolding (if ever split) | `repo-forge` archetypes |

Genuinely new code, everything else reused:

| Module | LOC est. |
|---|---|
| CRD struct definitions (3 CRDs × 3 phases = 9 structs total) | ~200 |
| LockFormat trait + 4 adapters (flake / helm / cargo / image) | ~600 |
| Upstream watchers (GitHub releases / crates.io / OCI / image registry) | ~400 |
| DAG planner (petgraph + topo sort + frontier walking) | ~300 |
| Verification dispatcher (resolves gate names → existing tools) | ~200 |
| Apply orchestrator (commit, push, FluxCD reconcile poll) | ~200 |
| shikumi config schema | ~50 |
| **Total new code across all 4 phases** | **~2,000 LOC** |

Phase 1 alone (flake.lock vertical slice) is ~800 LOC.

## Verification gate catalog

Gate name strings resolve at reconcile time to dispatchers. Phase-1
gate set:

| Gate name | Dispatcher | Use case |
|---|---|---|
| `nix-flake-check` | `nix flake check` in repo dir | Generic flake sanity |
| `nix-build:<attr>` | `nix build .#<attr> --no-link` | Specific output (e.g. `darwinConfigurations.cid.system`) |
| `nix-eval:<attr>` | `nix eval .#<attr> --apply '_: null'` | Evaluation-only check, faster than build |
| `forge-ci` | `forge ci` in repo dir | Substrate's full CI gate (build + test + cache push) |
| `cargo-test` | `cargo test --release` | Rust workspace tests |
| `cargo-build` | `cargo build --release` | Compile check only |

Phase 2+ adds:

| Gate | Dispatcher |
|---|---|
| `kustomize-build:<path>` | `kustomize build --load-restrictor LoadRestrictionsNone <path>` |
| `helm-template:<release>` | `helm template <release> --validate` |
| `iac-test-runner:<arch>` | `iac-test-runner` ephemeral cluster |
| `kenshi:<spec>` | kenshi ephemeral test cycle |

Each dispatcher returns `GateResult { name, passed, duration, log_excerpt }`.

## Reconcile loop (pseudo-code)

```rust
async fn reconcile_flake_policy(policy: Arc<FlakeUpdatePolicy>, ctx: Arc<Context>) -> Result<Action> {
    // 1. Read repo's current lock state via tend's existing flake_lock module.
    let lock = ctx.tend.read_lock(&policy.spec.repo, &FlakeLockAdapter)?;

    // 2. Discovery: ask each input's upstream watcher if there's a newer rev.
    //    Reuses release_swarm + head_cache machinery.
    let advances = ctx.watchers.discover_advances(&lock, &policy.spec).await?;

    // 3. Materialize as FlakeUpdateProposal CRs (idempotent — same input+to_rev = no-op).
    for adv in advances {
        ctx.api.upsert_proposal(&FlakeUpdateProposal {
            spec: FlakeUpdateProposalSpec {
                repo: policy.spec.repo.clone(),
                input: adv.input.clone(),
                from: adv.from.clone(),
                to: adv.to.clone(),
                discovered_at: Utc::now(),
                policy: policy.object_ref(),
                approved: matches!(policy.spec.input_mode(&adv.input), UpdateMode::Auto),
            },
            ..Default::default()
        }).await?;
    }

    // 4. DAG: union edges across this repo's policy + every other repo
    //    that follows the same upstream input. Walk topologically,
    //    schedule verification at each frontier.
    let dag = ctx.dag.build_or_extend(&policy, &lock).await?;
    ctx.dag.advance_frontier(&dag).await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn reconcile_proposal(proposal: Arc<FlakeUpdateProposal>, ctx: Arc<Context>) -> Result<Action> {
    use ProposalPhase::*;
    match proposal.status.phase {
        Pending if proposal.spec.approved => transition(proposal, Verifying, ctx).await,
        Pending => Ok(Action::requeue(Duration::from_secs(60))),
        Verifying => run_gates_and_transition(proposal, ctx).await,
        Verified => transition(proposal, Applying, ctx).await,
        Applying => apply_and_transition(proposal, ctx).await,
        Applied | Failed | Stale => Ok(Action::await_change()),
    }
}
```

## Deployment

### Helm chart

`helmworks` repo → `charts/pleme-tend-operator/` umbrella over
`pleme-microservice` library chart. Published to
`oci://ghcr.io/pleme-io/charts/pleme-tend-operator` via existing chart
release flow. Values surface:

```yaml
tendOperator:
  image:
    repository: ghcr.io/pleme-io/tend
    tag: ""             # defaults to chart appVersion
  config:
    workspaceConfigPath: /etc/tend/config.yaml
    pollIntervalSeconds: 300
    maxParallelGates: 8
  rio:
    builderEndpoint: ""  # pangea-jit-builders SSH URL for sandboxed nix builds
  notifications:
    ntfyTopic: rio-fleet-updates
```

### GitOps

`k8s/clusters/rio/infrastructure/tend-operator/` — kustomize overlay
referencing the published Helm chart, single-replica, persistent
storage for DAG state, ServiceMonitor for vmagent scraping. Wired
into `clusters/rio/infrastructure/kustomization.yaml` after
`pangea-operator` (depends on the same postgres).

### Workspace config

`~/.config/tend/config.yaml` (already exists for tend's CLI use) is
mounted into the operator pod as a ConfigMap so the operator inherits
the same workspace truth as the operator's local CLI invocations.

## Open follow-ups

- **`tend` workspace conversion.** Current tend is a single-crate
  binary. Splitting into `tend-core` (workspace logic) + `tend-cli`
  (binary) + `tend-operator` (controller) + `tend-lock-formats`
  (parser crate) is a follow-up. Phase 1 can land as new modules in
  the existing single-crate binary if speed matters more than
  separation; refactor before Phase 2 starts.

- **Cross-domain DAG bridges.** A bump to a Helm chart version that
  bundles a substrate-built image needs to know "this image was built
  from substrate@X" so a substrate bump triggers re-verification of
  every dependent HelmRelease. Phase 4 (image tags) is the natural
  home for this — image tags can encode the substrate rev they were
  built from, bridging flake↔image domains.

- **Approval-via-MCP.** A MCP tool `fleet_approve_proposal {ref}`
  surfaces gated proposals to operators in their MCP-aware editor.
  Generated by `kaname` for free.

- **Dashboards.** A `PangeaDashboard` CR per fleet view: "DAG state",
  "proposal queue depth", "gate latency", "stalled rollouts".

- **Attestation gating.** Once `tameshi` chain entries are written
  per applied proposal, `sekiban` admission webhook can refuse to
  admit any HelmRelease whose chart version isn't in the attested
  chain. Closes the loop: only operator-blessed pins can land in
  cluster.

## Decisions captured

| Decision | Choice | Reason |
|---|---|---|
| Per-domain CRDs vs unified | Per-domain with shared `LockFormat` trait | Per-domain keeps schemas clean; trait keeps orchestration generic |
| Extend tend vs sibling repo | Extend | tend already owns workspace knowledge, daemon shape, flake parsing |
| Topology source | Derived from lock files | Already correct + available; no override surface until proven needed |
| Phase order | flake → helm → cargo → image | flake densest + simplest + matches motivating problem |
| Gate identifier shape | `Vec<String>` | Extensible without CRD version bumps; unknown gates surface as status |
| Approval surface | `spec.approved: bool` | Simple; no separate Approval CR until audit needs it |
| Rollout CR | Yes | Operator visibility into in-flight waves earns the byte cost |
| Cross-domain edge override | Not yet | Real-world data should drive whether `FleetTopologyOverride` exists |

## Why this design pays off

**Substrate regression replay:** with the operator running, the
2026-04-26 substrate bump would have followed this path:

1. `release_swarm` watcher detects new substrate commit `3483839`.
2. `FlakeUpdateProposal` CRs created for every repo following substrate.
3. Each proposal enters `Verifying` phase; gate `nix-build:darwinConfigurations.cid.system` runs in a JIT builder sandbox.
4. Build fails (`module-trio.nix:321: attribute 'enable' missing`).
5. Proposal transitions to `Failed`; tsuuchi posts to ntfy.
6. **No flake.lock anywhere is mutated.** Fleet stays on `71281c6`.
7. Operator can manually inspect the failed gate via
   `kubectl describe flakeupdateproposal substrate-3483839-nix`.
8. Fix the substrate regression upstream; next watcher cycle picks
   up the corrective commit; cycle repeats with green gates; bump lands.

That's the convergence loop, applied to the typed software graph of
the entire fleet, running in perpetuity on rio.
