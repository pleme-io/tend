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

/// Which protocol a push destination speaks.
///
/// Attic was the fleet's only backend until it was retired on 2026-07-31
/// after twice serving Rust artifacts whose crate metadata a consumer
/// could not load. Its replacement, `sui cache serve`, is a plain HTTP
/// binary cache (`PUT /{hash}.narinfo`, `PUT /nar/{*path}`) that `nix
/// copy --to` speaks natively — no client binary, no login handshake.
///
/// Defaults to [`CacheBackend::Attic`] on the wire so every existing
/// config keeps parsing unchanged (★★ MODULARIZE, DON'T DELETE — attic
/// stays a fully working, selectable backend, it is simply not the
/// fleet's choice today).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheBackend {
    /// `attic push <cache> <path>`, preceded by `attic login`.
    #[default]
    Attic,
    /// `nix copy --to <url> <path>`. No login step.
    Sui,
}

impl CacheBackend {
    /// Does this backend need an `attic login` before pushing?
    ///
    /// A match rather than a bool field so adding a third backend is a
    /// compile error here instead of a silently-skipped handshake.
    #[must_use]
    pub fn needs_attic_login(self) -> bool {
        match self {
            Self::Attic => true,
            Self::Sui => false,
        }
    }
}

/// One binary-cache push destination. A prebuild cycle fans every
/// produced closure out to each enabled target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTarget {
    /// Which protocol this destination speaks.
    pub backend: CacheBackend,
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
    /// Usability is per-backend, because the identity fields are.
    /// Attic needs its whole quartet (name/server/url/token) to log in
    /// and push; sui needs only a URL — `nix copy --to` takes no cache
    /// name, no alias and no token, so demanding them would make every
    /// sui target silently unusable.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        if !self.enabled {
            return false;
        }
        match self.backend {
            CacheBackend::Attic => {
                !self.cache_name.is_empty()
                    && !self.server_name.is_empty()
                    && !self.server_url.is_empty()
                    && !self.token_file.is_empty()
            }
            CacheBackend::Sui => !self.server_url.is_empty(),
        }
    }

    /// Stable per-destination identity for the per-cycle push dedup and
    /// for log lines.
    ///
    /// It is NOT `cache_name`: that is empty for a sui target, so every
    /// sui destination would share the key `""` and the dedup would
    /// silently drop all but the first one's pushes. Keyed on whatever
    /// actually identifies the destination for its backend.
    #[must_use]
    pub fn dedup_key(&self) -> &str {
        match self.backend {
            CacheBackend::Attic => &self.cache_name,
            CacheBackend::Sui => &self.server_url,
        }
    }
}

/// Wire form of a [`CacheTarget`] as a typed Nix module renders it:
/// `builtins.toJSON` of a `caches` submodule list. Decouples the Nix/CLI
/// boundary (snake_case JSON) from the runtime type so the module surface
/// can evolve its attr names independently. This is the
/// Nix → JSON → Rust machine boundary the fleet standardises on.
#[derive(Debug, Deserialize)]
struct CacheTargetSpec {
    /// Absent ⇒ `attic`, so every pre-existing rendered config parses
    /// unchanged.
    #[serde(default)]
    backend: CacheBackend,
    /// Attic-only; a sui target may omit it.
    #[serde(default)]
    name: String,
    /// Attic-only; a sui target may omit it.
    #[serde(default)]
    server: String,
    url: String,
    /// Attic-only; a sui target may omit it.
    #[serde(default)]
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
            backend: s.backend,
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
    /// nix reported the output "differs", but every Rust crate artifact
    /// under it carries BYTE-IDENTICAL crate metadata — only object code
    /// drifted. Safe to push, and recorded distinctly from
    /// [`DeterminismOutcome::Reproducible`] so the audit trail never
    /// claims byte-identity it doesn't have.
    ///
    /// This distinction is the whole point of the gate on darwin. A
    /// consumer resolves a dependency by its metadata/SVH and NEVER by
    /// its object code, so object drift cannot produce the
    /// `error[E0463]: can't find crate` poison — while a whole-output
    /// `--check` cannot tell the two apart and would withhold every
    /// darwin Rust crate, leaving the cache silently empty. Canonical:
    /// `pleme-io/nix/docs/darwin-rust-cache-reproducibility.md`
    /// ("the safety property is METADATA determinism, NOT whole-output
    /// byte-identity").
    ObjectDriftOnly { path: String },
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
    // 2b. An object-drift-only result is PUSHABLE (metadata proved
    // identical) but is deliberately reported in preference to a plain
    // `Reproducible` when both are present: the closure as a whole is
    // then metadata-identical, NOT byte-identical, and the audit trail
    // must say the weaker true thing rather than the stronger false one.
    if let Some(drift) = per_drv
        .iter()
        .find(|o| matches!(o, DeterminismOutcome::ObjectDriftOnly { .. }))
    {
        return drift.clone();
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

// ── Crate-metadata comparison (pure) ────────────────────────────────
// The darwin reproducibility gate. `nix-store --realise --check` proves
// WHOLE-OUTPUT byte identity, which darwin Rust builds essentially never
// achieve: rustc's object code absorbs per-build entropy (Mach-O
// `LC_UUID`, an ad-hoc code-signature recompute) that is HARMLESS,
// because a consumer resolves a dependency by its crate metadata/SVH and
// never by its object code. Gating on whole-output identity therefore
// withholds every darwin Rust crate and leaves the cache silently empty;
// gating on METADATA identity is the actual safety property.
//
// Where the metadata physically lives (both verified empirically on
// aarch64-darwin against resident store paths):
//   * `.rlib`  — ar-archive member `lib.rmeta`, itself a Mach-O object
//                whose payload is section `.rmeta` (segment `__DWARF`).
//                That member carries NO `LC_UUID`.
//   * `.dylib` — proc-macro crates: Mach-O section `.rustc` (segment
//                `__DATA`). These DO carry `LC_UUID` + a code signature,
//                so whole-file comparison is unsafe here — section-scoped
//                extraction is mandatory, not an optimization.

/// Extract a Rust crate's metadata bytes from a compiled artifact.
///
/// Handles both shapes: an `.rlib` (ar archive → `lib.rmeta` member →
/// its `.rmeta` section) and a proc-macro `.dylib` (`.rustc` section).
/// Returns `None` when `bytes` is neither — the caller treats that
/// conservatively (it never means "equal").
#[must_use]
pub fn extract_crate_metadata(bytes: &[u8]) -> Option<Vec<u8>> {
    use object::{Object, ObjectSection};

    // .rlib — an ar archive carrying a `lib.rmeta` member.
    if let Ok(archive) = object::read::archive::ArchiveFile::parse(bytes) {
        for member in archive.members().flatten() {
            if member.name() != b"lib.rmeta" {
                continue;
            }
            let data = member.data(bytes).ok()?;
            // The member is itself a Mach-O object; the payload is its
            // `.rmeta` section. Fall back to the whole member when it
            // isn't (that member carries no LC_UUID, so it is stable).
            if let Ok(obj) = object::File::parse(data) {
                if let Some(sec) = obj.section_by_name(".rmeta") {
                    if let Ok(d) = sec.data() {
                        return Some(d.to_vec());
                    }
                }
            }
            return Some(data.to_vec());
        }
    }

    // proc-macro .dylib — metadata is the `.rustc` section. NEVER compare
    // the whole file: LC_UUID + the ad-hoc signature drift every build.
    let obj = object::File::parse(bytes).ok()?;
    let sec = obj.section_by_name(".rustc")?;
    sec.data().ok().map(<[u8]>::to_vec)
}

/// Whether a path names a compiled Rust artifact this gate can compare.
#[must_use]
pub fn is_rust_artifact(name: &str) -> bool {
    name.ends_with(".rlib") || name.ends_with(".dylib") || name.ends_with(".so")
}

/// Verdict of comparing the crate metadata of two builds of one output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataVerdict {
    /// Every comparable artifact carried byte-identical metadata, and at
    /// least one artifact was actually compared.
    Identical,
    /// At least one artifact's metadata differed — real poison.
    Differs,
    /// Nothing could be compared (no Rust artifacts, an unreadable or
    /// unparseable file, or a file present on one side only). NEVER
    /// treated as safe.
    Unknown,
}

/// Pure fold over per-artifact `(original, rebuilt)` metadata pairs.
///
/// Conservative by construction: a single `Differs` loses to nothing, and
/// an empty or any-`None` comparison yields [`MetadataVerdict::Unknown`]
/// rather than `Identical`. "Unknown ⇒ withhold" is the same precedence
/// [`classify_determinism`] already applies to nix's own signals.
#[must_use]
pub fn fold_metadata_comparison(pairs: &[(Option<Vec<u8>>, Option<Vec<u8>>)]) -> MetadataVerdict {
    if pairs.is_empty() {
        return MetadataVerdict::Unknown;
    }
    let mut compared = 0usize;
    for (a, b) in pairs {
        match (a, b) {
            (Some(a), Some(b)) => {
                if a != b {
                    return MetadataVerdict::Differs;
                }
                compared += 1;
            }
            // One side missing/unparseable: we cannot prove equality.
            _ => return MetadataVerdict::Unknown,
        }
    }
    if compared == 0 {
        MetadataVerdict::Unknown
    } else {
        MetadataVerdict::Identical
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
/// `attic login` for an attic target; a NO-OP for any backend that has
/// no login handshake.
///
/// The guard is the point: this is called unconditionally for every
/// configured target, so without it a sui destination would shell out to
/// `attic login <empty-alias> <url> <empty-token>` on every cycle and be
/// filtered out of the push list when that failed — a cache that looks
/// configured and silently never receives anything.
pub fn attic_login_target(target: &CacheTarget) -> Result<()> {
    if !target.backend.needs_attic_login() {
        return Ok(());
    }
    attic_login_target_inner(target)
}

fn attic_login_target_inner(target: &CacheTarget) -> Result<()> {
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
    /// Push one store path to one destination, dispatching on the
    /// target's [`CacheBackend`]. Result-returning so a per-cache
    /// failure is assertable and the fan-out in [`push_path_to_caches`]
    /// can `continue` past it without aborting the rest.
    ///
    /// Takes the whole `target` rather than a cache NAME (the shape
    /// before attic's retirement): a name is an attic concept, and a
    /// sui destination is addressed by URL alone, so a name-only
    /// signature could not express one.
    fn push(&self, target: &CacheTarget, path: &str) -> Result<()>;
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
                    // Refine BEFORE the fold: a "differs" verdict that is
                    // only object drift is safe to push, and the
                    // aggregator's precedence must see the refined value.
                    per_drv.push(refine_object_drift(classify_determinism(
                        o.status.success(),
                        &stderr,
                    )));
                }
                Err(e) => per_drv.push(DeterminismOutcome::Inconclusive {
                    stderr_tail: tail_trimmed(&e.to_string(), 200),
                }),
            }
        }

        // 4. All-or-none fold.
        aggregate_closure_determinism(&per_drv)
    }

    fn push(&self, target: &CacheTarget, path: &str) -> Result<()> {
        match target.backend {
            CacheBackend::Attic => {
                let cache = &target.cache_name;
                let status = Command::new("attic")
                    .args(["push", cache, path])
                    .status()
                    .with_context(|| format!("running attic push {cache} {path}"))?;
                if !status.success() {
                    anyhow::bail!("attic push {cache} {path} exited {status}");
                }
                Ok(())
            }
            CacheBackend::Sui => nix_copy_to(&target.server_url, path),
        }
    }
}

/// `nix copy --to <url> <path>` — the sui-cache push.
///
/// sui-cache is a plain HTTP binary cache, so nix's own copier is the
/// whole client: it PUTs the narinfo + NAR the server already serves on
/// GET. No `attic`-style binary, no login handshake, no token — the
/// server is deliberately fail-open, and what keeps poison out is the
/// reproducibility gate upstream of this call, never the transport.
fn nix_copy_to(url: &str, path: &str) -> Result<()> {
    let status = Command::new("nix")
        .args(["copy", "--to", url, path])
        .status()
        .with_context(|| format!("running nix copy --to {url} {path}"))?;
    if !status.success() {
        anyhow::bail!("nix copy --to {url} {path} exited {status}");
    }
    Ok(())
}

/// Relative paths of every compiled Rust artifact under `root`.
///
/// Sorted so the comparison order is stable and a diff in the audit log
/// is reproducible. Unreadable directories are skipped, which can only
/// SHRINK the compared set — and a shrunk set yields
/// [`MetadataVerdict::Unknown`], never a false `Identical`.
fn rust_artifacts_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(is_rust_artifact)
            {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out
}

/// Downgrade a whole-output `NonReproducible` to
/// [`DeterminismOutcome::ObjectDriftOnly`] when — and ONLY when — every
/// Rust artifact's crate metadata is byte-identical between the resident
/// output and the rebuild `nix-store --realise --check` leaves beside it
/// at `<path>.check`.
///
/// Every failure mode returns the ORIGINAL `NonReproducible` unchanged:
/// nix named no path, `<path>.check` is absent, a file is unreadable or
/// unparseable, the two sides disagree on which artifacts exist, or there
/// was nothing comparable at all. Unknown ⇒ withhold — the same
/// precedence [`classify_determinism`] applies to nix's own signals, and
/// the reason this refinement can only ever make the gate MORE
/// permissive about harmless object drift, never about real metadata
/// divergence.
fn refine_object_drift(outcome: DeterminismOutcome) -> DeterminismOutcome {
    let DeterminismOutcome::NonReproducible { path: Some(orig) } = &outcome else {
        return outcome;
    };
    let orig_root = std::path::Path::new(orig);
    let check_root = std::path::PathBuf::from(format!("{orig}.check"));
    if !check_root.exists() {
        return outcome;
    }

    let artifacts = rust_artifacts_under(orig_root);
    // A differing output with no Rust artifacts in it is outside this
    // gate's competence — leave the conservative verdict alone.
    if artifacts.is_empty() {
        return outcome;
    }

    let pairs: Vec<(Option<Vec<u8>>, Option<Vec<u8>>)> = artifacts
        .iter()
        .map(|rel| {
            let a = std::fs::read(orig_root.join(rel))
                .ok()
                .and_then(|b| extract_crate_metadata(&b));
            let b = std::fs::read(check_root.join(rel))
                .ok()
                .and_then(|b| extract_crate_metadata(&b));
            (a, b)
        })
        .collect();

    match fold_metadata_comparison(&pairs) {
        MetadataVerdict::Identical => DeterminismOutcome::ObjectDriftOnly {
            path: orig.clone(),
        },
        MetadataVerdict::Differs | MetadataVerdict::Unknown => outcome,
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
            d.claim(t.dedup_key(), path)
        };
        if !fresh {
            continue;
        }
        match env.push(t, path) {
            Ok(()) => ok += 1,
            Err(e) => eprintln!("[prebuild] push {}→{} {e:#}", t.dedup_key(), path),
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
            backend: CacheBackend::Attic,
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

    // ── Sui backend (the post-attic fill path) ───────────────────────

    fn sui_target(url: &str) -> CacheTarget {
        CacheTarget {
            backend: CacheBackend::Sui,
            cache_name: String::new(),
            server_name: String::new(),
            server_url: url.into(),
            token_file: String::new(),
            enabled: true,
        }
    }

    #[test]
    fn a_sui_target_is_usable_on_url_alone() {
        // The trap the attic-shaped quartet check would spring: a sui
        // destination has no cache name, no alias and no token, so the
        // old is_usable() would drop it silently and the cache would
        // look configured while receiving nothing.
        let t = sui_target("http://127.0.0.1:5000");
        assert!(t.is_usable(), "sui target needs only a URL");

        let no_url = CacheTarget { server_url: String::new(), ..t.clone() };
        assert!(!no_url.is_usable(), "a sui target without a URL is unusable");

        let disabled = CacheTarget { enabled: false, ..t };
        assert!(!disabled.is_usable(), "disabled beats backend");
    }

    #[test]
    fn sui_targets_dedup_on_url_not_on_empty_cache_name() {
        // Both sui targets have cache_name == "", so keying the per-cycle
        // dedup on cache_name would collide them and silently drop every
        // push after the first.
        let a = sui_target("http://127.0.0.1:5000");
        let b = sui_target("http://sui.camelot-build.svc");
        assert_ne!(a.dedup_key(), b.dedup_key());
        assert_eq!(a.dedup_key(), "http://127.0.0.1:5000");
        // Attic still keys on its cache name.
        let atticish = CacheTarget {
            backend: CacheBackend::Attic,
            cache_name: "nexus".into(),
            server_name: "nexus".into(),
            server_url: "http://rio:8080/".into(),
            token_file: "/t".into(),
            enabled: true,
        };
        assert_eq!(atticish.dedup_key(), "nexus");
    }

    #[test]
    fn only_attic_needs_a_login_handshake() {
        assert!(CacheBackend::Attic.needs_attic_login());
        assert!(!CacheBackend::Sui.needs_attic_login());
        // The guard must make the login call a no-op for sui — without
        // it, `attic login` runs against a sui URL every cycle, fails,
        // and the target is filtered out of the push list.
        assert!(attic_login_target(&sui_target("http://127.0.0.1:5000")).is_ok());
    }

    #[test]
    fn wire_format_defaults_to_attic_so_old_configs_still_parse() {
        // A pre-existing rendered config carries no `backend` key.
        let json = r#"[{"name":"nexus","server":"nexus","url":"http://rio:8080/","token_file":"/t"}]"#;
        let parsed = parse_caches_json(json).expect("legacy config must still parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].backend, CacheBackend::Attic);
        assert!(parsed[0].is_usable());
    }

    #[test]
    fn wire_format_accepts_a_url_only_sui_target() {
        let json = r#"[{"backend":"sui","url":"http://sui.camelot-build.svc"}]"#;
        let parsed = parse_caches_json(json).expect("sui target must parse");
        assert_eq!(parsed[0].backend, CacheBackend::Sui);
        assert_eq!(parsed[0].server_url, "http://sui.camelot-build.svc");
        assert!(parsed[0].is_usable(), "no name/alias/token required");
    }

    // ── Metadata gate (the darwin reproducibility refinement) ────────
    // Every one of these asserts the gate REJECTS something. A gate only
    // ever observed passing may be checking nothing.

    #[test]
    fn metadata_fold_rejects_differing_metadata() {
        // THE fail-once proof: one differing artifact ⇒ Differs, which
        // keeps the conservative NonReproducible verdict and withholds.
        let pairs = vec![
            (Some(b"aaaa".to_vec()), Some(b"aaaa".to_vec())),
            (Some(b"aaaa".to_vec()), Some(b"bbbb".to_vec())),
        ];
        assert_eq!(fold_metadata_comparison(&pairs), MetadataVerdict::Differs);
    }

    #[test]
    fn metadata_fold_accepts_only_when_all_identical() {
        let pairs = vec![
            (Some(b"aaaa".to_vec()), Some(b"aaaa".to_vec())),
            (Some(b"cccc".to_vec()), Some(b"cccc".to_vec())),
        ];
        assert_eq!(fold_metadata_comparison(&pairs), MetadataVerdict::Identical);
    }

    #[test]
    fn metadata_fold_withholds_on_unknown_rather_than_passing() {
        // Empty (nothing comparable) and one-side-missing (unreadable or
        // unparseable) must BOTH be Unknown — never Identical. This is
        // the "unknown ⇒ withhold" precedence.
        assert_eq!(fold_metadata_comparison(&[]), MetadataVerdict::Unknown);
        assert_eq!(
            fold_metadata_comparison(&[(Some(b"aaaa".to_vec()), None)]),
            MetadataVerdict::Unknown
        );
        assert_eq!(
            fold_metadata_comparison(&[(None, Some(b"aaaa".to_vec()))]),
            MetadataVerdict::Unknown
        );
        assert_eq!(
            fold_metadata_comparison(&[(None, None)]),
            MetadataVerdict::Unknown
        );
    }

    #[test]
    fn garbage_bytes_yield_no_metadata() {
        assert!(extract_crate_metadata(b"not an object file at all").is_none());
        assert!(extract_crate_metadata(&[]).is_none());
    }

    #[test]
    fn only_compiled_rust_artifacts_are_compared() {
        assert!(is_rust_artifact("libfoo-abc123.rlib"));
        assert!(is_rust_artifact("libfoo-abc123.dylib"));
        assert!(!is_rust_artifact("README.md"));
        assert!(!is_rust_artifact("foo.rs"));
    }

    #[test]
    fn object_drift_is_pushable_but_never_reported_as_byte_identical() {
        // Present alongside a byte-identical result, the WEAKER true
        // claim must win — the closure is metadata-identical, not
        // byte-identical, and the audit trail has to say so.
        assert_eq!(
            aggregate_closure_determinism(&[
                DeterminismOutcome::Reproducible,
                DeterminismOutcome::ObjectDriftOnly { path: "/nix/store/x".into() },
            ]),
            DeterminismOutcome::ObjectDriftOnly { path: "/nix/store/x".into() }
        );
    }

    #[test]
    fn real_non_reproducibility_still_beats_object_drift() {
        // The refinement must never be able to mask real poison.
        assert_eq!(
            aggregate_closure_determinism(&[
                DeterminismOutcome::ObjectDriftOnly { path: "/nix/store/x".into() },
                DeterminismOutcome::NonReproducible { path: Some("/nix/store/y".into()) },
            ]),
            DeterminismOutcome::NonReproducible { path: Some("/nix/store/y".into()) }
        );
        // ...and an unknown failure still withholds too.
        assert_eq!(
            aggregate_closure_determinism(&[
                DeterminismOutcome::ObjectDriftOnly { path: "/nix/store/x".into() },
                DeterminismOutcome::Inconclusive { stderr_tail: "boom".into() },
            ]),
            DeterminismOutcome::Inconclusive { stderr_tail: "boom".into() }
        );
    }

    #[test]
    fn refinement_leaves_non_differs_outcomes_untouched() {
        // Only a NonReproducible-with-a-named-path is refinable; a
        // path-less one has nothing to compare and must stay as-is.
        assert_eq!(
            refine_object_drift(DeterminismOutcome::NonReproducible { path: None }),
            DeterminismOutcome::NonReproducible { path: None }
        );
        assert_eq!(
            refine_object_drift(DeterminismOutcome::Reproducible),
            DeterminismOutcome::Reproducible
        );
    }

    /// Empirical proof against REAL compiled artifacts, not synthetic
    /// bytes: the parser must actually read an aarch64-darwin `.rlib`
    /// (ar → `lib.rmeta` → `.rmeta` section) and a proc-macro `.dylib`
    /// (`.rustc` section), AND must detect a single flipped metadata
    /// byte.
    ///
    /// `#[ignore]` because it needs a populated `/nix/store` — DELIBERATELY
    /// opt-in rather than silently skipped, so it can never masquerade as
    /// coverage it isn't providing. Re-verify with:
    ///   `cargo test --bin tend -- --ignored metadata_gate_on_real`
    #[test]
    #[ignore = "needs a populated /nix/store; run explicitly with --ignored"]
    fn metadata_gate_on_real_store_artifacts() {
        let mut checked = 0usize;
        let entries = std::fs::read_dir("/nix/store").expect("read /nix/store");
        for dir in entries.flatten().take(4000) {
            let lib = dir.path().join("lib");
            let Ok(files) = std::fs::read_dir(&lib) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !is_rust_artifact(name) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&p) else { continue };
                let Some(meta) = extract_crate_metadata(&bytes) else {
                    continue;
                };
                assert!(
                    !meta.is_empty(),
                    "extracted empty metadata from real artifact {}",
                    p.display()
                );

                // THE FAIL-ONCE PROOF, on real bytes: flip one byte of
                // the extracted metadata and confirm the fold rejects it.
                let mut poisoned = meta.clone();
                poisoned[0] ^= 0xff;
                assert_eq!(
                    fold_metadata_comparison(&[(Some(meta.clone()), Some(poisoned))]),
                    MetadataVerdict::Differs,
                    "gate FAILED to detect flipped metadata in {}",
                    p.display()
                );
                // Identical bytes must still pass, so the gate is not
                // simply rejecting everything.
                assert_eq!(
                    fold_metadata_comparison(&[(Some(meta.clone()), Some(meta))]),
                    MetadataVerdict::Identical
                );
                checked += 1;
                if checked >= 5 {
                    return;
                }
            }
        }
        assert!(
            checked > 0,
            "no real Rust artifacts found under /nix/store — this test proved NOTHING"
        );
    }

    #[test]
    fn refinement_withholds_when_check_path_is_absent() {
        // nix names a path but left no `<path>.check` beside it ⇒ nothing
        // to compare ⇒ the conservative verdict survives.
        let outcome = DeterminismOutcome::NonReproducible {
            path: Some("/nix/store/definitely-does-not-exist-zzz".into()),
        };
        assert_eq!(refine_object_drift(outcome.clone()), outcome);
    }
}
