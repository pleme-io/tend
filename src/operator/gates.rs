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
use std::path::Path;
use std::time::Instant;
use tokio::process::Command;

use super::crds::GateResult;

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
    let attr = gate_name.strip_prefix("nix-build:").or_else(|| gate_name.strip_prefix("nix-eval:"))?;
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

/// Run a single named gate against the given repo working tree.
/// Returns a `GateResult` even on failure — only catastrophic errors
/// (e.g. cwd doesn't exist) bubble as `Err`.
pub async fn run_gate(gate_name: &str, repo_dir: &Path) -> Result<GateResult> {
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
        dispatch_nix_build(attr, repo_dir).await
    } else if let Some(attr) = gate_name.strip_prefix("nix-eval:") {
        dispatch_nix_eval(attr, repo_dir).await
    } else {
        match gate_name {
            "nix-flake-check" => dispatch_nix_flake_check(repo_dir).await,
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

/// Run every gate in `gates` against `repo_dir`. Returns the full
/// list of results in the same order. Caller decides what to do
/// with partial failures (typically: any failed → proposal fails).
pub async fn run_all(gates: &[String], repo_dir: &Path) -> Result<Vec<GateResult>> {
    let mut out = Vec::with_capacity(gates.len());
    for g in gates {
        out.push(run_gate(g, repo_dir).await?);
    }
    Ok(out)
}

struct GateOutcome {
    passed: bool,
    log_tail: String,
}

// ─── Dispatchers ────────────────────────────────────────────────────

async fn dispatch_nix_build(attr: &str, repo_dir: &Path) -> Result<GateOutcome> {
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
            .current_dir(repo_dir),
    ).await
}

async fn dispatch_nix_eval(attr: &str, repo_dir: &Path) -> Result<GateOutcome> {
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
            .current_dir(repo_dir),
    ).await
}

async fn dispatch_nix_flake_check(repo_dir: &Path) -> Result<GateOutcome> {
    let mut cmd = Command::new("nix");
    nix_env(&mut cmd);
    capture(
        cmd.arg("flake")
            .arg("check")
            .arg("--no-build")
            .current_dir(repo_dir),
    ).await
}

/// Apply the env vars nix needs to operate from a no-/etc/passwd
/// distroless pod. Inheriting the parent process env *should* be
/// sufficient — but we explicitly set the load-bearing ones because
/// the nix C++ binary aborts hard ("cannot determine user's home
/// directory") without HOME, and inheritance failures are silent.
/// Setting them on the subprocess Command is idempotent with the
/// pod-level env, costs nothing, and makes the contract explicit.
fn nix_env(cmd: &mut Command) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tend".to_string());
    let cache = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{home}/.cache"));
    cmd.env("HOME", &home)
        .env("XDG_CACHE_HOME", &cache)
        .env("USER", "tend")
        .env("LOGNAME", "tend")
        // Guarantees nix-command + flakes are enabled regardless of
        // ambient state — the gates rely on `nix flake check` and
        // `nix build` syntax.
        .env("NIX_CONFIG", "experimental-features = nix-command flakes");
}

async fn dispatch_forge_ci(repo_dir: &Path) -> Result<GateOutcome> {
    capture(Command::new("forge").arg("ci").current_dir(repo_dir)).await
}

async fn dispatch_cargo_test(repo_dir: &Path) -> Result<GateOutcome> {
    capture(Command::new("cargo").arg("test").arg("--release").current_dir(repo_dir)).await
}

async fn dispatch_cargo_build(repo_dir: &Path) -> Result<GateOutcome> {
    capture(Command::new("cargo").arg("build").arg("--release").current_dir(repo_dir)).await
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
    use super::*;

    #[tokio::test]
    async fn unknown_gate_returns_failed_result() {
        let dir = std::env::temp_dir();
        let r = run_gate("totally-fake-gate", &dir).await.unwrap();
        assert!(!r.passed);
        assert!(r.log_excerpt.unwrap().contains("unknown gate"));
    }

    #[tokio::test]
    async fn missing_repo_dir_returns_failed_result() {
        let r = run_gate("nix-flake-check", Path::new("/this/does/not/exist")).await.unwrap();
        assert!(!r.passed);
        assert!(r.log_excerpt.unwrap().contains("does not exist"));
    }

    #[test]
    fn target_system_classifies_darwin_configurations() {
        assert_eq!(target_system("nix-build:darwinConfigurations.cid.system"), Some("darwin"));
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
        let r = run_gate(other_family_gate, &dir).await.unwrap();
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
