use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

/// Trait abstracting git operations for testability.
pub trait GitOps: Send + Sync {
    /// Stage a file for commit.
    fn add(&self, repo_dir: &Path, file_path: &Path) -> Result<()>;

    /// Check if there are staged changes.
    fn has_staged_changes(&self, repo_dir: &Path) -> Result<bool>;

    /// Create a commit with the given message.
    fn commit(&self, repo_dir: &Path, message: &str) -> Result<()>;

    /// Push to remote.
    fn push(&self, repo_dir: &Path) -> Result<()>;
}

/// Real implementation using system git commands.
pub struct SystemGitOps;

impl GitOps for SystemGitOps {
    fn add(&self, repo_dir: &Path, file_path: &Path) -> Result<()> {
        let output = Command::new("git")
            .args(["add", &file_path.to_string_lossy()])
            .current_dir(repo_dir)
            .output()
            .context("running git add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git add failed: {stderr}");
        }
        Ok(())
    }

    fn has_staged_changes(&self, repo_dir: &Path) -> Result<bool> {
        let status = Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(repo_dir)
            .status()
            .context("checking staged changes")?;
        Ok(!status.success())
    }

    fn commit(&self, repo_dir: &Path, message: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(repo_dir)
            .output()
            .context("running git commit")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git commit failed: {stderr}");
        }
        Ok(())
    }

    fn push(&self, repo_dir: &Path) -> Result<()> {
        let output = Command::new("git")
            .args(["push"])
            .current_dir(repo_dir)
            .output()
            .context("running git push")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git push failed: {stderr}");
        }
        Ok(())
    }
}
