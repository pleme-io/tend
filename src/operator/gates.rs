//! Verification gates — string identifier → dispatcher fn.
//!
//! Gate names live as `Vec<String>` on `FlakeUpdatePolicySpec.gates`.
//! Unknown gate names surface as a status condition rather than
//! failing API admission, which lets the operator deploy new
//! dispatchers without pre-coordinating CRD bumps.
//!
//! Dispatchers shell out via `tokio::process` against the working
//! tree of the proposal's repo. The repo is expected to already be
//! cloned by tend's workspace layer at
//! `<workspace.base_dir>/<repo>` — the operator does not clone.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::process::Command;

use super::crds::{FlakeRev, GateResult};
use super::failure_set::FailureSet;

const LOG_TAIL_BYTES: usize = 2048;

/// The platform the operator pod is currently running on.
/// Resolved at compile time from the build target — runtime
/// `uname` would also work but compile-time matches the binary
/// the operator actually built `nix build` for.
#[must_use]
pub const fn current_system() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-linux"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-linux"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-darwin"
    } else {
        "unknown"
    }
}

/// Best-effort target-system classification for a gate. Returns
/// `Some(system)` when we can confidently say the gate produces
/// outputs for a specific Nix system (so we know whether to run it
/// here or skip / delegate); `None` for system-agnostic gates
/// like `nix-flake-check` or `cargo-test` that work anywhere.
#[must_use]
pub fn target_system(gate_name: &str) -> Option<&'static str> {
    let attr = gate_name
        .strip_prefix("nix-build:")
        .or_else(|| gate_name.strip_prefix("nix-eval:"))?;
    if attr.starts_with("darwinConfigurations.") {
        // nix-darwin systems pin a darwin platform; we can't infer
        // x86_64 vs aarch64 from the attr name alone, so report
        // generic "darwin" — the matcher rejects any non-darwin host.
        Some("darwin")
    } else if attr.starts_with("nixosConfigurations.") {
        Some("linux")
    } else if let Some(rest) = attr.strip_prefix("homeConfigurations.") {
        // homeConfigurations names sometimes encode the system
        // (e.g. "drzzln@aarch64-darwin"). Falls back to None when
        // the convention isn't followed — caller treats unknown as
        // "try here, fail visibly" which is better than over-skipping.
        rest.split('.').next()?.split('@').nth(1).map(classify)
    } else {
        None
    }
}

/// Map an arbitrary system token to a coarse family for matching.
/// `target_system` returns these strings; `current_system_family`
/// reduces the running platform to the same shape for comparison.
fn classify(system: &str) -> &'static str {
    if system.contains("darwin") {
        "darwin"
    } else if system.contains("linux") {
        "linux"
    } else {
        "unknown"
    }
}

#[must_use]
fn current_system_family() -> &'static str {
    classify(current_system())
}

/// What scenario the gate runs against. `Baseline` uses the repo's
/// current lockfile state. `FlakeOverride` injects a `--override-input
/// <input> <flake_ref>` so the gate evaluates the proposed change
/// without modifying flake.lock — a clean way to ask "what would
/// happen if we landed this pin?" against a real working tree.
///
/// Non-nix dispatchers ignore the override (they have no equivalent
/// mechanism today). When Phase 2-4 controllers add their own
/// override modes, this enum grows variants.
#[derive(Debug, Clone)]
pub enum GateContext {
    Baseline,
    FlakeOverride { input: String, flake_ref: String },
}

impl GateContext {
    /// Args to inject before the subcommand for nix gates. Returns an
    /// empty vec for Baseline or non-flake contexts.
    fn nix_override_args(&self) -> Vec<String> {
        match self {
            GateContext::Baseline => Vec::new(),
            GateContext::FlakeOverride { input, flake_ref } => {
                vec!["--override-input".into(), input.clone(), flake_ref.clone()]
            }
        }
    }
}

/// Format the `nix --override-input <name> <flake_ref>` value from a
/// proposed `FlakeRev`. URL is `github:owner/repo` and rev is the
/// new SHA — concatenated with `/` per the nix flake-ref grammar.
#[must_use]
pub fn override_flake_ref(rev: &FlakeRev) -> String {
    format!("{}/{}", rev.url, rev.rev)
}

/// Run a single named gate against the given repo working tree.
/// Returns a `GateResult` even on failure — only catastrophic errors
/// (e.g. cwd doesn't exist) bubble as `Err`.
pub async fn run_gate(gate_name: &str, repo_dir: &Path, ctx: &GateContext) -> Result<GateResult> {
    if !repo_dir.is_dir() {
        return Ok(GateResult {
            name: gate_name.to_string(),
            passed: false,
            skipped: false,
            duration_ms: 0,
            log_excerpt: Some(format!("repo dir does not exist: {}", repo_dir.display())),
        });
    }

    // Platform-aware skip: gates targeting a different OS family
    // than the operator's runtime can't possibly succeed locally.
    // Mark Skipped (not Failed) so the proposal isn't blocked —
    // a future RemoteBuilder integration will replace skipping
    // with delegation to a same-platform builder.
    if let Some(target) = target_system(gate_name) {
        let here = current_system_family();
        if classify(target) != here {
            return Ok(GateResult {
                name: gate_name.to_string(),
                passed: false,
                skipped: true,
                duration_ms: 0,
                log_excerpt: Some(format!(
                    "skipped: target_system={target}, operator_system={current} ({here}); \
                     remote-builder delegation not wired yet",
                    current = current_system(),
                )),
            });
        }
    }

    let start = Instant::now();
    let result: Result<GateOutcome> = if let Some(attr) = gate_name.strip_prefix("nix-build:") {
        dispatch_nix_build(attr, repo_dir, ctx).await
    } else if let Some(attr) = gate_name.strip_prefix("nix-eval:") {
        dispatch_nix_eval(attr, repo_dir, ctx).await
    } else {
        match gate_name {
            "nix-flake-check" => dispatch_nix_flake_check(repo_dir, ctx).await,
            "forge-ci" => dispatch_forge_ci(repo_dir).await,
            "cargo-test" => dispatch_cargo_test(repo_dir).await,
            "cargo-build" => dispatch_cargo_build(repo_dir).await,
            other => Ok(GateOutcome {
                passed: false,
                log_tail: format!("unknown gate: `{other}` — no dispatcher registered"),
            }),
        }
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    let outcome = result.unwrap_or_else(|e| GateOutcome {
        passed: false,
        log_tail: format!("dispatcher error: {e}"),
    });

    Ok(GateResult {
        name: gate_name.to_string(),
        passed: outcome.passed,
        skipped: false,
        duration_ms,
        log_excerpt: Some(outcome.log_tail),
    })
}

/// Run every gate in `gates` against `repo_dir` with one context.
/// Returns the full list of results in the same order. Caller decides
/// what to do with partial failures (typically: any failed → proposal
/// fails).
pub async fn run_all(
    gates: &[String],
    repo_dir: &Path,
    ctx: &GateContext,
) -> Result<Vec<GateResult>> {
    let mut out = Vec::with_capacity(gates.len());
    for g in gates {
        out.push(run_gate(g, repo_dir, ctx).await?);
    }
    Ok(out)
}

/// Differential gate execution: a gate "passes" if the proposed pin
/// doesn't introduce any *new* failures vs the baseline.
///
/// For each gate we run twice — once at baseline, once with the
/// proposed flake override — and compare failure signatures. If the
/// proposed failures are a subset of baseline failures, the gate
/// passes (the bump didn't make things worse). If the proposed
/// introduces a failure not in baseline, the gate fails and the new
/// failures are surfaced in the log excerpt.
///
/// Skipped gates (platform mismatch) skip cleanly without running
/// either pass — saves ~2× time on darwin gates from a linux pod.
pub async fn run_all_differential(
    gates: &[String],
    repo_dir: &Path,
    override_ctx: &GateContext,
) -> Result<Vec<GateResult>> {
    let mut out = Vec::with_capacity(gates.len());
    for name in gates {
        out.push(run_gate_differential(name, repo_dir, override_ctx).await?);
    }
    Ok(out)
}

/// Single-gate differential. See `run_all_differential` for semantics.
/// Did the proposed pin fail a gate the baseline passed, WITHOUT leaving a
/// comparable failure signature?
///
/// Pure, so the rule is provable without running a gate — and the rule is
/// the whole point: `FailureSet::extract` only recognises lines beginning
/// `error:` and friends, so a silent-but-nonzero failure (killed process,
/// OOM, diagnostics on stdout, an unrecognised error prefix) extracts to
/// the EMPTY set, which is a subset of everything. The signature
/// comparison then reports `passed: true` and the pin is pushed.
///
/// Dampening pre-existing noise is what the signature diff is for. It is
/// not a substitute for asking whether the run succeeded.
#[must_use]
pub fn is_silent_regression(
    proposed_passed: bool,
    baseline_passed: bool,
    proposed_fails: &FailureSet,
) -> bool {
    // The signature set is deliberately NOT consulted for the pass/fail
    // decision here — an extractable failure flows to the normal
    // subset comparison below, which can report it properly.
    let _ = proposed_fails;
    !proposed_passed && baseline_passed
}

pub async fn run_gate_differential(
    gate_name: &str,
    repo_dir: &Path,
    override_ctx: &GateContext,
) -> Result<GateResult> {
    // Step 1: the proposed run. If platform-mismatched it returns
    // skipped with no work — short-circuit baseline too.
    let proposed = run_gate(gate_name, repo_dir, override_ctx).await?;
    if proposed.skipped {
        return Ok(proposed);
    }
    if proposed.passed {
        // Trivially fine — the proposed change is clean by itself.
        return Ok(proposed);
    }

    // Step 2: baseline run, only when proposed failed (the only case
    // where a comparison matters). Saves a full gate run when the
    // common-case proposed-passes-cleanly path holds.
    let baseline = run_gate(gate_name, repo_dir, &GateContext::Baseline).await?;

    let baseline_fails = FailureSet::extract(baseline.log_excerpt.as_deref().unwrap_or(""));
    let proposed_fails = FailureSet::extract(proposed.log_excerpt.as_deref().unwrap_or(""));

    // ── ★ THE EXIT STATUS IS LOAD-BEARING, NOT DECORATIVE ────────────────
    // The verdict below is derived purely from comparing EXTRACTED failure
    // signatures, and `proposed.passed` — the child's real exit status —
    // was discarded once both runs completed. `FailureSet::extract` only
    // recognises lines starting with `error:` and friends, so a gate that
    // failed WITHOUT emitting a recognised line extracts to the empty set,
    // and the empty set is a subset of everything. The gate then reports
    // `passed: true` and the pin is pushed to main.
    //
    // Any failure mode that is silent-but-nonzero lands here: a killed
    // process, an OOM, a tool that writes its diagnostics to stdout, a new
    // nix error prefix. The signature comparison is a good way to DAMPEN
    // pre-existing noise; it is not a substitute for asking whether the
    // run succeeded.
    //
    // So: a proposed run that FAILED while the baseline SUCCEEDED is a
    // regression regardless of what could be extracted from its log.
    if is_silent_regression(proposed.passed, baseline.passed, &proposed_fails) {
        return Ok(GateResult {
            name: gate_name.to_string(),
            passed: false,
            skipped: false,
            duration_ms: proposed.duration_ms.saturating_add(baseline.duration_ms),
            log_excerpt: Some(format!(
                "differential FAIL: the proposed pin failed this gate while the baseline passed,                  and no comparable failure signature was extractable from its output                  ({} signature(s) seen). Treating a non-zero exit as a regression rather than                  inferring from the log.\nproposed_log_tail: {}",
                proposed_fails.len(),
                proposed.log_excerpt.as_deref().unwrap_or("")
                    .chars().rev().take(400).collect::<String>().chars().rev().collect::<String>()
            )),
        });
    }

    if proposed_fails.is_subset_of(&baseline_fails) {
        // Pre-existing failures only — the proposed pin didn't make
        // things worse. Mark passed and surface the dampened diff.
        let baseline_count = baseline_fails.len();
        Ok(GateResult {
            name: gate_name.to_string(),
            passed: true,
            skipped: false,
            duration_ms: proposed.duration_ms.saturating_add(baseline.duration_ms),
            log_excerpt: Some(format!(
                "differential pass: {baseline_count} pre-existing failures unchanged by proposed pin\n\
                 baseline_log_tail: {}",
                baseline.log_excerpt.as_deref().unwrap_or("")
                    .chars().rev().take(400).collect::<String>().chars().rev().collect::<String>()
            )),
        })
    } else {
        let new = proposed_fails.new_vs(&baseline_fails);
        Ok(GateResult {
            name: gate_name.to_string(),
            passed: false,
            skipped: false,
            duration_ms: proposed.duration_ms.saturating_add(baseline.duration_ms),
            log_excerpt: Some(format!(
                "proposed pin introduces {} new failure(s) not in baseline:\n  - {}\n\nproposed_log_tail: {}",
                new.len(),
                new.join("\n  - "),
                proposed.log_excerpt.as_deref().unwrap_or("")
            )),
        })
    }
}

struct GateOutcome {
    passed: bool,
    log_tail: String,
}

// ─── Dispatchers ────────────────────────────────────────────────────

async fn dispatch_nix_build(attr: &str, repo_dir: &Path, ctx: &GateContext) -> Result<GateOutcome> {
    if attr.is_empty() {
        return Ok(GateOutcome {
            passed: false,
            log_tail: "nix-build gate requires `:<attr>` suffix (e.g. nix-build:darwinConfigurations.cid.system)".into(),
        });
    }
    let target = format!(".#{attr}");
    let mut cmd = Command::new("nix");
    nix_env(&mut cmd);
    capture(
        cmd.arg("build")
            .arg(&target)
            .arg("--no-link")
            .arg("--print-build-logs")
            .args(ctx.nix_override_args())
            .current_dir(repo_dir),
    )
    .await
}

async fn dispatch_nix_eval(attr: &str, repo_dir: &Path, ctx: &GateContext) -> Result<GateOutcome> {
    if attr.is_empty() {
        return Ok(GateOutcome {
            passed: false,
            log_tail: "nix-eval gate requires `:<attr>` suffix".into(),
        });
    }
    let target = format!(".#{attr}");
    let mut cmd = Command::new("nix");
    nix_env(&mut cmd);
    capture(
        cmd.arg("eval")
            .arg(&target)
            .arg("--apply")
            .arg("_: null")
            .args(ctx.nix_override_args())
            .current_dir(repo_dir),
    )
    .await
}

async fn dispatch_nix_flake_check(repo_dir: &Path, ctx: &GateContext) -> Result<GateOutcome> {
    let mut cmd = Command::new("nix");
    nix_env(&mut cmd);
    capture(
        cmd.arg("flake")
            .arg("check")
            .arg("--no-build")
            .args(ctx.nix_override_args())
            .current_dir(repo_dir),
    )
    .await
}

/// Apply the env vars nix needs to operate from a distroless pod.
///
/// Five concerns rolled into one call so every nix invocation in the
/// operator is identical-by-construction:
///
///   1. **HOME / XDG_CACHE_HOME / USER / LOGNAME** — nix's C++ binary
///      aborts hard ("cannot determine user's home directory") without
///      a resolvable user. The image ships an /etc/passwd entry for
///      UID 1000 and we mount a writable home volume, but env
///      inheritance is silent-on-failure so we set them explicitly.
///
///   2. **experimental-features** — gates use `nix flake check`/`build`
///      syntax which still requires the experimental flag.
///
///   3. **accept-flake-config = true** — the pleme-io nix repo's flake
///      sets `nixConfig.extra-substituters` etc.; without this nix
///      refuses to honor them and the eval errors out.
///
///   4. **access-tokens = github.com=$GITHUB_TOKEN** — without auth,
///      private pleme-io flake inputs return HTTP 404 from the GitHub
///      tarball API and gate evaluation fails. The token comes from
///      the same secret the operator uses for `git push`.
///
///   5. **netrc-file** — git deps via https also need this token; nix's
///      git fetcher reads ~/.config/nix/nix.conf access-tokens for
///      tarball URLs and netrc for raw git URLs. Future-proofing.
pub fn nix_env(cmd: &mut Command) {
    let (home, cache) = nix_home_and_cache(|k| std::env::var_os(k));
    let mut nix_config = String::from(
        "experimental-features = nix-command flakes\n\
         accept-flake-config = true",
    );
    // NIX_CONFIG is process-scoped, so the token dies with the child.
    // Routed through `Secret` anyway: this is the one place tend still
    // interpolates a credential into a string, and typing it keeps it
    // visible to the `expose()` audit rather than looking like an
    // ordinary env var.
    if let Some(token) = crate::provider::github_token() {
        nix_config.push_str(&format!("\naccess-tokens = github.com={}", token.expose()));
    }
    cmd.env("HOME", &home)
        .env("XDG_CACHE_HOME", &cache)
        .env("USER", "tend")
        .env("LOGNAME", "tend")
        .env("NIX_CONFIG", nix_config);
}

/// Resolve the `HOME` / `XDG_CACHE_HOME` pair handed to a child `nix`.
///
/// **Both env arms are filtered for ABSOLUTE, not merely non-empty** — the
/// masked-branch class of `theory/MASKED-BRANCH.md`. The literal fallbacks
/// are the guarded arm and cannot be relative, which is precisely what made
/// the raw arms read as safe. `HOME=""` made nix abort ("cannot determine
/// user's home directory") *and* silently produced `XDG_CACHE_HOME=/.cache`;
/// a relative `HOME` or `XDG_CACHE_HOME` is worse than either, because these
/// commands run with `current_dir(repo_dir)` and nix would then write its
/// eval cache into the very repository being gated.
///
/// okiba is deliberately NOT used here: the fallback is the pod's literal
/// `/home/tend`, not `dirs::home_dir()`, and routing it through okiba would
/// relocate the cache of a valid distroless-pod configuration (where HOME is
/// unset by design and no getpwuid answer exists).
///
/// `lookup` is explicit so the invariant is testable without mutating
/// `std::env`, which races under parallel test execution.
fn nix_home_and_cache(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> (PathBuf, PathBuf) {
    let home = lookup("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/home/tend"));
    let cache = lookup("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".cache"));
    (home, cache)
}

async fn dispatch_forge_ci(repo_dir: &Path) -> Result<GateOutcome> {
    capture(Command::new("forge").arg("ci").current_dir(repo_dir)).await
}

async fn dispatch_cargo_test(repo_dir: &Path) -> Result<GateOutcome> {
    capture(
        Command::new("cargo")
            .arg("test")
            .arg("--release")
            .current_dir(repo_dir),
    )
    .await
}

async fn dispatch_cargo_build(repo_dir: &Path) -> Result<GateOutcome> {
    capture(
        Command::new("cargo")
            .arg("build")
            .arg("--release")
            .current_dir(repo_dir),
    )
    .await
}

async fn capture(cmd: &mut Command) -> Result<GateOutcome> {
    let output = cmd.output().await?;
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let log_tail = tail_utf8(&combined, LOG_TAIL_BYTES);
    Ok(GateOutcome {
        passed: output.status.success(),
        log_tail,
    })
}

fn tail_utf8(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max);
    // Snap to a UTF-8 boundary forward so we don't slice mid-codepoint.
    let mut start = start;
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

#[cfg(test)]
mod tests {

    /// ── ★ THE MASKED BRANCH (theory/MASKED-BRANCH.md) ───────────────────
    /// `nix_env`'s literal fallbacks (`/home/tend`, `<home>/.cache`) cannot
    /// be relative, so the function read as safe while both env arms took
    /// their variable verbatim. These gates run with
    /// `current_dir(repo_dir)`, so a relative `HOME` / `XDG_CACHE_HOME`
    /// pointed nix's eval cache at the repository under gate, and an empty
    /// `HOME` produced the nonsense `XDG_CACHE_HOME=/.cache` on the way to
    /// nix aborting outright.
    ///
    /// Red run: drop either `.filter(|p| p.is_absolute())` and the empty and
    /// relative rows below come back as `""` / `rel` and `/.cache` / `rel/…`.
    #[test]
    fn no_environment_value_yields_a_relative_nix_env() {
        for bogus in ["", "rel", "./rel", "..", "cache"] {
            let b = std::ffi::OsString::from(bogus);
            let (home, cache) = super::nix_home_and_cache(|_| Some(b.clone()));
            assert!(home.is_absolute(), "HOME={bogus:?} gave nix {home:?}");
            assert!(
                cache.is_absolute(),
                "XDG_CACHE_HOME={bogus:?} gave nix {cache:?}"
            );
            assert_eq!(home, std::path::PathBuf::from("/home/tend"));
            assert_eq!(cache, std::path::PathBuf::from("/home/tend/.cache"));
        }
    }

    /// A valid pod/workstation environment must resolve exactly where it did
    /// before — including the unset case the distroless image relies on.
    #[test]
    fn a_valid_environment_resolves_where_it_always_did() {
        let (home, cache) = super::nix_home_and_cache(|_| None);
        assert_eq!(home, std::path::PathBuf::from("/home/tend"));
        assert_eq!(cache, std::path::PathBuf::from("/home/tend/.cache"));

        let (home, cache) = super::nix_home_and_cache(|k| match k {
            "HOME" => Some("/home/u".into()),
            _ => None,
        });
        assert_eq!(home, std::path::PathBuf::from("/home/u"));
        assert_eq!(cache, std::path::PathBuf::from("/home/u/.cache"));

        let (home, cache) = super::nix_home_and_cache(|k| match k {
            "HOME" => Some("/home/u".into()),
            "XDG_CACHE_HOME" => Some("/fast/cache".into()),
            _ => None,
        });
        assert_eq!(home, std::path::PathBuf::from("/home/u"));
        assert_eq!(cache, std::path::PathBuf::from("/fast/cache"));
    }

    /// ── ★ A GATE THAT NEVER RAN MUST NOT REPORT PASSED ──────────────────
    /// The differential verdict compared extracted failure SIGNATURES and
    /// threw away the child's exit status. `FailureSet::extract` only
    /// recognises `error:`-style lines, so a failure that emits none —
    /// killed process, OOM, diagnostics on stdout, a new nix error prefix —
    /// extracts to the empty set. The empty set is a subset of every set,
    /// so the gate reported `passed: true` and the pin went to main.
    ///
    /// Red run: replace the body with `false` and the silent-failure case
    /// below returns "not a regression" for a gate that exited non-zero.
    #[test]
    fn a_silent_nonzero_failure_is_a_regression() {
        let empty = FailureSet::extract("");
        assert!(
            is_silent_regression(false, true, &empty),
            "proposed failed, baseline passed, nothing extractable — regression"
        );
    }

    #[test]
    fn a_baseline_that_also_failed_is_not_a_regression() {
        let empty = FailureSet::extract("");
        assert!(
            !is_silent_regression(false, false, &empty),
            "both failed — pre-existing, and the subset comparison handles it"
        );
    }

    #[test]
    fn a_passing_proposed_run_is_never_a_regression() {
        let empty = FailureSet::extract("");
        assert!(!is_silent_regression(true, true, &empty));
        assert!(!is_silent_regression(true, false, &empty));
    }

    use super::*;

    #[tokio::test]
    async fn unknown_gate_returns_failed_result() {
        let dir = std::env::temp_dir();
        let r = run_gate("totally-fake-gate", &dir, &GateContext::Baseline)
            .await
            .unwrap();
        assert!(!r.passed);
        assert!(r.log_excerpt.unwrap().contains("unknown gate"));
    }

    #[tokio::test]
    async fn missing_repo_dir_returns_failed_result() {
        let r = run_gate(
            "nix-flake-check",
            Path::new("/this/does/not/exist"),
            &GateContext::Baseline,
        )
        .await
        .unwrap();
        assert!(!r.passed);
        assert!(r.log_excerpt.unwrap().contains("does not exist"));
    }

    #[test]
    fn target_system_classifies_darwin_configurations() {
        assert_eq!(
            target_system("nix-build:darwinConfigurations.cid.system"),
            Some("darwin")
        );
    }

    #[test]
    fn target_system_classifies_nixos_configurations() {
        assert_eq!(
            target_system("nix-build:nixosConfigurations.rio.config.system.build.toplevel"),
            Some("linux")
        );
    }

    #[test]
    fn target_system_returns_none_for_agnostic_gates() {
        assert_eq!(target_system("nix-flake-check"), None);
        assert_eq!(target_system("cargo-test"), None);
        assert_eq!(target_system("forge-ci"), None);
    }

    #[test]
    fn classify_normalizes_full_system_strings() {
        assert_eq!(classify("aarch64-darwin"), "darwin");
        assert_eq!(classify("x86_64-darwin"), "darwin");
        assert_eq!(classify("aarch64-linux"), "linux");
        assert_eq!(classify("x86_64-linux"), "linux");
        assert_eq!(classify("solaris"), "unknown");
    }

    #[tokio::test]
    async fn cross_platform_gate_skips_when_runtime_mismatched() {
        // Run from any host; if we're on linux, the darwin gate skips.
        // If we're on darwin, the linux gate skips. The test infra runs
        // on whatever the developer's machine is — this assertion is
        // platform-agnostic by checking the *opposite* family from
        // the current runtime.
        let dir = std::env::temp_dir();
        let other_family_gate = if current_system_family() == "linux" {
            "nix-build:darwinConfigurations.foo.system"
        } else {
            "nix-build:nixosConfigurations.foo.config.system.build.toplevel"
        };
        let r = run_gate(other_family_gate, &dir, &GateContext::Baseline)
            .await
            .unwrap();
        assert!(r.skipped, "expected skipped, got {r:?}");
        assert!(!r.passed, "skipped gate must not also be passed: {r:?}");
        assert!(
            r.log_excerpt.unwrap().contains("skipped"),
            "log excerpt should mention skip"
        );
    }

    #[test]
    fn tail_utf8_handles_short_input() {
        let s = tail_utf8(b"hello", 100);
        assert_eq!(s, "hello");
    }

    #[test]
    fn tail_utf8_truncates_to_boundary() {
        let s = tail_utf8(b"abcdef", 3);
        assert_eq!(s, "def");
    }
}
