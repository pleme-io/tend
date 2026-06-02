//! Cache-fill primitives for the prebuild subsystem.
//!
//! Extends prebuild from "build `packages.${system}.default`, push to ONE
//! attic cache" to "build EVERY selected flake package across the whole
//! org's repos, fan each resulting closure out to MANY caches, skip what's
//! already cached, never push a non-reproducible artifact."
//!
//! Three design forces, each a direct consequence of this fleet's history:
//!
//! 1. **Many caches, many packages — typed, not stringly.** A
//!    [`CacheTarget`] is one push destination; a cycle carries a
//!    `Vec<CacheTarget>`. A [`PackageSelector`] decides which flake
//!    outputs to realise. Both parse from the shikumi `prebuild:` block
//!    with great defaults so "just turn it on" ships the maximum.
//!
//! 2. **In-memory dedup — push the minimum invocations.** Org-wide
//!    builds produce massively-overlapping closures (every Rust repo
//!    shares serde, tokio, …). [`ClosureDedup`] is a per-cycle in-memory
//!    set so a `(cache, store_path)` pair triggers at most one `attic
//!    push` per cycle. Server-side, `attic push`'s own get-missing-paths
//!    skips paths the cache already holds — so the two together keep
//!    network + atticd load proportional to *new* closure, not total.
//!
//! 3. **Reproducibility gate — never amplify cache poison.** The
//!    2026-06-02 incident proved that pushing non-reproducible artifacts
//!    (the derive_deftly / wasmtime / cranelift SVH-fragile closures)
//!    poisons the cache for the whole fleet. The poison lived in an
//!    *intermediate* proc-macro `.rustc` crate-SVH — never in the
//!    top-level output — so the gate has to be **closure-deep**:
//!    [`ReproPolicy::VerifyBeforePush`] makes the fill loop
//!    `nix-store --realise --check` EVERY locally-built derivation in
//!    the pushed closure (skipping substituted / unknown-deriver paths,
//!    which were verified by whoever built them), classify each result
//!    with [`classify_determinism`], and withhold the WHOLE leaf unless
//!    ALL are provably [`DeterminismOutcome::Reproducible`]
//!    ([`aggregate_closure_determinism`]). So the cache-filler becomes
//!    the thing that keeps the cache CONSISTENT rather than the thing
//!    that breaks it. Off by default — it rebuilds the locally-built
//!    closure, which is expensive; on for any cache the fleet trusts for
//!    substitution.

use serde::Deserialize;
use std::collections::HashSet;

/// One binary-cache push destination. A prebuild cycle fans every
/// produced closure out to each enabled target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTarget {
    /// Cache name as known by atticd (e.g. `nexus`).
    pub cache_name: String,
    /// Server alias passed to `attic login`.
    pub server_name: String,
    /// Attic server root URL (e.g. `http://rio:8080/`).
    pub server_url: String,
    /// Path to a file holding the atticd JWT. Read once per cycle.
    pub token_file: String,
    /// When false the target is skipped entirely (lets an operator keep
    /// a cache declared but paused without deleting its config).
    pub enabled: bool,
}

impl CacheTarget {
    /// A target is *usable* only when its four identity fields are
    /// non-empty AND it's enabled. Half-specified targets are silently
    /// dropped rather than crashing `attic login` mid-cycle — same
    /// belt-and-suspenders posture as the legacy single-cache quartet
    /// check in [`crate::prebuild::effective_per_workspace`].
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.enabled
            && !self.cache_name.is_empty()
            && !self.server_name.is_empty()
            && !self.server_url.is_empty()
            && !self.token_file.is_empty()
    }
}

/// Wire form of a [`CacheTarget`] as a typed Nix module renders it:
/// `builtins.toJSON` of a `caches` submodule list. Decouples the Nix/CLI
/// boundary (snake_case JSON) from the runtime type so the module surface
/// can evolve its attr names independently. This is the
/// Nix → JSON → Rust machine boundary the fleet standardises on.
#[derive(Debug, Deserialize)]
struct CacheTargetSpec {
    name: String,
    server: String,
    url: String,
    token_file: String,
    #[serde(default = "spec_enabled_default")]
    enabled: bool,
}

fn spec_enabled_default() -> bool {
    true
}

/// Parse a `--caches-json '[{…},…]'` argument (rendered by the Nix module
/// from its typed `caches` list) into runtime [`CacheTarget`]s. Empty
/// array ⇒ no multi-cache (caller falls back to the legacy single
/// `--attic-*` quartet). A malformed value is a hard error — a typed Nix
/// surface can't emit one, so seeing it means a hand-edit to fix.
pub fn parse_caches_json(json: &str) -> Result<Vec<CacheTarget>, serde_json::Error> {
    let specs: Vec<CacheTargetSpec> = serde_json::from_str(json)?;
    Ok(specs
        .into_iter()
        .map(|s| CacheTarget {
            cache_name: s.name,
            server_name: s.server,
            server_url: s.url,
            token_file: s.token_file,
            enabled: s.enabled,
        })
        .collect())
}

/// Which flake outputs a cycle realises. The whole point of "ship as
/// many packages as possible" is [`PackageSelector::All`] — the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSelector {
    /// Only `packages.${system}.default` — the legacy behavior.
    Default,
    /// Every `packages.${system}.*` the flake exposes. Default for the
    /// org-fill mode: maximise cache coverage.
    All,
    /// An explicit allow-list of attribute names (e.g. `["mado","tear"]`).
    Named(Vec<String>),
}

impl PackageSelector {
    /// Parse the shikumi `packages:` knob. `"all"` (any case) → All;
    /// `"default"`/empty → Default; anything else is treated as a
    /// comma-separated allow-list. Total + lossless: an operator can't
    /// write a value that fails to parse.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let t = raw.trim();
        match t.to_ascii_lowercase().as_str() {
            "all" | "*" => Self::All,
            "" | "default" => Self::Default,
            _ => {
                let names: Vec<String> = t
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if names.is_empty() {
                    Self::Default
                } else {
                    Self::Named(names)
                }
            }
        }
    }

    /// Does this selector admit the given flake-output attribute name?
    #[must_use]
    pub fn admits(&self, attr: &str) -> bool {
        match self {
            Self::Default => attr == "default",
            Self::All => true,
            Self::Named(names) => names.iter().any(|n| n == attr),
        }
    }
}

/// A concrete buildable flake output: `.#packages.${system}.${attr}`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PackageRef {
    pub system: String,
    pub attr: String,
}

impl PackageRef {
    /// The flake installable string nix consumes for this output.
    #[must_use]
    pub fn installable(&self) -> String {
        let mut s = String::with_capacity(self.system.len() + self.attr.len() + 12);
        s.push_str(".#packages.");
        s.push_str(&self.system);
        s.push('.');
        s.push_str(&self.attr);
        s
    }
}

/// Minimal view of `nix flake show --json` — we only care about the
/// `packages` tree. serde ignores every other top-level key.
#[derive(Debug, Deserialize)]
struct FlakeShow {
    #[serde(default)]
    packages: std::collections::BTreeMap<String, std::collections::BTreeMap<String, serde_json::Value>>,
}

/// Parse `nix flake show --json` into the concrete set of [`PackageRef`]s
/// to build, filtered by the wanted `systems` and the `selector`.
///
/// Pure: no nix invocation, no I/O — the caller runs `nix flake show`
/// and hands the bytes here. Deterministic ordering (BTreeMap +
/// post-sort) so a cycle's build order is stable across runs.
///
/// `systems` empty ⇒ accept every system the flake advertises (lets an
/// operator say "build for whatever this flake targets" without
/// enumerating triples).
pub fn flake_show_packages(
    json: &str,
    systems: &[String],
    selector: &PackageSelector,
) -> Result<Vec<PackageRef>, serde_json::Error> {
    let show: FlakeShow = serde_json::from_str(json)?;
    let mut out = Vec::new();
    for (system, attrs) in &show.packages {
        if !systems.is_empty() && !systems.iter().any(|s| s == system) {
            continue;
        }
        for attr in attrs.keys() {
            if selector.admits(attr) {
                out.push(PackageRef {
                    system: system.clone(),
                    attr: attr.clone(),
                });
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Per-cycle, in-memory record of which `(cache, store_path)` pairs have
/// already been handled, so a closure shared across many repos/packages
/// is pushed to each cache at most once. Lives for one cycle only — not
/// persisted, deliberately: the on-disk seen-rev cache governs *whether
/// to build*; this governs *whether to push within a build wave*, where
/// staleness would be a correctness bug (a path evicted from the cache
/// between cycles must be re-pushable).
#[derive(Debug, Default)]
pub struct ClosureDedup {
    seen: HashSet<(String, String)>,
}

impl ClosureDedup {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim `(cache, path)` for pushing. Returns true the FIRST time the
    /// pair is seen (caller should push), false on every repeat (caller
    /// skips — already handled this cycle). Atomic claim-and-mark so two
    /// concurrent build tasks producing the same closure don't both push.
    pub fn claim(&mut self, cache_name: &str, store_path: &str) -> bool {
        self.seen
            .insert((cache_name.to_string(), store_path.to_string()))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// What the fill loop does about reproducibility before pushing a closure
/// to a *trusted* (substitution-source) cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReproPolicy {
    /// Push whatever was built. Fast, but a non-reproducible build
    /// poisons the cache for the fleet (the 2026-06-02 incident).
    Trusting,
    /// Before pushing, `--check` every *locally-built* derivation in the
    /// pushed closure (substituted / unknown-deriver paths are skipped —
    /// they were verified by whoever originally built them) and push only
    /// when ALL of them are provably reproducible. This is genuinely
    /// closure-deep: it catches the intermediate proc-macro SVH that
    /// poisoned the cache in the 2026-06-02 incident, which a top-level
    /// `nix build --rebuild` would have missed (nix reuses already-built
    /// dependency crates as-valid and never re-checks them). The cost is
    /// honest — it rebuilds the locally-built closure — so it's opt-in,
    /// for any cache the fleet trusts as a substitution source.
    VerifyBeforePush,
}

impl ReproPolicy {
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "verify" | "verify-before-push" | "true" | "yes" => Self::VerifyBeforePush,
            _ => Self::Trusting,
        }
    }

    #[must_use]
    pub fn verifies(self) -> bool {
        matches!(self, Self::VerifyBeforePush)
    }
}

/// The typed result of `--check`ing ONE locally-built derivation, and —
/// after [`aggregate_closure_determinism`] — of the whole pushed closure.
/// Replaces the old `bool` so the fill loop can distinguish "proven
/// reproducible" from "proven non-reproducible" from "couldn't prove
/// either way" — the last two both withhold the push, but for different
/// reasons the audit log records faithfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeterminismOutcome {
    /// The `--check` rebuild matched the resident output byte-for-byte.
    /// Safe to push.
    Reproducible,
    /// nix reported the build "may not be deterministic" — the poison
    /// class. `path` is the differing output path when nix names it.
    NonReproducible { path: Option<String> },
    /// The derivation isn't resident / was substituted, so `--check`
    /// can't run on it. Not a failure — someone else built+verified it;
    /// we simply have nothing to prove here. `reason` is the tail of
    /// nix's message for the audit trail.
    Uncheckable { reason: String },
    /// nix exited nonzero for some reason we don't classify. Conservative:
    /// an unknown failure must withhold, never push on a maybe. `stderr_tail`
    /// is the tail of the message for the audit trail.
    Inconclusive { stderr_tail: String },
}

/// Tail of `s` (last `n` chars), trimmed. Used to keep audit-trail
/// reasons bounded without dragging a whole nix stderr into the log.
fn tail_trimmed(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        return t.to_string();
    }
    let start = t.chars().count() - n;
    t.chars().skip(start).collect::<String>().trim().to_string()
}

/// Extract the differing output path from a nix `--check` failure, i.e.
/// the `<path>` in `output '<path>' differs`. Returns `None` when nix
/// names no path (older phrasings just say "may not be deterministic").
fn extract_differs_path(stderr: &str) -> Option<String> {
    let marker = "output '";
    let start = stderr.find(marker)? + marker.len();
    let rest = &stderr[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Pure classifier of ONE `nix-store --realise --check <drv>` result into
/// a [`DeterminismOutcome`]. No I/O — the caller runs nix and hands the
/// (success, stderr) pair here so this stays unit-testable without a
/// store. Precedence is "worst signal wins" within a single check:
/// success → reproducible; the determinism phrasing → non-reproducible;
/// the can't-check phrasings → uncheckable; anything else → inconclusive.
#[must_use]
pub fn classify_determinism(success: bool, stderr: &str) -> DeterminismOutcome {
    if success {
        return DeterminismOutcome::Reproducible;
    }
    if stderr.contains("may not be deterministic") {
        return DeterminismOutcome::NonReproducible {
            path: extract_differs_path(stderr),
        };
    }
    if stderr.contains("is not valid") || stderr.contains("checking is not possible") {
        return DeterminismOutcome::Uncheckable {
            reason: tail_trimmed(stderr, 120),
        };
    }
    DeterminismOutcome::Inconclusive {
        stderr_tail: tail_trimmed(stderr, 200),
    }
}

/// Pure all-or-none aggregator over a closure's per-derivation checks.
/// The whole leaf is only pushable when at least one derivation proved
/// [`DeterminismOutcome::Reproducible`] and NONE failed. Precedence,
/// in order:
///
/// 1. any [`DeterminismOutcome::NonReproducible`] ⇒ NonReproducible (the
///    first, preserving its `path`) — poison present, withhold.
/// 2. else any [`DeterminismOutcome::Inconclusive`] ⇒ Inconclusive (the
///    first) — an unknown failure must withhold, never push on a maybe.
/// 3. else any [`DeterminismOutcome::Reproducible`] ⇒ Reproducible —
///    ≥1 proven and nothing failed; trailing `Uncheckable`s are
///    substituted paths we legitimately skip.
/// 4. else (empty, or every check was `Uncheckable`) ⇒ Uncheckable —
///    we proved nothing, so conservatively withhold rather than push a
///    closure none of which we could verify.
#[must_use]
pub fn aggregate_closure_determinism(per_drv: &[DeterminismOutcome]) -> DeterminismOutcome {
    if let Some(nonrepro) = per_drv
        .iter()
        .find(|o| matches!(o, DeterminismOutcome::NonReproducible { .. }))
    {
        return nonrepro.clone();
    }
    if let Some(inconc) = per_drv
        .iter()
        .find(|o| matches!(o, DeterminismOutcome::Inconclusive { .. }))
    {
        return inconc.clone();
    }
    if per_drv
        .iter()
        .any(|o| matches!(o, DeterminismOutcome::Reproducible))
    {
        return DeterminismOutcome::Reproducible;
    }
    DeterminismOutcome::Uncheckable {
        reason: "no resident derivations to check".to_string(),
    }
}

// ── Effectful layer ─────────────────────────────────────────────────
// Subprocess + closure-dedup wrappers. Kept thin and out of the pure
// functions above so the parse/select/dedup logic stays unit-testable
// without nix, attic, or a network. Each is a typed wrapper around one
// `Command` shape — the "typed subprocess function" idiom this fleet
// uses instead of ad-hoc shell.

use anyhow::{Context, Result};
use std::process::Command;
use std::sync::Mutex;

/// `attic login <server> <url> <token>` for one target. Run once per
/// cycle per cache. Reads the JWT fresh each time (never held between
/// cycles) so a rotated token is picked up on the next cycle.
pub fn attic_login_target(target: &CacheTarget) -> Result<()> {
    let token = std::fs::read_to_string(&target.token_file)
        .with_context(|| format!("reading {}", target.token_file))?;
    let status = Command::new("attic")
        .args(["login", &target.server_name, &target.server_url, token.trim()])
        .status()
        .context("running attic login")?;
    if !status.success() {
        anyhow::bail!("attic login for cache '{}' exited {}", target.cache_name, status);
    }
    Ok(())
}

/// `nix flake show --json` in `repo`. Returns the raw JSON for
/// [`flake_show_packages`] to parse. A flake that exposes no `packages`
/// still returns valid JSON (`{"packages":{}}`), so the only Err here is
/// a genuine eval failure — which the caller treats as a soft skip, same
/// as the legacy missing-default path.
pub fn run_flake_show(repo: &std::path::Path) -> Result<String> {
    let out = Command::new("nix")
        .args([
            "flake", "show", "--json", "--all-systems",
            "--option", "warn-dirty", "false",
        ])
        .current_dir(repo)
        .output()
        .with_context(|| format!("nix flake show in {}", repo.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "nix flake show failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `nix build <installable> --print-out-paths` in `repo`. Returns the
/// realised out-paths. `installable` is e.g. `.#packages.<sys>.<attr>`
/// (from [`PackageRef::installable`]) or `"."` for the legacy default.
pub fn nix_build_installable(repo: &std::path::Path, installable: &str) -> Result<Vec<String>> {
    let out = Command::new("nix")
        .args([
            "build", installable,
            "--no-link", "--print-out-paths", "--refresh",
            "--option", "warn-dirty", "false",
        ])
        .current_dir(repo)
        .output()
        .with_context(|| format!("nix build {installable} in {}", repo.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if crate::prebuild::missing_default_attribute(&stderr) {
            // Caller treats an empty Vec for a selected attr as "this
            // output doesn't exist here" — soft skip, not a hard fail.
            return Ok(Vec::new());
        }
        anyhow::bail!("nix build {installable} failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// The effectful seam the cache-fill loop runs against. Mirrors
/// [`crate::git::GitOps`] exactly: a `Send + Sync` trait of typed
/// subprocess wrappers ([`RealEnv`] in production) so the orchestration
/// in [`crate::prebuild::prebuild_one`] / [`push_path_to_caches`] is
/// unit-testable with a mock — no nix, no attic, no git, no network.
/// `Send + Sync` is what lets an `Arc<dyn CacheFillEnv>` cross the
/// `spawn_blocking` boundary in [`crate::prebuild::run_cycle`].
pub(crate) trait CacheFillEnv: Send + Sync {
    /// HEAD rev of `repo` (`git rev-parse HEAD`).
    fn git_head_rev(&self, repo: &std::path::Path) -> Result<String>;
    /// `nix flake show --json` in `repo` — raw JSON for
    /// [`flake_show_packages`] to parse.
    fn flake_show(&self, repo: &std::path::Path) -> Result<String>;
    /// `nix build <installable> --print-out-paths` in `repo`. `Ok(empty)`
    /// is a soft skip — the selected attr is absent here.
    fn build(&self, repo: &std::path::Path, installable: &str) -> Result<Vec<String>>;
    /// Closure-deep reproducibility check of the union runtime closure of
    /// `out_paths` (see [`RealEnv::verify_closure`] for the algorithm).
    fn verify_closure(&self, repo: &std::path::Path, out_paths: &[String]) -> DeterminismOutcome;
    /// `attic push <cache> <path>`. Result-returning so a per-cache
    /// failure is assertable and the fan-out in [`push_path_to_caches`]
    /// can `continue` past it without aborting the rest.
    fn attic_push(&self, cache: &str, path: &str) -> Result<()>;
}

/// Production [`CacheFillEnv`] — real subprocess calls.
pub(crate) struct RealEnv;

impl CacheFillEnv for RealEnv {
    fn git_head_rev(&self, repo: &std::path::Path) -> Result<String> {
        crate::prebuild::git_head_rev(repo)
    }

    fn flake_show(&self, repo: &std::path::Path) -> Result<String> {
        run_flake_show(repo)
    }

    fn build(&self, repo: &std::path::Path, installable: &str) -> Result<Vec<String>> {
        nix_build_installable(repo, installable)
    }

    /// Closure-deep reproducibility verification — the genuinely
    /// closure-deep gate the 2026-06-02 incident demanded. A top-level
    /// `nix build --rebuild` only re-checks the LEAF output; nix reuses
    /// already-built dependency crates as-valid and never re-checks
    /// them, so the intermediate proc-macro SVH that actually poisoned
    /// the cache slips through. This instead:
    ///
    /// 1. `nix-store -qR <out_path…>` — the union runtime closure of the
    ///    pushed paths.
    /// 2. for each store path, `nix-store -q --deriver <path>` — its
    ///    `.drv`. Paths whose deriver is unknown / empty / not a resident
    ///    `.drv` file were SUBSTITUTED (built+verified by someone else),
    ///    so we skip them — not our responsibility.
    /// 3. for each remaining resident `.drv`,
    ///    `nix-store --realise --check <drv>` — rebuild-and-compare.
    ///    Classify each via [`classify_determinism`].
    /// 4. [`aggregate_closure_determinism`] folds the per-drv results
    ///    all-or-none.
    ///
    /// Any nix invocation that errors folds in as
    /// [`DeterminismOutcome::Inconclusive`] — an unknown failure
    /// conservatively withholds rather than pushing on a maybe.
    fn verify_closure(&self, repo: &std::path::Path, out_paths: &[String]) -> DeterminismOutcome {
        if out_paths.is_empty() {
            return aggregate_closure_determinism(&[]);
        }
        // 1. Union runtime closure of every pushed out-path.
        let mut qr = Command::new("nix-store");
        qr.arg("-qR");
        for p in out_paths {
            qr.arg(p);
        }
        let closure = match qr.current_dir(repo).output() {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>(),
            Ok(o) => {
                return DeterminismOutcome::Inconclusive {
                    stderr_tail: tail_trimmed(&String::from_utf8_lossy(&o.stderr), 200),
                };
            }
            Err(e) => {
                return DeterminismOutcome::Inconclusive {
                    stderr_tail: tail_trimmed(&e.to_string(), 200),
                };
            }
        };

        let mut per_drv: Vec<DeterminismOutcome> = Vec::new();
        // Dedupe derivers: a multi-output drv reaches the closure via
        // several store paths, but rebuild-and-compare need only run once.
        let mut checked: HashSet<String> = HashSet::new();
        for store_path in &closure {
            // 2. Find the deriver. Substituted paths report
            // "unknown-deriver" or an empty line — skip them.
            let deriver = match Command::new("nix-store")
                .args(["-q", "--deriver", store_path])
                .current_dir(repo)
                .output()
            {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                Ok(_) => continue, // can't resolve a deriver → skip (substituted)
                Err(e) => {
                    per_drv.push(DeterminismOutcome::Inconclusive {
                        stderr_tail: tail_trimmed(&e.to_string(), 200),
                    });
                    continue;
                }
            };
            if deriver.is_empty()
                || deriver == "unknown-deriver"
                || !deriver.ends_with(".drv")
                || !std::path::Path::new(&deriver).is_file()
            {
                // Substituted / unknown-deriver — verified by its origin.
                continue;
            }
            if !checked.insert(deriver.clone()) {
                continue; // already rebuilt-and-compared this drv this closure
            }

            // 3. Rebuild-and-compare this one derivation.
            match Command::new("nix-store")
                .args(["--realise", "--check", &deriver])
                .current_dir(repo)
                .output()
            {
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    per_drv.push(classify_determinism(o.status.success(), &stderr));
                }
                Err(e) => per_drv.push(DeterminismOutcome::Inconclusive {
                    stderr_tail: tail_trimmed(&e.to_string(), 200),
                }),
            }
        }

        // 4. All-or-none fold.
        aggregate_closure_determinism(&per_drv)
    }

    fn attic_push(&self, cache: &str, path: &str) -> Result<()> {
        let status = Command::new("attic")
            .args(["push", cache, path])
            .status()
            .with_context(|| format!("running attic push {cache} {path}"))?;
        if !status.success() {
            anyhow::bail!("attic push {cache} {path} exited {status}");
        }
        Ok(())
    }
}

/// Fan one store path out to every usable cache target, deduping via the
/// shared per-cycle [`ClosureDedup`] so a closure shared across repos is
/// pushed to each cache at most once. `attic push` itself skips paths the
/// server already holds (get-missing-paths), so this layer's job is to
/// avoid the redundant *invocation*, not to re-implement membership.
///
/// Returns the count of `(cache, path)` pushes that succeeded this call.
/// A failed push to one cache never aborts the others — one stuck cache
/// must not strand the rest of the fan-out. The push itself routes
/// through [`CacheFillEnv::attic_push`] so the fan-out + dedup logic is
/// unit-testable against a mock.
pub(crate) fn push_path_to_caches(
    env: &dyn CacheFillEnv,
    targets: &[CacheTarget],
    path: &str,
    dedup: &Mutex<ClosureDedup>,
) -> usize {
    let mut ok = 0;
    for t in targets.iter().filter(|t| t.is_usable()) {
        // Claim under lock; skip if another task already pushed this
        // (cache, path) this cycle.
        let fresh = {
            let mut d = dedup.lock().expect("closure dedup mutex poisoned");
            d.claim(&t.cache_name, path)
        };
        if !fresh {
            continue;
        }
        match env.attic_push(&t.cache_name, path) {
            Ok(()) => ok += 1,
            Err(e) => eprintln!("[prebuild] attic push {}→{} {e:#}", t.cache_name, path),
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_target_usable_requires_full_quartet_and_enabled() {
        let full = CacheTarget {
            cache_name: "nexus".into(),
            server_name: "nexus".into(),
            server_url: "http://rio:8080/".into(),
            token_file: "/run/secrets/tend/jwt".into(),
            enabled: true,
        };
        assert!(full.is_usable());

        let disabled = CacheTarget { enabled: false, ..full.clone() };
        assert!(!disabled.is_usable(), "disabled target is unusable");

        let partial = CacheTarget { token_file: String::new(), ..full };
        assert!(!partial.is_usable(), "missing token_file → unusable");
    }

    #[test]
    fn parse_caches_json_roundtrips_nix_rendered_list() {
        let json = r#"[
          {"name":"nexus","server":"nexus","url":"http://rio:8080/","token_file":"/run/secrets/tend/jwt"},
          {"name":"backup","server":"backup","url":"https://cache.example/","token_file":"/run/secrets/tend/jwt2","enabled":false}
        ]"#;
        let got = parse_caches_json(json).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].cache_name, "nexus");
        assert_eq!(got[0].server_url, "http://rio:8080/");
        assert!(got[0].enabled, "enabled defaults to true when omitted");
        assert!(got[0].is_usable());
        assert_eq!(got[1].cache_name, "backup");
        assert!(!got[1].enabled, "explicit enabled=false honoured");
        assert!(!got[1].is_usable(), "disabled → unusable");
        // Empty list is valid (fall back to legacy single cache).
        assert!(parse_caches_json("[]").unwrap().is_empty());
        // Malformed is a hard error.
        assert!(parse_caches_json("not json").is_err());
    }

    #[test]
    fn package_selector_parse_covers_all_default_named() {
        assert_eq!(PackageSelector::parse("all"), PackageSelector::All);
        assert_eq!(PackageSelector::parse("ALL"), PackageSelector::All);
        assert_eq!(PackageSelector::parse("*"), PackageSelector::All);
        assert_eq!(PackageSelector::parse("default"), PackageSelector::Default);
        assert_eq!(PackageSelector::parse(""), PackageSelector::Default);
        assert_eq!(PackageSelector::parse("  "), PackageSelector::Default);
        assert_eq!(
            PackageSelector::parse("mado, tear , frost"),
            PackageSelector::Named(vec!["mado".into(), "tear".into(), "frost".into()])
        );
        // A trailing/empty token list collapses back to Default, never
        // an empty Named that would match nothing.
        assert_eq!(PackageSelector::parse(", ,"), PackageSelector::Default);
    }

    #[test]
    fn package_selector_admits() {
        assert!(PackageSelector::Default.admits("default"));
        assert!(!PackageSelector::Default.admits("mado"));
        assert!(PackageSelector::All.admits("default"));
        assert!(PackageSelector::All.admits("anything"));
        let named = PackageSelector::Named(vec!["mado".into(), "tear".into()]);
        assert!(named.admits("mado"));
        assert!(named.admits("tear"));
        assert!(!named.admits("frost"));
    }

    #[test]
    fn package_ref_installable_string() {
        let p = PackageRef { system: "aarch64-darwin".into(), attr: "mado".into() };
        assert_eq!(p.installable(), ".#packages.aarch64-darwin.mado");
    }

    #[test]
    fn flake_show_all_packages_for_all_systems() {
        let json = r#"{
          "packages": {
            "aarch64-darwin": {
              "default": {"type":"derivation","name":"mado"},
              "tear":    {"type":"derivation","name":"tear"}
            },
            "x86_64-linux": {
              "default": {"type":"derivation","name":"mado-linux"}
            }
          },
          "devShells": {"aarch64-darwin": {"default": {}}}
        }"#;
        let got = flake_show_packages(json, &[], &PackageSelector::All).unwrap();
        assert_eq!(got.len(), 3, "2 darwin + 1 linux package, devShells ignored");
        // Deterministic sort: darwin before linux, default before tear.
        assert_eq!(got[0], PackageRef { system: "aarch64-darwin".into(), attr: "default".into() });
        assert_eq!(got[1], PackageRef { system: "aarch64-darwin".into(), attr: "tear".into() });
        assert_eq!(got[2], PackageRef { system: "x86_64-linux".into(), attr: "default".into() });
    }

    #[test]
    fn flake_show_filters_by_system_and_selector() {
        let json = r#"{"packages":{
          "aarch64-darwin":{"default":{},"tear":{}},
          "x86_64-linux":{"default":{},"tear":{}}
        }}"#;
        let only_darwin_default = flake_show_packages(
            json,
            &["aarch64-darwin".to_string()],
            &PackageSelector::Default,
        )
        .unwrap();
        assert_eq!(only_darwin_default.len(), 1);
        assert_eq!(only_darwin_default[0].system, "aarch64-darwin");
        assert_eq!(only_darwin_default[0].attr, "default");

        let darwin_named = flake_show_packages(
            json,
            &["aarch64-darwin".to_string()],
            &PackageSelector::Named(vec!["tear".into()]),
        )
        .unwrap();
        assert_eq!(darwin_named.len(), 1);
        assert_eq!(darwin_named[0].attr, "tear");
    }

    #[test]
    fn flake_show_empty_packages_yields_empty_not_error() {
        assert!(flake_show_packages("{}", &[], &PackageSelector::All).unwrap().is_empty());
        assert!(flake_show_packages(r#"{"packages":{}}"#, &[], &PackageSelector::All)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn flake_show_malformed_json_errors() {
        assert!(flake_show_packages("not json", &[], &PackageSelector::All).is_err());
    }

    #[test]
    fn closure_dedup_claims_once_per_cache_path_pair() {
        let mut d = ClosureDedup::new();
        assert!(d.claim("nexus", "/nix/store/x"), "first claim → push");
        assert!(!d.claim("nexus", "/nix/store/x"), "repeat → skip");
        // Same path, DIFFERENT cache → independent claim (still needs push).
        assert!(d.claim("other", "/nix/store/x"), "per-cache independence");
        assert!(d.claim("nexus", "/nix/store/y"), "different path → push");
        assert_eq!(d.len(), 3);
        assert!(!d.is_empty());
    }

    #[test]
    fn repro_policy_parse_and_verifies() {
        assert_eq!(ReproPolicy::parse("verify"), ReproPolicy::VerifyBeforePush);
        assert_eq!(ReproPolicy::parse("verify-before-push"), ReproPolicy::VerifyBeforePush);
        assert_eq!(ReproPolicy::parse("true"), ReproPolicy::VerifyBeforePush);
        assert_eq!(ReproPolicy::parse(""), ReproPolicy::Trusting);
        assert_eq!(ReproPolicy::parse("trusting"), ReproPolicy::Trusting);
        assert!(ReproPolicy::VerifyBeforePush.verifies());
        assert!(!ReproPolicy::Trusting.verifies());
    }

    #[test]
    fn classify_determinism_covers_four_branches_and_path_extraction() {
        // success → Reproducible.
        assert_eq!(
            classify_determinism(true, ""),
            DeterminismOutcome::Reproducible
        );

        // "may not be deterministic" with a named path → NonReproducible
        // carrying the differing path.
        let nonrepro = classify_determinism(
            false,
            "error: derivation '/nix/store/x.drv' may not be deterministic: \
             output '/nix/store/y-mado' differs",
        );
        assert_eq!(
            nonrepro,
            DeterminismOutcome::NonReproducible {
                path: Some("/nix/store/y-mado".to_string())
            }
        );

        // determinism phrasing WITHOUT a named path → NonReproducible{None}.
        assert_eq!(
            classify_determinism(false, "error: build may not be deterministic"),
            DeterminismOutcome::NonReproducible { path: None }
        );

        // "is not valid" → Uncheckable.
        match classify_determinism(false, "error: path '/nix/store/z' is not valid") {
            DeterminismOutcome::Uncheckable { reason } => {
                assert!(reason.contains("is not valid"));
            }
            other => panic!("expected Uncheckable, got {other:?}"),
        }
        // "checking is not possible" → Uncheckable too.
        assert!(matches!(
            classify_determinism(false, "error: checking is not possible for a substituted path"),
            DeterminismOutcome::Uncheckable { .. }
        ));

        // Anything else nonzero → Inconclusive.
        match classify_determinism(false, "error: out of disk space while building") {
            DeterminismOutcome::Inconclusive { stderr_tail } => {
                assert!(stderr_tail.contains("disk space"));
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_closure_determinism_precedence() {
        use DeterminismOutcome::{Inconclusive, NonReproducible, Reproducible, Uncheckable};

        // 1. any NonReproducible wins (first one, preserving its path) —
        // even when a Reproducible and an Inconclusive are also present.
        let nonrepro = NonReproducible { path: Some("/nix/store/p".into()) };
        assert_eq!(
            aggregate_closure_determinism(&[
                Reproducible,
                nonrepro.clone(),
                Inconclusive { stderr_tail: "x".into() },
                NonReproducible { path: Some("/nix/store/other".into()) },
            ]),
            nonrepro,
            "first NonReproducible (with its path) takes precedence"
        );

        // 2. else any Inconclusive withholds (no NonReproducible present).
        let inconc = Inconclusive { stderr_tail: "boom".into() };
        assert_eq!(
            aggregate_closure_determinism(&[Reproducible, inconc.clone(), Uncheckable { reason: "r".into() }]),
            inconc,
            "Inconclusive withholds over Reproducible"
        );

        // 3. else ≥1 Reproducible + only Uncheckables → Reproducible.
        assert_eq!(
            aggregate_closure_determinism(&[
                Uncheckable { reason: "substituted".into() },
                Reproducible,
                Uncheckable { reason: "substituted".into() },
            ]),
            Reproducible,
            "one proven + trailing substituted-skips → Reproducible"
        );

        // 4. empty → Uncheckable (proved nothing, conservative withhold).
        assert_eq!(
            aggregate_closure_determinism(&[]),
            Uncheckable { reason: "no resident derivations to check".into() }
        );
        // all-Uncheckable → same conservative withhold.
        assert_eq!(
            aggregate_closure_determinism(&[
                Uncheckable { reason: "a".into() },
                Uncheckable { reason: "b".into() },
            ]),
            Uncheckable { reason: "no resident derivations to check".into() }
        );
    }
}
