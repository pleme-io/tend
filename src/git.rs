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

    /// Get the current branch name.
    fn current_branch(&self, repo_dir: &Path) -> Result<String>;

    /// Pull from remote for the given branch.
    fn pull(&self, repo_dir: &Path, branch: &str) -> Result<()>;

    /// Check if the working tree is clean (no uncommitted changes).
    fn is_clean(&self, repo_dir: &Path) -> Result<bool>;
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

    fn current_branch(&self, repo_dir: &Path) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(repo_dir)
            .output()
            .context("running git rev-parse --abbrev-ref HEAD")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git rev-parse failed: {stderr}");
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn pull(&self, repo_dir: &Path, branch: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["pull", "origin", branch])
            .current_dir(repo_dir)
            .output()
            .context("running git pull")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git pull failed: {stderr}");
        }
        Ok(())
    }

    fn is_clean(&self, repo_dir: &Path) -> Result<bool> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(repo_dir)
            .output()
            .context("running git status --porcelain")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git status failed: {stderr}");
        }
        Ok(output.stdout.is_empty())
    }
}
