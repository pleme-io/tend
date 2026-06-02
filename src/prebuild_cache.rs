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
//!    poisons the cache for the whole fleet. [`ReproPolicy`] lets the
//!    fill loop *verify determinism before pushing* — build-and-compare
//!    via `nix build --rebuild` — so the cache-filler becomes the thing
//!    that keeps the cache CONSISTENT rather than the thing that breaks
//!    it. Off by default (it doubles build cost); on for any cache the
//!    fleet trusts for substitution.

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
    /// Before pushing, `nix build --rebuild` the closure; push only if
    /// the rebuild is byte-identical. Doubles build cost; the price of a
    /// cache the fleet can safely substitute from.
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

/// Verify a built installable is reproducible before trusting it for the
/// cache: `nix build <installable> --rebuild` rebuilds and compares to
/// the resident output, failing with "may not be deterministic" on a
/// mismatch. Returns true iff the rebuild matched (safe to push). This
/// is the anti-poison gate from the 2026-06-02 incident — a
/// non-reproducible artifact never reaches a substitution-source cache.
pub fn verify_deterministic(repo: &std::path::Path, installable: &str) -> bool {
    Command::new("nix")
        .args([
            "build", installable,
            "--rebuild", "--no-link",
            "--option", "warn-dirty", "false",
        ])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fan one store path out to every usable cache target, deduping via the
/// shared per-cycle [`ClosureDedup`] so a closure shared across repos is
/// pushed to each cache at most once. `attic push` itself skips paths the
/// server already holds (get-missing-paths), so this layer's job is to
/// avoid the redundant *invocation*, not to re-implement membership.
///
/// Returns the count of `(cache, path)` pushes that succeeded this call.
/// A failed push to one cache never aborts the others — one stuck cache
/// must not strand the rest of the fan-out.
pub fn push_path_to_caches(
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
        match Command::new("attic")
            .args(["push", &t.cache_name, path])
            .status()
        {
            Ok(s) if s.success() => ok += 1,
            Ok(s) => eprintln!("[prebuild] attic push {}→{} exit {}", t.cache_name, path, s),
            Err(e) => eprintln!("[prebuild] attic push {}→{} {}", t.cache_name, path, e),
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
}
