//! Apply path — write a verified pin to flake.lock and push.
//!
//! Sequence (every step idempotent):
//!   1. Read flake.lock
//!   2. Adapter `write_pin()` produces new contents
//!   3. Atomic write (tmpfile + rename)
//!   4. `nix flake lock --update-input <name>` so Nix recomputes
//!      narHash if discovery left it empty
//!   5. git add + git commit + git push
//!
//! Failure at any step leaves the proposal in `Failed` phase with
//! the error in `status.error` — caller handles. No partial writes.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use super::crds::FlakeRev;
use super::flake_lock_adapter::FlakeLockAdapter;
use super::git_ops::{commit_and_push, fetch_and_reset_to_origin, GitCommitter};
use super::lock_format::LockFormat;

pub struct ApplyOutcome {
    /// SHA of the commit that landed the new pin.
    pub commit: String,
}

pub async fn apply_pin(
    repo_dir: &Path,
    input_name: &str,
    new: &FlakeRev,
    token: Option<&str>,
) -> Result<ApplyOutcome> {
    // Concurrent-writer race: the user (or another bot) can push to
    // origin/main between our fetch+reset and our push. When that
    // happens git rejects with "Updates were rejected because the
    // remote contains work that you do not have locally."
    //
    // Bounded retry: refetch, replay write+commit, push again. The
    // retry envelope catches the race; if origin keeps moving past
    // our budget we surface as Failed and the next reconcile picks
    // it up fresh.
    const MAX_ATTEMPTS: usize = 3;
    for attempt in 0..MAX_ATTEMPTS {
        match try_apply_pin_once(repo_dir, input_name, new, token).await {
            Ok(outcome) => return Ok(outcome),
            Err(e) if attempt + 1 < MAX_ATTEMPTS && is_push_race(&e) => {
                tracing::warn!(
                    attempt,
                    error = %format!("{e:#}"),
                    "apply: push race, retrying after refetch"
                );
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop exits via return");
}

/// One attempt at the apply pipeline: fetch fresh origin, rewrite
/// flake.lock for the proposed pin, commit, push. Pulled out so the
/// retry loop can replay each attempt against a fresh origin state.
async fn try_apply_pin_once(
    repo_dir: &Path,
    input_name: &str,
    new: &FlakeRev,
    token: Option<&str>,
) -> Result<ApplyOutcome> {
    fetch_and_reset_to_origin(repo_dir, "main", token)
        .await
        .context("fetch + reset to origin/main before apply")?;

    let lock_path = repo_dir.join("flake.lock");
    let contents = tokio::fs::read_to_string(&lock_path)
        .await
        .with_context(|| format!("reading {}", lock_path.display()))?;

    let updated = FlakeLockAdapter
        .write_pin(&contents, input_name, new)
        .with_context(|| format!("writing pin for `{input_name}`"))?;

    atomic_write(&lock_path, &updated)
        .await
        .with_context(|| format!("atomic write {}", lock_path.display()))?;

    // Let Nix rebalance narHash if discovery left it empty. Idempotent
    // when narHash already matches. `gates::nix_env` handles HOME,
    // GITHUB_TOKEN access, accept-flake-config, etc. — same primitive
    // every nix invocation in the operator uses.
    let mut cmd = Command::new("nix");
    super::gates::nix_env(&mut cmd);
    let nix_status = cmd
        .arg("flake")
        .arg("lock")
        .arg("--update-input")
        .arg(input_name)
        .current_dir(repo_dir)
        .status()
        .await
        .context("running nix flake lock")?;
    if !nix_status.success() {
        return Err(anyhow!("nix flake lock --update-input {input_name} failed"));
    }

    let msg = format!(
        "chore(flake): bump {input_name} to {} (tend operator)",
        short_rev(&new.rev)
    );
    let commit = commit_and_push(
        repo_dir,
        &["flake.lock"],
        &msg,
        &GitCommitter::tend_bot(),
        token,
    )
    .await
    .context("commit + push flake.lock pin")?;
    Ok(ApplyOutcome { commit })
}

/// True when the error chain matches one of git's canonical push-race
/// signatures — origin moved between our fetch and our push, retry
/// makes sense. Substring match because we shell out to `git`.
fn is_push_race(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("rejected")
        || s.contains("fetch first")
        || s.contains("non-fast-forward")
        || s.contains("failed to push some refs")
        || s.contains("remote contains work that you do not have")
}

async fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    let tmp = tempfile_in(dir, path);
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

fn tempfile_in(dir: &Path, target: &Path) -> PathBuf {
    let stem = target.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".{stem}.tmp.{pid}.{ts}"))
}

fn short_rev(rev: &str) -> &str {
    if rev.len() > 8 {
        &rev[..8]
    } else {
        rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_write_replaces_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.txt");
        tokio::fs::write(&p, "old").await.unwrap();
        atomic_write(&p, "new").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "new");
    }

    #[test]
    fn short_rev_truncates() {
        assert_eq!(short_rev("abcdef0123456789"), "abcdef01");
        assert_eq!(short_rev("short"), "short");
    }

    #[test]
    fn is_push_race_detects_canonical_signatures() {
        // git's three canonical "you lost the race" messages — all
        // need to trigger retry. Composed errors via anyhow::Error
        // since the real failure path wraps several context layers.
        let e = anyhow::anyhow!("commit + push: git push failed (exit 128): \
            ! [rejected] main -> main (fetch first)");
        assert!(is_push_race(&e), "rejected/fetch-first should retry: {e:#}");

        let e = anyhow::anyhow!("git push: error: failed to push some refs \
            because the remote contains work that you do not have locally");
        assert!(is_push_race(&e), "remote ahead should retry: {e:#}");

        let e = anyhow::anyhow!("non-fast-forward update");
        assert!(is_push_race(&e), "non-fast-forward should retry: {e:#}");
    }

    #[test]
    fn is_push_race_does_not_retry_on_unrelated_errors() {
        let e = anyhow::anyhow!("Permission denied (publickey)");
        assert!(!is_push_race(&e), "auth failures should not retry: {e:#}");

        let e = anyhow::anyhow!("nix flake lock --update-input fenix failed");
        assert!(!is_push_race(&e), "nix failures should not retry: {e:#}");
    }
}
