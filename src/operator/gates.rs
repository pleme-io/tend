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

/// Run a single named gate against the given repo working tree.
/// Returns a `GateResult` even on failure — only catastrophic errors
/// (e.g. cwd doesn't exist) bubble as `Err`.
pub async fn run_gate(gate_name: &str, repo_dir: &Path) -> Result<GateResult> {
    if !repo_dir.is_dir() {
        return Ok(GateResult {
            name: gate_name.to_string(),
            passed: false,
            duration_ms: 0,
            log_excerpt: Some(format!("repo dir does not exist: {}", repo_dir.display())),
        });
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
    capture(
        Command::new("nix")
            .arg("build")
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
    capture(
        Command::new("nix")
            .arg("eval")
            .arg(&target)
            .arg("--apply")
            .arg("_: null")
            .current_dir(repo_dir),
    ).await
}

async fn dispatch_nix_flake_check(repo_dir: &Path) -> Result<GateOutcome> {
    capture(
        Command::new("nix")
            .arg("flake")
            .arg("check")
            .arg("--no-build")
            .current_dir(repo_dir),
    ).await
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
