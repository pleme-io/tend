//! Prebuild: walk workspace repos that carry a flake.nix, build their
//! canonical output (`packages.${system}.default`), and optionally push
//! the resulting closure into an Attic binary cache.
//!
//! Sibling of [`flake_update_daemon`](crate::main::run_flake_update_daemon):
//! `flake-update` propagates lock bumps across the workspace's dep graph;
//! `prebuild` does the *next* step, which is actually realising the
//! resulting outputs into `/nix/store` so consumers (CI nodes, other
//! workstations) get cache hits instead of cold builds.
//!
//! Design — three guarantees:
//! 1. **Idempotent across cycles.** A persistent seen-rev cache at
//!    `${XDG_CACHE_HOME:-~/.cache}/tend/prebuild-seen.json` records
//!    `{ repo_path: last_built_git_rev }`. Repos at the same rev as
//!    last cycle are skipped.
//! 2. **Per-closure attic push (no SQLite-var-limit landmine).** Each
//!    successful `nix build` produces a small set of out-paths; we
//!    push those directly. We never try to push the entire store in
//!    one `get-missing-paths` request.
//! 3. **Resource discipline lives outside this code.** The daemon is
//!    a single-threaded loop (`nix build` does its own parallelism
//!    internally). Operators throttle via systemd `CPUQuota=`,
//!    `MemoryHigh=`, `IOWeight=`, etc. — see
//!    `blackmatter-tend/module/default.nix`.

use crate::audit::AuditLog;
use crate::config::Config;
use crate::prebuild_cache::{self, CacheTarget, ClosureDedup, PackageSelector, ReproPolicy};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use shigoto_budget::{BudgetSpec, BudgetTree};
use shigoto_dag::Dag;
use shigoto_emit::{AuditFileEmitter, InMemorySink, TransitionEmitter};
use shigoto_scheduler::{InProcessScheduler, Scheduler};
use shigoto_types::{JobId, JobKindId, JobPhase, OutputSink};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// One repo's outcome within a single prebuild cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// HEAD matches the seen-rev cache; nothing to do.
    NoChange,
    /// `nix build .` failed with "does not provide attribute" — repo
    /// has no `packages.${system}.default`. Skipped without error.
    NoDefault,
    /// Build succeeded; out-paths are the closures that now live in
    /// `/nix/store` and (optionally) were pushed to attic.
    Built {
        out_paths: Vec<String>,
        pushed: usize,
    },
}

/// Rollup across all repos in a cycle.
#[derive(Debug, Default, Clone, Serialize)]
pub struct PrebuildSummary {
    pub built: usize,
    pub no_change: usize,
    pub skipped_no_default: usize,
    pub failed: usize,
    pub pushed: usize,
}

impl PrebuildSummary {
    /// "Work performed this cycle" — drives daemon exponential-backoff
    /// reset, mirroring [`flake::ExecSummary::work`].
    #[must_use]
    pub fn work(&self) -> usize {
        self.built
    }

    fn absorb(&mut self, outcome: &BuildOutcome) {
        match outcome {
            BuildOutcome::NoChange => self.no_change += 1,
            BuildOutcome::NoDefault => self.skipped_no_default += 1,
            BuildOutcome::Built { pushed, .. } => {
                self.built += 1;
                self.pushed += *pushed;
            }
        }
    }
}

/// Operator-tunable knobs threaded into every per-repo build attempt.
#[derive(Debug, Clone)]
pub struct PrebuildOptions {
    /// Suppress per-repo log lines (still emits one-line audit events).
    pub quiet: bool,
    /// If `Some`, push every produced closure to this attic cache via
    /// `attic push <cache_name> <out-path>` (per closure, so each
    /// request stays well below atticd's SQLite var limit).
    pub attic: Option<AtticPush>,
    /// Maximum concurrent per-repo `nix build` invocations. Each
    /// in-flight build still parallelises internally per `max-jobs`,
    /// so set this conservatively — `1` for shared developer
    /// workstations, `2`–`4` on a dedicated builder like rio. The
    /// hard ceiling on real resource use lives in systemd's
    /// `CPUQuota`/`MemoryHigh`/`IOWeight` on the unit, not here.
    pub max_inflight: usize,

    // ── Cache-fill extensions ───────────────────────────────────────
    /// Push every produced closure to each of these caches (fan-out).
    /// Takes precedence over the legacy single `attic` above; when
    /// empty, `attic` (if set) is promoted to a one-element list in
    /// [`run_cycle`] so existing CLI/config keeps working.
    pub caches: Vec<CacheTarget>,
    /// Which flake outputs to build. `Default` = legacy `nix build .`
    /// fast-path (no `flake show`); `All`/`Named` enumerate via
    /// `nix flake show --json`.
    pub selector: PackageSelector,
    /// Target systems for `All`/`Named`. Empty ⇒ this host's system.
    pub systems: Vec<String>,
    /// Reproducibility gate before pushing (anti-poison).
    pub repro: ReproPolicy,
}

impl Default for PrebuildOptions {
    fn default() -> Self {
        Self {
            quiet: false,
            attic: None,
            max_inflight: 1,
            caches: Vec::new(),
            selector: PackageSelector::Default,
            systems: Vec::new(),
            repro: ReproPolicy::Trusting,
        }
    }
}

impl PrebuildOptions {
    /// The effective fan-out cache list: the explicit `caches` when
    /// non-empty, otherwise the legacy single `attic` quartet promoted
    /// to a one-element list. Filters to *usable* targets so a
    /// half-specified cache never reaches `attic login`.
    #[must_use]
    pub fn effective_caches(&self) -> Vec<CacheTarget> {
        let mut out: Vec<CacheTarget> = if self.caches.is_empty() {
            self.attic
                .as_ref()
                .map(|a| {
                    vec![CacheTarget {
                        // The legacy single-cache `--attic-*` quartet can
                        // only ever describe an attic destination.
                        backend: prebuild_cache::CacheBackend::Attic,
                        cache_name: a.cache_name.clone(),
                        server_name: a.server_name.clone(),
                        server_url: a.server_url.clone(),
                        token_file: a.token_file.display().to_string(),
                        enabled: true,
                    }]
                })
                .unwrap_or_default()
        } else {
            self.caches.clone()
        };
        out.retain(CacheTarget::is_usable);
        out
    }

    /// Overlay the cache-fill knobs (`packages`/`systems`/`repro`/
    /// `caches`) from the config's `prebuild:` block onto this
    /// CLI-derived options value. Reads the FIRST workspace that
    /// declares a `prebuild:` block — the common single-org case. The
    /// daemon reloads config each cycle and re-applies this, so editing
    /// `config.yaml` reshapes the fill within one interval, no restart.
    /// Legacy CLI fields (`quiet`/`max_inflight`/`attic`) are preserved;
    /// per-workspace legacy overrides still flow through
    /// [`effective_per_workspace`].
    #[must_use]
    pub fn with_fill_from_config(mut self, cfg: &Config) -> Self {
        let Some(pc) = cfg.workspaces.iter().find_map(|w| w.prebuild.as_ref()) else {
            return self;
        };
        self.selector = PackageSelector::parse(&pc.packages);
        self.systems = pc.systems.clone();
        self.repro = ReproPolicy::parse(&pc.repro);
        if !pc.caches.is_empty() {
            self.caches = pc.caches.iter().map(|c| c.to_target()).collect();
        }
        self
    }
}

/// Attic-reachability awareness for the daemon loop.
///
/// When the daemon's push target (the Attic server named by
/// `--attic-url`) is unreachable, building locally is pure waste: we
/// produce closures we can't ship to the cache the rest of the fleet
/// reads from. Rather than burn CPU/IO on builds that strand in the
/// local store, the daemon probes reachability before each cycle and,
/// when the server is down, enters a *separate* exponential backoff —
/// distinct from the converged-backoff that governs steady-state idle
/// polling — sleeping and re-probing without building.
///
/// All knobs carry best-default values via [`Default`]; an operator who
/// sets nothing gets a 60s→1800s doubling probe with a 5s timeout.
#[derive(Debug, Clone)]
pub struct ReachabilityOptions {
    /// When false, the daemon never probes and always builds (legacy
    /// behavior). Defaults to true *only when an attic URL is present*
    /// — the wiring in `main.rs` enables it iff `--attic-url` is given.
    pub enabled: bool,
    /// The URL probed each cycle (the attic server root, e.g.
    /// `http://rio:8080/`). Reused from `--attic-url` so there's one
    /// source of truth for "where attic lives".
    pub url: String,
    /// Floor of the unreachable-backoff (seconds). First unreachable
    /// cycle sleeps this long.
    pub min_interval: u64,
    /// Ceiling of the unreachable-backoff (seconds). The doubling
    /// caps here so a long outage settles into a steady re-probe pace.
    pub max_interval: u64,
    /// Per-probe HTTP timeout (seconds). A short timeout keeps a dead
    /// server from stalling the loop.
    pub probe_timeout: u64,
}

impl Default for ReachabilityOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            min_interval: 60,
            max_interval: 1800,
            probe_timeout: 5,
        }
    }
}

impl ReachabilityOptions {
    /// Compute the next unreachable-backoff sleep given how many cycles
    /// in a row the server has been unreachable. Pure function: exposed
    /// for unit testing the doubling/cap behavior without a network.
    ///
    /// `consecutive_unreachable` is 1-based: the *first* unreachable
    /// observation passes `1` and yields `min_interval`; `2` yields
    /// `2*min`; capped at `max_interval`. A reachable observation never
    /// calls this — the caller resets to the normal converged loop.
    #[must_use]
    pub fn unreachable_sleep(&self, consecutive_unreachable: u32) -> u64 {
        let min = self.min_interval.max(1);
        let max = self.max_interval.max(min);
        // First strike (n=1) → min; each subsequent strike doubles.
        // Double via saturating_mul so the value (not just the shift
        // amount) can't overflow on a long outage — `checked_shl`
        // guards the shift count but silently *wraps* the value, which
        // would zero out a large product. Cap at `max` each step so we
        // bail early instead of churning through u32::MAX iterations.
        let mut scaled = min;
        for _ in 1..consecutive_unreachable {
            scaled = scaled.saturating_mul(2);
            if scaled >= max {
                return max;
            }
        }
        scaled.min(max)
    }
}

/// Probe whether the Attic server at `url` is reachable. A cheap HTTP
/// GET with a short timeout — any response (even a 4xx/5xx) proves the
/// server is *up* and answering, which is all we need to decide it's
/// worth building+pushing. Only a connect/timeout/DNS error counts as
/// unreachable. Uses the non-optional `reqwest` dep (rustls-tls) so no
/// new dependency enters the lockfile.
pub async fn probe_attic_reachable(url: &str, timeout_secs: u64) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    // A bare GET to the server root. atticd answers with a redirect or
    // status page; either way a transport-level success means "up".
    client.get(url).send().await.is_ok()
}

#[derive(Debug, Clone)]
pub struct AtticPush {
    /// Cache name as known by atticd (e.g. `nexus`).
    pub cache_name: String,
    /// Server alias (passed to `attic login`).
    pub server_name: String,
    /// Attic server URL (e.g. `http://rio:8080/`).
    pub server_url: String,
    /// Path to a file containing a JWT signed by atticd. Read once at
    /// the start of each cycle; not held in memory between cycles.
    pub token_file: PathBuf,
}

/// Persistent seen-rev cache so unchanged repos are skipped across
/// daemon cycles AND across process restarts.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SeenCache {
    /// `{ absolute_repo_path: last_successfully_built_git_rev }`.
    /// BTreeMap so the on-disk JSON has stable key order for diffs.
    pub revs: BTreeMap<String, String>,
}

impl SeenCache {
    pub(crate) fn default_path() -> PathBuf {
        crate::cache::tend_cache_root().join("prebuild-seen.json")
    }

    pub(crate) fn load_from(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let s = serde_json::to_string_pretty(self).context("serializing prebuild-seen.json")?;
        std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))
    }
}

/// Public one-shot cycle: walks every workspace, builds every flake
/// repo whose HEAD has moved since last cycle, optionally pushes each
/// resulting closure to attic. Returns a rollup [`PrebuildSummary`].
///
/// Per-repo work is dispatched as one
/// [`PrebuildRepoJob`][crate::jobs::prebuild_repo] each, registered
/// against a `shigoto::InProcessScheduler` — the canonical tend
/// orchestrator, identical in shape to `reconcile`'s pull/sync path.
/// Concurrency is bounded by a per-kind `BudgetSpec::max_concurrent`
/// of `opts.max_inflight` (the typed replacement for the legacy
/// `Semaphore`). Each Job runs `prebuild_one` inside
/// [`tokio::task::spawn_blocking`] so the (synchronous) Command calls
/// don't block the runtime's reactor.
///
/// The `cfg` argument is the materialised tend config (same shape
/// `flake-update` consumes). `ws_filter` matches `Workspace::name`
/// 1:1; pass `None` to iterate everything.
///
/// The function is fully reusable from the K8s `operator` feature —
/// no CLI-specific state lives here, just the typed
/// `Config`/`PrebuildOptions`/`AuditLog` triple.
pub async fn run_cycle(
    cfg: &Config,
    ws_filter: Option<&str>,
    opts: &PrebuildOptions,
    audit: &AuditLog,
) -> Result<PrebuildSummary> {
    // Overlay cache-fill knobs (packages/systems/repro/caches) from the
    // config's `prebuild:` block. Owned shadow so the daemon's per-cycle
    // config reload reshapes the fill without a restart.
    let opts = opts.clone().with_fill_from_config(cfg);
    let seen_path = SeenCache::default_path();
    let seen = Arc::new(Mutex::new(SeenCache::load_from(&seen_path)));

    // Resolve the fan-out cache list (multi-cache `caches`, else the
    // legacy single `attic` quartet) and `attic login` each ONCE per
    // cycle. A cache whose login fails is dropped from the list — the
    // rest still receive pushes, and builds happen regardless (closures
    // land in the local store either way).
    let logged_in_caches: Vec<CacheTarget> = opts
        .effective_caches()
        .into_iter()
        .filter(|t| match prebuild_cache::attic_login_target(t) {
            Ok(()) => true,
            Err(e) => {
                eprintln!(
                    "[prebuild] attic login for '{}' failed; skipping that cache: {e:#}",
                    t.cache_name
                );
                audit.log(
                    "prebuild_attic_login_failed",
                    serde_json::json!({ "cache": t.cache_name, "error": format!("{e:#}") }),
                );
                false
            }
        })
        .collect();
    if !opts.quiet {
        println!(
            "[prebuild] cache fan-out: {} cache(s) ready, packages={:?}, repro={}",
            logged_in_caches.len(),
            opts.selector,
            opts.repro.verifies(),
        );
    }
    let caches_arc: Arc<Vec<CacheTarget>> = Arc::new(logged_in_caches);
    // One in-memory closure-dedup table for the whole cycle, so a closure
    // shared across many repos/packages is pushed to each cache at most
    // once — keeps network + atticd load proportional to *new* closure.
    let dedup: Arc<Mutex<ClosureDedup>> = Arc::new(Mutex::new(ClosureDedup::new()));
    // The effectful seam (real nix/attic/git). `Arc<dyn CacheFillEnv>` is
    // Send+Sync because the trait is, so it crosses spawn_blocking fine.
    let env_arc: Arc<dyn prebuild_cache::CacheFillEnv> = Arc::new(prebuild_cache::RealEnv);

    // Snapshot the full repo list up front so the iteration doesn't
    // straddle the await boundary with a borrow of `cfg`. Each entry
    // carries its owning workspace name so the per-repo Job can scope
    // its JobId to `JobScope::Workspace(...)`.
    let mut all_repos: Vec<(String, PathBuf)> = Vec::new();
    for ws in crate::filter_workspaces(&cfg.workspaces, ws_filter) {
        let base = ws.resolved_base_dir()?;
        // Honour workspace-level `prebuild:` config from shikumi:
        // a workspace can declare its own intervals + attic cache
        // without changing the CLI flags. Merged for log visibility
        // only — the runtime budget + attic-login are global per cycle.
        let effective = effective_per_workspace(&opts, ws.prebuild.as_ref());
        if !effective.quiet {
            println!(
                "[prebuild] workspace={} base={} max_inflight={} attic_cache={:?}",
                ws.name,
                base.display(),
                effective.max_inflight,
                effective.attic.as_ref().map(|a| &a.cache_name),
            );
        }
        for repo in enumerate_repos_with_flake(&base) {
            all_repos.push((ws.name.clone(), repo));
        }
    }

    // The concurrency bound, as a typed per-kind budget — the
    // replacement for the legacy `Semaphore`. The scheduler will only
    // execute up to `max_inflight` PrebuildRepoJobs simultaneously even
    // when more are Ready (mirrors reconcile's pull-budget).
    let max_inflight = u32::try_from(opts.max_inflight.max(1)).unwrap_or(u32::MAX);

    // AuditLog itself isn't Clone (it owns a PathBuf and an implicit
    // append-handle contract via the file path). Wrap once in Arc so
    // every Job gets a cheap reference-counted handle.
    let audit_arc: Arc<AuditLog> = Arc::new(AuditLog::new(audit.path().to_path_buf()));
    let opts_arc: Arc<PrebuildOptions> = Arc::new(opts.clone());

    // The cycle is already audit-driven — point the scheduler's
    // transition emitter at the same audit log so every Job's
    // Pending→Ready→Running→Succeeded (+ retries) lands in the JSONL
    // trail alongside the prebuild_built/prebuild_failed events.
    let emitter: Arc<dyn TransitionEmitter> = Arc::new(
        AuditFileEmitter::new(audit.path())
            .with_context(|| format!("opening transition log {}", audit.path().display()))?,
    );
    let scheduler = InProcessScheduler::new("tend.prebuild").with_emitter(emitter);

    let mut budget = BudgetTree::new();
    budget.by_kind.insert(
        JobKindId::new(crate::jobs::prebuild_repo::PREBUILD_REPO_KIND),
        BudgetSpec::max_concurrent(max_inflight),
    );
    scheduler.install_budget(budget).await;

    // One shared sink captures every Job's typed BuildOutcome; the
    // summary is folded from these after the cycle ticks to quiescence.
    let sink: Arc<InMemorySink<BuildOutcome>> = Arc::new(InMemorySink::new());
    let sink_for_jobs: Arc<dyn OutputSink<BuildOutcome>> = sink.clone();

    let mut dag = Dag::new();
    let mut all_ids: Vec<JobId> = Vec::with_capacity(all_repos.len());
    for (workspace, repo) in all_repos {
        let key = repo.display().to_string();
        // Snapshot the previous-seen rev so the Job doesn't hold the
        // SeenCache lock across the build.
        let prev_rev = seen
            .lock()
            .expect("seen cache mutex poisoned")
            .revs
            .get(&key)
            .cloned();
        let repo_name = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| key.clone());

        let job = Arc::new(
            crate::jobs::prebuild_repo::PrebuildRepoJob::new(
                repo,
                repo_name,
                workspace,
                prev_rev,
                Arc::clone(&opts_arc),
                Arc::clone(&caches_arc),
                Arc::clone(&dedup),
                Arc::clone(&audit_arc),
                Arc::clone(&env_arc),
                Arc::clone(&seen),
                seen_path.clone(),
            )
            .with_output_sink(sink_for_jobs.clone()),
        );
        let id = <crate::jobs::prebuild_repo::PrebuildRepoJob as shigoto_types::Job>::id(&job);
        all_ids.push(id.clone());
        dag.ensure_node(id);
        scheduler.register_job(job).await;
    }

    // Tick the scheduler to quiescence — the loop EXITS as soon as a tick
    // advances no Job (`transitions_this_tick.is_empty()`), so the cap is
    // pure runaway-protection, never the normal exit. Unlike reconcile
    // (high concurrency over cheap git-pulls, where a flat 64 always
    // drains), prebuild runs at LOW `max_inflight` (builds are expensive)
    // over the WHOLE org — 200+ repos at concurrency 1-2 needs far more
    // than 64 waves to drain, and a premature cap would miscount still-
    // pending Jobs as `failed` below. So scale the ceiling with the Job
    // count (× a per-Job FSM+retry slack); a stuck/looping scheduler still
    // trips it. Each Job's seen-cache update happens inside its own
    // execute_body, so the cycle no longer re-reads HEAD here.
    let max_ticks = all_ids.len().saturating_mul(8).saturating_add(64);
    for _ in 0..max_ticks {
        let receipt = scheduler.tick(&mut dag).await?;
        if receipt.transitions_this_tick.is_empty() {
            break;
        }
    }

    // Fold the captured outcomes into the rollup summary.
    let mut final_summary = PrebuildSummary::default();
    for outcome in sink.drain().values() {
        final_summary.absorb(outcome);
    }
    // The `failed` count comes from the scheduler snapshot, exactly like
    // reconcile derives its Failed bucket: any Job whose terminal phase
    // is not Succeeded (Deadlettered after retries, still Retrying at the
    // tick cap, …). A `prebuild_one` `anyhow::Error` surfaces as a
    // PrebuildRepoError::Invocation → the Job deadletters → it lands here.
    let snap = scheduler.snapshot(&dag).await;
    final_summary.failed = all_ids
        .iter()
        .filter(|id| !matches!(snap.phases.get(id), Some(JobPhase::Succeeded)))
        .count();

    // Report the in-memory dedup effect: how many distinct (cache, path)
    // pushes the cycle issued after collapsing the org's overlapping
    // closures. A high number relative to repos built = the fan-out is
    // doing real work; near-zero = everything was already cached.
    if !opts.quiet {
        let d = dedup.lock().expect("closure dedup mutex poisoned");
        if !d.is_empty() {
            println!(
                "[prebuild] cache fan-out: {} unique (cache,path) pushes issued this cycle",
                d.len()
            );
        }
    }

    Ok(final_summary)
}

/// Build one repo (if its HEAD has moved). Returns the typed outcome
/// without mutating the seen-cache — the caller updates `seen` only
/// after a successful build, so partial-progress is preserved across
/// crashes. Takes `prev_rev: Option<&str>` rather than borrowing the
/// shared cache so this function is trivially callable from inside a
/// `spawn_blocking` closure.
pub(crate) fn prebuild_one(
    env: &dyn prebuild_cache::CacheFillEnv,
    repo: &Path,
    prev_rev: Option<&str>,
    opts: &PrebuildOptions,
    caches: &[CacheTarget],
    dedup: &Mutex<ClosureDedup>,
    audit: &AuditLog,
) -> Result<BuildOutcome> {
    let rev = env.git_head_rev(repo)?;
    let key = repo.display().to_string();
    if prev_rev == Some(rev.as_str()) {
        return Ok(BuildOutcome::NoChange);
    }

    if !opts.quiet {
        println!(
            "[prebuild] building {} @ {}",
            repo.display(),
            &rev[..rev.len().min(8)]
        );
    }

    // Resolve the build units. `Default` keeps the legacy fast-path
    // (`nix build .` → packages.${currentSystem}.default, no flake-show
    // round-trip). `All`/`Named` enumerate every selected output across
    // the wanted systems via `nix flake show --json`.
    let installables: Vec<String> = match &opts.selector {
        PackageSelector::Default => vec![".".to_string()],
        sel => {
            let json = match env.flake_show(repo) {
                Ok(j) => j,
                Err(e) => {
                    // A flake that can't even `flake show` is a non-build
                    // repo for our purposes — soft skip, like NoDefault.
                    if !opts.quiet {
                        eprintln!("[prebuild] {key}: flake show failed, skipping: {e:#}");
                    }
                    return Ok(BuildOutcome::NoDefault);
                }
            };
            let pkgs = prebuild_cache::flake_show_packages(&json, &opts.systems, sel)
                .with_context(|| format!("parsing flake show for {key}"))?;
            if pkgs.is_empty() {
                return Ok(BuildOutcome::NoDefault);
            }
            pkgs.iter()
                .map(prebuild_cache::PackageRef::installable)
                .collect()
        }
    };

    let mut all_out_paths: Vec<String> = Vec::new();
    let mut pushed = 0usize;
    let mut any_built = false;

    for installable in &installables {
        let out = env.build(repo, installable)?;
        if out.is_empty() {
            // Selected attr absent here (e.g. a Named output a given repo
            // lacks) — soft skip this installable, keep going.
            continue;
        }
        any_built = true;

        // Anti-poison gate (2026-06-02 incident): never push a
        // non-reproducible closure to a substitution-source cache. The
        // gate is closure-deep — it `--check`s every locally-built
        // derivation in this installable's pushed closure (substituted
        // paths are skipped, trusted-by-origin) and withholds the WHOLE
        // installable unless ALL are provably reproducible. On withhold,
        // keep the local artifact but skip the push so the fleet never
        // substitutes an SVH-fragile build. `verifies()` short-circuits
        // BEFORE `verify_closure` so Trusting never pays the cost.
        if !caches.is_empty() && opts.repro.verifies() {
            let outcome = env.verify_closure(repo, &out);
            if outcome != prebuild_cache::DeterminismOutcome::Reproducible {
                audit.log(
                    "prebuild_nonreproducible_withheld",
                    serde_json::json!({
                        "repo": key,
                        "installable": installable,
                        "outcome": format!("{outcome:?}"),
                    }),
                );
                if !opts.quiet {
                    eprintln!(
                        "[prebuild] {key}: {installable} not provably reproducible ({outcome:?}) — built, push withheld"
                    );
                }
                all_out_paths.extend(out);
                continue;
            }
        }

        for path in &out {
            pushed += prebuild_cache::push_path_to_caches(env, caches, path, dedup);
        }
        all_out_paths.extend(out);
    }

    if !any_built {
        return Ok(BuildOutcome::NoDefault);
    }

    audit.log(
        "prebuild_built",
        serde_json::json!({
            "repo": key,
            "rev": rev,
            "installables": installables.len(),
            "out_paths": all_out_paths.len(),
            "pushed": pushed,
        }),
    );

    Ok(BuildOutcome::Built {
        out_paths: all_out_paths,
        pushed,
    })
}

/// Merge a workspace-level `prebuild:` declaration onto the
/// CLI-derived [`PrebuildOptions`]. The workspace block, when
/// present, **overrides** the CLI for any field it sets (non-zero
/// numerics, `Some(_)` for attic-* options). This is the shikumi
/// surface — operators edit `config.yaml`'s `prebuild:` to change
/// daemon behavior; the daemon re-loads the config every cycle so
/// edits propagate within one interval, no restart needed.
pub(crate) fn effective_per_workspace(
    cli: &PrebuildOptions,
    ws: Option<&crate::config::PrebuildConfig>,
) -> PrebuildOptions {
    let mut out = cli.clone();
    let Some(ws) = ws else {
        return out;
    };
    if ws.max_inflight > 0 {
        out.max_inflight = ws.max_inflight;
    }
    // The two interval knobs live on the daemon loop (not in
    // PrebuildOptions which models a single cycle), so a workspace
    // can't yet override them without restructuring. Tracked as a
    // follow-up; the typed surface is what matters for now.
    if let (Some(cache), Some(server), Some(url), Some(token)) = (
        ws.attic_cache.as_ref(),
        ws.attic_server.as_ref(),
        ws.attic_url.as_ref(),
        ws.attic_token_file.as_ref(),
    ) {
        out.attic = Some(AtticPush {
            cache_name: cache.clone(),
            server_name: server.clone(),
            server_url: url.clone(),
            token_file: PathBuf::from(token),
        });
    }
    out
}

/// Enumerate immediate children of `base` that contain `flake.nix`
/// AND a `.git/` (or `.git` worktree marker file). One level deep on
/// purpose — pleme-io workspaces are `{base}/{repo}/...` shaped, never
/// nested. We sort by repo name so the cycle order is deterministic
/// across runs (useful for tail-the-journal debugging).
pub(crate) fn enumerate_repos_with_flake(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let flake = p.join("flake.nix");
        let git = p.join(".git");
        if flake.exists() && git.exists() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Capture the HEAD rev of a repo via `git rev-parse HEAD`.
pub(crate) fn git_head_rev(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git rev-parse HEAD in {}", repo.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// nix's "does not provide attribute" comes in a handful of phrasings
/// depending on whether the flake declares `packages` at all, declares
/// it for a different system, or sets `default` explicitly. Cover the
/// observed ones — false positives just mean we treat a real failure
/// as `NoDefault`, which is the same outcome we'd want anyway
/// (skip-and-move-on).
pub(crate) fn missing_default_attribute(stderr: &str) -> bool {
    stderr.contains("does not provide attribute")
        || stderr.contains("flake does not provide attribute")
        || stderr.contains("attribute 'default' missing")
        || stderr.contains("error: flake 'git+file:") && stderr.contains("does not provide")
}

/// Shared test fixtures usable from sibling test modules (notably
/// [`crate::jobs::prebuild_repo`]). Kept `pub(crate)` so the
/// `PrebuildRepoJob` tests can drive `prebuild_one` through a no-real-IO
/// seam without duplicating the mock. Compiled only under `cfg(test)`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::prebuild_cache::{CacheFillEnv, DeterminismOutcome};

    /// A minimal no-real-IO [`CacheFillEnv`] for Job-level tests. Unlike
    /// the richer `MockCacheFillEnv` in `prebuild`'s own `tests` module
    /// (which programs per-installable build/verify outcomes), this one
    /// only needs to (a) hand back a canonical `Default`-selector build
    /// path so `prebuild_one` reaches `Built`, and (b) optionally
    /// `panic!` if `build` is ever reached — proving the `NoChange`
    /// short-circuit never touched the seam. `git_head_rev` is delegated
    /// to the real implementation so tempdir-git tests observe genuine
    /// revs.
    #[derive(Default)]
    pub(crate) struct MinimalEnv {
        /// When true, `build` panics — used to assert the no-build path.
        panic_on_build: bool,
    }

    impl MinimalEnv {
        /// A `MinimalEnv` whose `build` panics if reached.
        pub(crate) fn panic_on_build() -> Self {
            Self {
                panic_on_build: true,
            }
        }
    }

    impl CacheFillEnv for MinimalEnv {
        fn git_head_rev(&self, repo: &Path) -> Result<String> {
            // Defer to the real rev capture so tempdir-git fixtures see
            // a genuine HEAD (the no-change short-circuit needs it).
            git_head_rev(repo)
        }

        fn flake_show(&self, _repo: &Path) -> Result<String> {
            Ok(r#"{"packages":{}}"#.to_string())
        }

        fn build(&self, _repo: &Path, _installable: &str) -> Result<Vec<String>> {
            assert!(
                !self.panic_on_build,
                "build reached but the test forbade it"
            );
            Ok(vec!["/nix/store/hash-default".to_string()])
        }

        fn verify_closure(&self, _repo: &Path, _out_paths: &[String]) -> DeterminismOutcome {
            DeterminismOutcome::Reproducible
        }

        fn push(&self, _target: &CacheTarget, _path: &str) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Workspace;
    use crate::prebuild_cache::DeterminismOutcome;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A no-real-I/O [`prebuild_cache::CacheFillEnv`] for the WS3 truth
    /// tables. Every method is programmable; nothing touches nix, git,
    /// attic, or the network — so these tests run anywhere.
    struct MockCacheFillEnv {
        /// Returned by `git_head_rev`.
        head: String,
        /// Returned by `flake_show` (the json) or an Err if `None`.
        flake_show: std::result::Result<String, String>,
        /// Per-installable `build` results. Default below when absent.
        builds: HashMap<String, std::result::Result<Vec<String>, String>>,
        /// Default `build` result for installables not in `builds`.
        default_build: std::result::Result<Vec<String>, String>,
        /// Per-installable verify outcome; default `verify_default`.
        verifies: HashMap<String, DeterminismOutcome>,
        verify_default: DeterminismOutcome,
        /// Interior call counter — proves `verify_closure` was/wasn't run.
        verify_calls: AtomicUsize,
        /// Recorded `(cache, path)` pushes.
        pushes: Mutex<Vec<(String, String)>>,
        /// A cache name that `attic_push` programmatically fails for.
        fail_push_cache: Option<String>,
    }

    impl MockCacheFillEnv {
        fn new() -> Self {
            Self {
                head: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
                flake_show: Ok(r#"{"packages":{}}"#.to_string()),
                builds: HashMap::new(),
                default_build: Ok(vec!["/nix/store/hash-default".to_string()]),
                verifies: HashMap::new(),
                verify_default: DeterminismOutcome::Reproducible,
                verify_calls: AtomicUsize::new(0),
                pushes: Mutex::new(Vec::new()),
                fail_push_cache: None,
            }
        }

        fn with_verify_default(mut self, o: DeterminismOutcome) -> Self {
            self.verify_default = o;
            self
        }

        fn with_build(
            mut self,
            installable: &str,
            r: std::result::Result<Vec<String>, String>,
        ) -> Self {
            self.builds.insert(installable.to_string(), r);
            self
        }

        fn with_verify(mut self, installable: &str, o: DeterminismOutcome) -> Self {
            self.verifies.insert(installable.to_string(), o);
            self
        }

        fn with_flake_show(mut self, json: &str) -> Self {
            self.flake_show = Ok(json.to_string());
            self
        }

        fn with_push_failure(mut self, cache: &str) -> Self {
            self.fail_push_cache = Some(cache.to_string());
            self
        }

        fn pushes(&self) -> Vec<(String, String)> {
            self.pushes.lock().expect("pushes mutex poisoned").clone()
        }

        fn verify_call_count(&self) -> usize {
            self.verify_calls.load(Ordering::SeqCst)
        }
    }

    impl prebuild_cache::CacheFillEnv for MockCacheFillEnv {
        fn git_head_rev(&self, _repo: &Path) -> Result<String> {
            Ok(self.head.clone())
        }

        fn flake_show(&self, _repo: &Path) -> Result<String> {
            self.flake_show
                .clone()
                .map_err(|e| anyhow::anyhow!("mock flake_show err: {e}"))
        }

        fn build(&self, _repo: &Path, installable: &str) -> Result<Vec<String>> {
            let r = self.builds.get(installable).unwrap_or(&self.default_build);
            r.clone()
                .map_err(|e| anyhow::anyhow!("mock build err: {e}"))
        }

        fn verify_closure(&self, _repo: &Path, _out_paths: &[String]) -> DeterminismOutcome {
            self.verify_calls.fetch_add(1, Ordering::SeqCst);
            // verify is modeled PER-INSTALLABLE in prebuild (the gate
            // runs once per installable's closure). The mock keys verify
            // outcomes off the out-path so a test can give one installable
            // a Reproducible result and another NonReproducible without
            // implying any transitive verification across installables.
            for p in _out_paths {
                if let Some(o) = self.verifies.get(p) {
                    return o.clone();
                }
            }
            self.verify_default.clone()
        }

        fn push(&self, target: &CacheTarget, path: &str) -> Result<()> {
            // Keyed on dedup_key, not cache_name, so this mock stays
            // meaningful for a sui target (whose cache_name is empty).
            let key = target.dedup_key();
            if self.fail_push_cache.as_deref() == Some(key) {
                anyhow::bail!("mock programmed push failure for cache {key}");
            }
            self.pushes
                .lock()
                .expect("pushes mutex poisoned")
                .push((key.to_string(), path.to_string()));
            Ok(())
        }
    }

    /// One usable cache target for the WS3 push tests.
    fn usable_cache(name: &str) -> CacheTarget {
        CacheTarget {
            backend: prebuild_cache::CacheBackend::Attic,
            cache_name: name.to_string(),
            server_name: name.to_string(),
            server_url: "http://rio:8080/".to_string(),
            token_file: "/run/secrets/tend/jwt".to_string(),
            enabled: true,
        }
    }

    /// One usable SUI target — URL only, exactly as the retirement of
    /// attic leaves it: no cache name, no alias, no token.
    fn usable_sui(url: &str) -> CacheTarget {
        CacheTarget {
            backend: prebuild_cache::CacheBackend::Sui,
            cache_name: String::new(),
            server_name: String::new(),
            server_url: url.to_string(),
            token_file: String::new(),
            enabled: true,
        }
    }

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tend-prebuild-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn summary_absorb_built_increments_built_and_pushed() {
        let mut s = PrebuildSummary::default();
        s.absorb(&BuildOutcome::Built {
            out_paths: vec!["/nix/store/x".to_string(), "/nix/store/y".to_string()],
            pushed: 2,
        });
        assert_eq!(s.built, 1);
        assert_eq!(s.pushed, 2);
        assert_eq!(s.no_change, 0);
        assert_eq!(s.work(), 1, "work() drives backoff reset");
    }

    #[test]
    fn summary_absorb_no_change_does_not_count_as_work() {
        let mut s = PrebuildSummary::default();
        s.absorb(&BuildOutcome::NoChange);
        assert_eq!(s.no_change, 1);
        assert_eq!(s.work(), 0, "no_change must not trigger backoff reset");
    }

    #[test]
    fn summary_absorb_no_default_recorded_but_not_work() {
        let mut s = PrebuildSummary::default();
        s.absorb(&BuildOutcome::NoDefault);
        assert_eq!(s.skipped_no_default, 1);
        assert_eq!(s.work(), 0);
    }

    #[test]
    fn seen_cache_roundtrip_through_disk() {
        let dir = tmpdir();
        let p = dir.join("seen.json");
        let mut c = SeenCache::default();
        c.revs.insert("/x/foo".into(), "abc123".into());
        c.revs.insert("/x/bar".into(), "def456".into());
        c.save_to(&p).unwrap();

        let loaded = SeenCache::load_from(&p);
        assert_eq!(loaded.revs.len(), 2);
        assert_eq!(loaded.revs.get("/x/foo").unwrap(), "abc123");
        assert_eq!(loaded.revs.get("/x/bar").unwrap(), "def456");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_cache_load_missing_file_yields_empty() {
        let dir = tmpdir();
        let p = dir.join("does-not-exist.json");
        let c = SeenCache::load_from(&p);
        assert!(c.revs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_cache_load_corrupt_file_yields_empty_not_panic() {
        let dir = tmpdir();
        let p = dir.join("bad.json");
        fs::write(&p, "not-json-at-all").unwrap();
        let c = SeenCache::load_from(&p);
        assert!(c.revs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_cache_save_creates_parent_dirs() {
        let dir = tmpdir();
        let p = dir.join("nested/sub/seen.json");
        let mut c = SeenCache::default();
        c.revs.insert("/x".into(), "rev".into());
        c.save_to(&p).unwrap();
        assert!(p.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumerate_skips_dirs_without_flake_nix() {
        let dir = tmpdir();
        // repo-a: has flake + .git → included
        let a = dir.join("repo-a");
        fs::create_dir_all(a.join(".git")).unwrap();
        fs::write(a.join("flake.nix"), "{}").unwrap();
        // repo-b: has .git but no flake → excluded
        let b = dir.join("repo-b");
        fs::create_dir_all(b.join(".git")).unwrap();
        // repo-c: has flake but no .git → excluded
        let c = dir.join("repo-c");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("flake.nix"), "{}").unwrap();
        // file: not a dir → excluded
        fs::write(dir.join("README.md"), "hi").unwrap();

        let found = enumerate_repos_with_flake(&dir);
        assert_eq!(found.len(), 1, "exactly one buildable repo");
        assert_eq!(found[0].file_name().unwrap(), "repo-a");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumerate_missing_base_returns_empty_not_error() {
        let p = PathBuf::from("/tmp/does-not-exist-deadbeef-tend-prebuild");
        let found = enumerate_repos_with_flake(&p);
        assert!(found.is_empty());
    }

    #[test]
    fn enumerate_output_is_sorted_deterministic() {
        let dir = tmpdir();
        for name in &["zz", "aa", "mm"] {
            let p = dir.join(name);
            fs::create_dir_all(p.join(".git")).unwrap();
            fs::write(p.join("flake.nix"), "{}").unwrap();
        }
        let found = enumerate_repos_with_flake(&dir);
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["aa", "mm", "zz"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_default_attribute_matches_known_phrasings() {
        assert!(missing_default_attribute(
            "error: flake does not provide attribute 'packages.x86_64-linux.default'"
        ));
        assert!(missing_default_attribute(
            "error: attribute 'default' missing"
        ));
        assert!(missing_default_attribute(
            "error: does not provide attribute 'packages'"
        ));
        // Unrelated error must NOT match — those are real failures.
        assert!(!missing_default_attribute(
            "error: hash mismatch in fixed-output derivation"
        ));
        assert!(!missing_default_attribute(
            "error: builder for '/nix/store/x.drv' failed"
        ));
    }

    #[test]
    fn effective_per_workspace_none_returns_cli_unchanged() {
        let cli = PrebuildOptions {
            quiet: true,
            max_inflight: 3,
            attic: None,
            ..Default::default()
        };
        let out = effective_per_workspace(&cli, None);
        assert_eq!(out.max_inflight, 3);
        assert!(out.attic.is_none());
        assert!(out.quiet);
    }

    #[test]
    fn effective_per_workspace_zero_max_inflight_doesnt_override() {
        // A workspace declaring `prebuild: {}` with all defaults
        // (max_inflight=0 via TieredConfig::bare()) MUST NOT clamp
        // the CLI's max_inflight to zero — the merge treats 0 as
        // "unspecified."
        let cli = PrebuildOptions {
            quiet: false,
            max_inflight: 4,
            attic: None,
            ..Default::default()
        };
        let ws = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: None,
            attic_server: None,
            attic_url: None,
            attic_token_file: None,
            ..Default::default()
        };
        let out = effective_per_workspace(&cli, Some(&ws));
        assert_eq!(out.max_inflight, 4, "zero must not override");
    }

    #[test]
    fn effective_per_workspace_overrides_max_inflight_when_set() {
        let cli = PrebuildOptions {
            quiet: false,
            max_inflight: 1,
            attic: None,
            ..Default::default()
        };
        let ws = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 6,
            attic_cache: None,
            attic_server: None,
            attic_url: None,
            attic_token_file: None,
            ..Default::default()
        };
        let out = effective_per_workspace(&cli, Some(&ws));
        assert_eq!(out.max_inflight, 6, "workspace value wins");
    }

    #[test]
    fn effective_per_workspace_overrides_attic_only_when_full_quartet_set() {
        let cli = PrebuildOptions {
            quiet: false,
            max_inflight: 1,
            attic: None,
            ..Default::default()
        };
        // Partial — missing token_file → MUST NOT install a half-baked
        // AtticPush that'd crash at login time.
        let partial = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: Some("nexus".into()),
            attic_server: Some("nexus".into()),
            attic_url: Some("http://rio:8080/".into()),
            attic_token_file: None,
            ..Default::default()
        };
        let out = effective_per_workspace(&cli, Some(&partial));
        assert!(out.attic.is_none(), "partial spec must not install attic");

        // Full quartet — install.
        let full = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: Some("nexus".into()),
            attic_server: Some("nexus".into()),
            attic_url: Some("http://rio:8080/".into()),
            attic_token_file: Some("/run/secrets/tend/attic-jwt-token".into()),
            ..Default::default()
        };
        let out = effective_per_workspace(&cli, Some(&full));
        let attic = out.attic.expect("full quartet must install attic");
        assert_eq!(attic.cache_name, "nexus");
        assert_eq!(attic.server_url, "http://rio:8080/");
        assert_eq!(
            attic.token_file,
            PathBuf::from("/run/secrets/tend/attic-jwt-token")
        );
    }

    #[test]
    fn reachability_unreachable_sleep_doubles_then_caps() {
        let opts = ReachabilityOptions {
            enabled: true,
            url: "http://rio:8080/".into(),
            min_interval: 60,
            max_interval: 1800,
            probe_timeout: 5,
        };
        // 1-based: first strike = min, then doubling.
        assert_eq!(opts.unreachable_sleep(1), 60);
        assert_eq!(opts.unreachable_sleep(2), 120);
        assert_eq!(opts.unreachable_sleep(3), 240);
        assert_eq!(opts.unreachable_sleep(4), 480);
        assert_eq!(opts.unreachable_sleep(5), 960);
        // 6th strike would be 1920 > 1800 → cap.
        assert_eq!(opts.unreachable_sleep(6), 1800);
        // Far-future strikes stay capped, never overflow.
        assert_eq!(opts.unreachable_sleep(40), 1800);
        assert_eq!(opts.unreachable_sleep(u32::MAX), 1800);
    }

    #[test]
    fn reachability_unreachable_sleep_respects_floor_and_min_ge_one() {
        // Degenerate config: min=0 must clamp to >=1 so we never
        // busy-spin probing a dead server.
        let opts = ReachabilityOptions {
            enabled: true,
            url: String::new(),
            min_interval: 0,
            max_interval: 0,
            probe_timeout: 0,
        };
        // min clamps to 1; max clamps to >= min, so everything is 1.
        assert_eq!(opts.unreachable_sleep(1), 1);
        assert_eq!(opts.unreachable_sleep(10), 1);
    }

    #[test]
    fn reachability_default_has_best_defaults() {
        let opts = ReachabilityOptions::default();
        assert!(!opts.enabled, "off until an attic url is wired");
        assert_eq!(opts.min_interval, 60);
        assert_eq!(opts.max_interval, 1800);
        assert_eq!(opts.probe_timeout, 5);
    }

    #[test]
    fn git_head_rev_on_nonexistent_repo_errors() {
        let p = PathBuf::from("/tmp/no-such-repo-tend-prebuild-xyz");
        let r = git_head_rev(&p);
        assert!(r.is_err(), "git rev-parse on missing repo must error");
    }

    /// Initialise a minimal git repo with one commit, return (dir,
    /// head_rev). Used by the prebuild_one idempotency test below.
    fn init_one_commit_repo() -> Option<(PathBuf, String)> {
        let dir = tmpdir();
        let run = |args: &[&str]| -> bool {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .ok()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None; // no git available — skip test
        }
        let _ = run(&["config", "user.email", "tend-test@local"]);
        let _ = run(&["config", "user.name", "tend-test"]);
        fs::write(dir.join("README"), "hi").ok()?;
        if !run(&["add", "README"]) {
            return None;
        }
        if !run(&["commit", "-q", "-m", "init"]) {
            return None;
        }
        let rev = git_head_rev(&dir).ok()?;
        Some((dir, rev))
    }

    #[test]
    fn prebuild_one_returns_no_change_when_prev_rev_matches_head() {
        let Some((dir, rev)) = init_one_commit_repo() else {
            eprintln!("git unavailable; skipping prebuild_one_no_change test");
            return;
        };
        let opts = PrebuildOptions::default();
        let audit = AuditLog::default_path();
        // Pass the current HEAD as prev_rev — prebuild_one must return
        // NoChange without ever invoking nix build (so no flake.nix is
        // required for the test).
        let dedup = Mutex::new(ClosureDedup::new());
        let outcome = prebuild_one(
            &prebuild_cache::RealEnv,
            &dir,
            Some(&rev),
            &opts,
            &[],
            &dedup,
            &audit,
        )
        .unwrap();
        assert_eq!(outcome, BuildOutcome::NoChange);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prebuild_one_rev_mismatch_with_no_flake_returns_no_default() {
        // When prev_rev != HEAD, prebuild_one DOES invoke nix build.
        // Without a flake.nix it'll fail with "does not provide
        // attribute" → we treat that as NoDefault. This test confirms
        // the soft-skip path for non-buildable repos.
        let Some((dir, _rev)) = init_one_commit_repo() else {
            eprintln!("git unavailable; skipping prebuild_one_no_default test");
            return;
        };
        // Skip the test if nix isn't on PATH — CI may not have it.
        if Command::new("nix").arg("--version").output().is_err() {
            eprintln!("nix unavailable; skipping prebuild_one_no_default test");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let opts = PrebuildOptions::default();
        let audit = AuditLog::default_path();
        // prev_rev is None → mismatch, build will be attempted, will fail.
        let dedup = Mutex::new(ClosureDedup::new());
        let outcome = prebuild_one(
            &prebuild_cache::RealEnv,
            &dir,
            None,
            &opts,
            &[],
            &dedup,
            &audit,
        );
        // Either NoDefault (preferred) or an Err containing the nix
        // failure — both are acceptable signals that the non-flake
        // repo wasn't silently treated as Built.
        match outcome {
            Ok(BuildOutcome::NoDefault) => {}
            Ok(BuildOutcome::Built { .. }) => {
                panic!("repo with no flake.nix must not appear as Built")
            }
            Ok(BuildOutcome::NoChange) => {
                panic!("rev mismatch must not yield NoChange")
            }
            Err(_) => {} // nix may emit a phrasing we don't classify
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ── WS3: prebuild_one + push_path_to_caches via MockCacheFillEnv ──
    // No nix / git / attic / network. The mock's HEAD is "deadbeef…";
    // passing prev_rev=Some("old") forces the build path. Selector
    // Default builds installable ".".

    /// Build VerifyBeforePush options with the given caches.
    fn verify_opts(caches: Vec<CacheTarget>) -> PrebuildOptions {
        PrebuildOptions {
            quiet: true,
            caches,
            repro: ReproPolicy::VerifyBeforePush,
            ..Default::default()
        }
    }

    #[test]
    fn prebuild_one_withholds_push_on_nonreproducible_but_keeps_artifact() {
        // (a) VerifyBeforePush + non-empty caches + verify→NonReproducible.
        let env =
            MockCacheFillEnv::new().with_verify_default(DeterminismOutcome::NonReproducible {
                path: Some("/nix/store/poison".into()),
            });
        let opts = verify_opts(vec![usable_cache("nexus")]);
        let caches = opts.effective_caches();
        let dedup = Mutex::new(ClosureDedup::new());
        let audit = AuditLog::default_path();
        let outcome = prebuild_one(
            &env,
            Path::new("/tmp/repo"),
            Some("oldrev"),
            &opts,
            &caches,
            &dedup,
            &audit,
        )
        .unwrap();
        // Built — the local artifact is kept even when the push is withheld.
        match outcome {
            BuildOutcome::Built { out_paths, pushed } => {
                assert_eq!(pushed, 0, "non-reproducible → nothing pushed");
                assert!(!out_paths.is_empty(), "artifact kept locally");
            }
            other => panic!("expected Built, got {other:?}"),
        }
        // The withheld path: verify ran once, nothing reached attic.
        assert_eq!(env.verify_call_count(), 1);
        assert!(env.pushes().is_empty(), "withheld → no attic_push");
    }

    #[test]
    fn prebuild_one_pushes_when_reproducible() {
        // (b) verify→Reproducible ⇒ pushed==1, pushes()==[(cache,path)].
        let env = MockCacheFillEnv::new(); // default verify = Reproducible
        let opts = verify_opts(vec![usable_cache("nexus")]);
        let caches = opts.effective_caches();
        let dedup = Mutex::new(ClosureDedup::new());
        let audit = AuditLog::default_path();
        let outcome = prebuild_one(
            &env,
            Path::new("/tmp/repo"),
            Some("oldrev"),
            &opts,
            &caches,
            &dedup,
            &audit,
        )
        .unwrap();
        assert_eq!(
            outcome,
            BuildOutcome::Built {
                out_paths: vec!["/nix/store/hash-default".into()],
                pushed: 1,
            }
        );
        assert_eq!(env.verify_call_count(), 1);
        assert_eq!(
            env.pushes(),
            vec![("nexus".to_string(), "/nix/store/hash-default".to_string())]
        );
    }

    #[test]
    fn prebuild_one_trusting_never_verifies_or_withholds() {
        // (c) Trusting + non-empty caches ⇒ verify_call_count()==0 AND
        // pushed==1. Trusting must short-circuit BEFORE verify_closure.
        let env = MockCacheFillEnv::new()
            // Even if verify WERE called it'd say NonReproducible — proving
            // Trusting never calls it (else this would withhold).
            .with_verify_default(DeterminismOutcome::NonReproducible { path: None });
        let opts = PrebuildOptions {
            quiet: true,
            caches: vec![usable_cache("nexus")],
            repro: ReproPolicy::Trusting,
            ..Default::default()
        };
        let caches = opts.effective_caches();
        let dedup = Mutex::new(ClosureDedup::new());
        let audit = AuditLog::default_path();
        let outcome = prebuild_one(
            &env,
            Path::new("/tmp/repo"),
            Some("oldrev"),
            &opts,
            &caches,
            &dedup,
            &audit,
        )
        .unwrap();
        assert_eq!(env.verify_call_count(), 0, "Trusting must not verify");
        match outcome {
            BuildOutcome::Built { pushed, .. } => assert_eq!(pushed, 1),
            other => panic!("expected Built, got {other:?}"),
        }
    }

    #[test]
    fn prebuild_one_empty_caches_never_verifies_or_pushes() {
        // (d) empty caches + VerifyBeforePush ⇒ verify_call_count()==0
        // and pushed==0 (no caches → nothing to gate or push).
        let env = MockCacheFillEnv::new();
        let opts = verify_opts(vec![]); // no caches
        let caches = opts.effective_caches();
        assert!(caches.is_empty());
        let dedup = Mutex::new(ClosureDedup::new());
        let audit = AuditLog::default_path();
        let outcome = prebuild_one(
            &env,
            Path::new("/tmp/repo"),
            Some("oldrev"),
            &opts,
            &caches,
            &dedup,
            &audit,
        )
        .unwrap();
        assert_eq!(env.verify_call_count(), 0, "no caches → no verify");
        match outcome {
            BuildOutcome::Built { pushed, .. } => assert_eq!(pushed, 0),
            other => panic!("expected Built, got {other:?}"),
        }
        assert!(env.pushes().is_empty());
    }

    #[test]
    fn push_path_to_caches_fans_out_dedups_and_never_aborts() {
        let env = MockCacheFillEnv::new();
        let targets = vec![
            usable_cache("a"),
            usable_cache("b"),
            // disabled → never reaches attic_push.
            CacheTarget {
                enabled: false,
                ..usable_cache("c")
            },
            // half-specified → unusable → never reaches attic_push.
            CacheTarget {
                token_file: String::new(),
                ..usable_cache("d")
            },
        ];
        let dedup = Mutex::new(ClosureDedup::new());
        // First call: one path → 2 usable caches, pushed once each.
        let n = prebuild_cache::push_path_to_caches(&env, &targets, "/nix/store/p", &dedup);
        assert_eq!(n, 2, "two usable caches");
        assert_eq!(env.pushes().len(), 2, "disabled/half-spec never pushed");
        // Second call, SAME path + SAME dedup → 0 (already claimed).
        let n2 = prebuild_cache::push_path_to_caches(&env, &targets, "/nix/store/p", &dedup);
        assert_eq!(n2, 0, "dedup blocks the repeat");
        assert_eq!(env.pushes().len(), 2, "no extra pushes on repeat");
    }

    #[test]
    fn fan_out_mixes_attic_and_sui_targets() {
        // The post-attic-retirement shape: attic and sui side by side.
        // Two sui targets both have cache_name == "", so this also
        // guards the dedup-collision trap — key on the URL and they are
        // distinct destinations; key on cache_name and the second is
        // silently dropped.
        let env = MockCacheFillEnv::new();
        // Spelled once, asserted from the same binding below: a restated
        // literal is free to disagree with the target it exists to pin.
        let in_cluster_sui = "http://sui.build-cache.svc";
        let targets = vec![
            usable_cache("nexus"),
            usable_sui("http://127.0.0.1:5000"),
            usable_sui(in_cluster_sui),
        ];
        let dedup = Mutex::new(ClosureDedup::new());
        let n = prebuild_cache::push_path_to_caches(&env, &targets, "/nix/store/p", &dedup);
        assert_eq!(n, 3, "one attic + two distinct sui destinations");

        let keys: Vec<String> = env.pushes().iter().map(|(k, _)| k.clone()).collect();
        assert!(keys.contains(&"nexus".to_string()));
        assert!(keys.contains(&"http://127.0.0.1:5000".to_string()));
        assert!(keys.contains(&in_cluster_sui.to_string()));

        // Same path again → dedup blocks every backend equally.
        let n2 = prebuild_cache::push_path_to_caches(&env, &targets, "/nix/store/p", &dedup);
        assert_eq!(n2, 0, "dedup applies across backends");
    }

    #[test]
    fn push_path_to_caches_per_cache_failure_does_not_abort_fanout() {
        // [good, broken] and [broken, good] both attempt BOTH and return
        // the usable-count minus the failing push — pins non-aborting
        // continue.
        for order in [["good", "broken"], ["broken", "good"]] {
            let env = MockCacheFillEnv::new().with_push_failure("broken");
            let targets = vec![usable_cache(order[0]), usable_cache(order[1])];
            let dedup = Mutex::new(ClosureDedup::new());
            let n = prebuild_cache::push_path_to_caches(&env, &targets, "/nix/store/p", &dedup);
            assert_eq!(n, 1, "only the good cache counts as success");
            // The good cache was pushed; the broken one was attempted
            // (claimed under dedup) but errored — exactly one recorded push.
            let pushes = env.pushes();
            assert_eq!(pushes.len(), 1, "only the good push recorded");
            assert_eq!(pushes[0].0, "good");
        }
    }

    #[test]
    fn prebuild_one_multi_package_soft_skip_and_per_installable_gate() {
        // Selector Named(["present","absent"]); flake_show exposes both.
        // build("absent")→Ok(empty) soft-skips; build("present")→Ok(["p"]).
        let flake_json = r#"{"packages":{"host":{"present":{},"absent":{}}}}"#;
        let present = ".#packages.host.present";
        let absent = ".#packages.host.absent";
        let env = MockCacheFillEnv::new()
            .with_flake_show(flake_json)
            .with_build(present, Ok(vec!["/nix/store/p".into()]))
            .with_build(absent, Ok(vec![]));
        let opts = PrebuildOptions {
            quiet: true,
            caches: vec![usable_cache("nexus")],
            repro: ReproPolicy::VerifyBeforePush,
            selector: PackageSelector::Named(vec!["present".into(), "absent".into()]),
            systems: vec!["host".into()],
            ..Default::default()
        };
        let caches = opts.effective_caches();
        let dedup = Mutex::new(ClosureDedup::new());
        let audit = AuditLog::default_path();
        let outcome = prebuild_one(
            &env,
            Path::new("/tmp/repo"),
            Some("oldrev"),
            &opts,
            &caches,
            &dedup,
            &audit,
        )
        .unwrap();
        match outcome {
            BuildOutcome::Built { out_paths, pushed } => {
                assert_eq!(
                    out_paths,
                    vec!["/nix/store/p".to_string()],
                    "only the present path"
                );
                assert_eq!(pushed, 1);
            }
            other => panic!("expected Built, got {other:?}"),
        }
    }

    #[test]
    fn prebuild_one_mixed_repro_withholds_only_flaky_keeps_both_local() {
        // present(verify→Reproducible) + flaky(verify→NonReproducible)
        // both build non-empty ⇒ Built, pushes() contains present only,
        // all_out_paths includes BOTH (withheld keeps local).
        // NOTE: verify is modeled PER-INSTALLABLE in the mock (keyed off
        // the out-path) — matching prebuild's per-installable gate. The
        // mock does NOT imply transitive verification across installables.
        let flake_json = r#"{"packages":{"host":{"present":{},"flaky":{}}}}"#;
        let present = ".#packages.host.present";
        let flaky = ".#packages.host.flaky";
        let env = MockCacheFillEnv::new()
            .with_flake_show(flake_json)
            .with_build(present, Ok(vec!["/nix/store/present".into()]))
            .with_build(flaky, Ok(vec!["/nix/store/flaky".into()]))
            .with_verify("/nix/store/present", DeterminismOutcome::Reproducible)
            .with_verify(
                "/nix/store/flaky",
                DeterminismOutcome::NonReproducible {
                    path: Some("/nix/store/flaky".into()),
                },
            );
        let opts = PrebuildOptions {
            quiet: true,
            caches: vec![usable_cache("nexus")],
            repro: ReproPolicy::VerifyBeforePush,
            selector: PackageSelector::Named(vec!["present".into(), "flaky".into()]),
            systems: vec!["host".into()],
            ..Default::default()
        };
        let caches = opts.effective_caches();
        let dedup = Mutex::new(ClosureDedup::new());
        let audit = AuditLog::default_path();
        let outcome = prebuild_one(
            &env,
            Path::new("/tmp/repo"),
            Some("oldrev"),
            &opts,
            &caches,
            &dedup,
            &audit,
        )
        .unwrap();
        match outcome {
            BuildOutcome::Built { out_paths, pushed } => {
                assert_eq!(pushed, 1, "only the reproducible one pushed");
                // Both artifacts kept locally (withheld keeps local).
                assert!(out_paths.contains(&"/nix/store/present".to_string()));
                assert!(out_paths.contains(&"/nix/store/flaky".to_string()));
            }
            other => panic!("expected Built, got {other:?}"),
        }
        assert_eq!(
            env.pushes(),
            vec![("nexus".to_string(), "/nix/store/present".to_string())],
            "only the reproducible installable reached attic"
        );
    }

    // ── WS4: pure PrebuildOptions methods (no seam, no nix host) ──────

    #[test]
    fn effective_caches_promotes_legacy_attic_when_no_explicit_caches() {
        let opts = PrebuildOptions {
            attic: Some(AtticPush {
                cache_name: "nexus".into(),
                server_name: "nexus".into(),
                server_url: "http://rio:8080/".into(),
                token_file: PathBuf::from("/run/secrets/tend/jwt"),
            }),
            caches: vec![],
            ..Default::default()
        };
        let eff = opts.effective_caches();
        assert_eq!(eff.len(), 1, "legacy attic promoted to one element");
        assert_eq!(eff[0].cache_name, "nexus");
        assert_eq!(eff[0].server_url, "http://rio:8080/");
        assert_eq!(eff[0].token_file, "/run/secrets/tend/jwt");
        assert!(eff[0].is_usable());
    }

    #[test]
    fn effective_caches_explicit_list_wins_over_legacy_attic() {
        let opts = PrebuildOptions {
            attic: Some(AtticPush {
                cache_name: "legacy".into(),
                server_name: "legacy".into(),
                server_url: "http://rio:8080/".into(),
                token_file: PathBuf::from("/run/secrets/tend/jwt"),
            }),
            caches: vec![usable_cache("explicit")],
            ..Default::default()
        };
        let eff = opts.effective_caches();
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].cache_name, "explicit", "explicit caches win");
    }

    #[test]
    fn effective_caches_retains_only_usable_from_explicit_list() {
        let opts = PrebuildOptions {
            caches: vec![
                usable_cache("good"),
                CacheTarget {
                    enabled: false,
                    ..usable_cache("disabled")
                },
            ],
            ..Default::default()
        };
        let eff = opts.effective_caches();
        assert_eq!(eff.len(), 1, "disabled dropped by retain(is_usable)");
        assert_eq!(eff[0].cache_name, "good");
    }

    #[test]
    fn effective_caches_empty_when_no_attic_and_no_caches() {
        let opts = PrebuildOptions {
            attic: None,
            caches: vec![],
            ..Default::default()
        };
        assert!(opts.effective_caches().is_empty());
    }

    /// A Config whose Nth workspace carries the given PrebuildConfig.
    fn config_with_prebuild_on(
        idx: usize,
        total: usize,
        pc: crate::config::PrebuildConfig,
    ) -> Config {
        let workspaces = (0..total)
            .map(|i| {
                let mut ws = Workspace::test_default(&format!("ws{i}"));
                if i == idx {
                    ws.prebuild = Some(pc.clone());
                }
                ws
            })
            .collect();
        Config {
            workspaces,
            host_health: Default::default(),
        }
    }

    #[test]
    fn with_fill_from_config_reads_first_declaring_workspace_not_index_zero() {
        // Prebuild block on the SECOND workspace (first has None). find_map
        // must pick it — a [0]-indexed regression would miss it entirely.
        let pc = crate::config::PrebuildConfig {
            packages: "mado,tear".into(),
            systems: vec!["aarch64-darwin".into()],
            repro: "verify".into(),
            ..Default::default()
        };
        let cfg = config_with_prebuild_on(1, 2, pc);
        let out = PrebuildOptions::default().with_fill_from_config(&cfg);
        assert_eq!(
            out.selector,
            PackageSelector::Named(vec!["mado".into(), "tear".into()])
        );
        assert_eq!(out.systems, vec!["aarch64-darwin".to_string()]);
        assert!(out.repro.verifies());
    }

    #[test]
    fn with_fill_from_config_empty_caches_preserve_cli_caches_but_apply_rest() {
        // The prebuild block sets selector/systems/repro but has an EMPTY
        // caches list — the narrow guard must NOT clobber the CLI caches,
        // yet must still apply selector/systems/repro.
        let pc = crate::config::PrebuildConfig {
            packages: "all".into(),
            systems: vec!["x86_64-linux".into()],
            repro: "verify".into(),
            caches: vec![], // empty
            ..Default::default()
        };
        let cfg = config_with_prebuild_on(0, 1, pc);
        let cli = PrebuildOptions {
            caches: vec![usable_cache("cli-cache")],
            ..Default::default()
        };
        let out = cli.with_fill_from_config(&cfg);
        assert_eq!(out.caches.len(), 1, "CLI caches preserved");
        assert_eq!(out.caches[0].cache_name, "cli-cache");
        assert_eq!(out.selector, PackageSelector::All);
        assert_eq!(out.systems, vec!["x86_64-linux".to_string()]);
        assert!(out.repro.verifies());
    }

    #[test]
    fn with_fill_from_config_no_prebuild_block_is_identity() {
        let cfg = Config {
            workspaces: vec![
                Workspace::test_default("ws0"),
                Workspace::test_default("ws1"),
            ],
            host_health: Default::default(),
        };
        let cli = PrebuildOptions {
            caches: vec![usable_cache("cli-cache")],
            selector: PackageSelector::Default,
            ..Default::default()
        };
        let out = cli.clone().with_fill_from_config(&cfg);
        assert_eq!(out.caches.len(), 1);
        assert_eq!(out.caches[0].cache_name, "cli-cache");
        assert_eq!(out.selector, PackageSelector::Default);
        assert!(!out.repro.verifies(), "Trusting preserved (identity)");
    }
}
