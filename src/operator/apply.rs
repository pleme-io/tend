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
use super::lock_format::LockFormat;

pub struct ApplyOutcome {
    /// SHA of the commit that landed the new pin.
    pub commit: String,
}

pub async fn apply_pin(
    repo_dir: &Path,
    input_name: &str,
    new: &FlakeRev,
) -> Result<ApplyOutcome> {
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
    // when narHash already matches.
    let nix_status = Command::new("nix")
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

    sh(repo_dir, "git", &["add", "flake.lock"]).await?;
    sh(repo_dir, "git", &["commit", "-m", &msg]).await?;
    sh(repo_dir, "git", &["push"]).await?;

    let commit = sh_capture(repo_dir, "git", &["rev-parse", "HEAD"])
        .await?
        .trim()
        .to_string();
    Ok(ApplyOutcome { commit })
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

async fn sh(dir: &Path, cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd).args(args).current_dir(dir).status().await?;
    if !status.success() {
        return Err(anyhow!("`{} {}` failed in {}", cmd, args.join(" "), dir.display()));
    }
    Ok(())
}

async fn sh_capture(dir: &Path, cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd).args(args).current_dir(dir).output().await?;
    if !out.status.success() {
        return Err(anyhow!(
            "`{} {}` failed in {}: {}",
            cmd,
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
}
