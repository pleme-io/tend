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

    /// Restore (revert) the given paths to HEAD, discarding working-tree edits.
    fn restore(&self, repo_dir: &Path, paths: &[&Path]) -> Result<()>;
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

    fn restore(&self, repo_dir: &Path, paths: &[&Path]) -> Result<()> {
        let mut args = vec!["checkout".to_string(), "--".to_string()];
        args.extend(paths.iter().map(|p| p.to_string_lossy().into_owned()));
        let output = Command::new("git")
            .args(&args)
            .current_dir(repo_dir)
            .output()
            .context("running git checkout -- to restore files")?;
        if !output.status.success() {
            anyhow::bail!(
                "git restore failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    fn commit(&self, repo_dir: &Path, message: &str) -> Result<()> {
        // vau CleanTreeWitness (safe-concurrent-git-mutation, theory/VAU.md) —
        // REFUSE to commit a conflicted tree. The live bug 7ac40036 staged
        // conflict markers into flake.lock and committed them. A plain
        // `git diff --check` (working-tree vs index) MISSES markers that are
        // already staged, so gate on BOTH: `ls-files -u` (unmerged index entries)
        // AND `git diff --cached --check` (STAGED markers, index vs HEAD — the
        // R1 correction; the 7ac40036 markers were staged-clean). Either
        // non-empty ⇒ a conflicted tree is not committable.
        let unmerged = Command::new("git")
            .args(["ls-files", "-u"])
            .current_dir(repo_dir)
            .output()
            .context("running git ls-files -u")?;
        if !unmerged.stdout.is_empty() {
            anyhow::bail!(
                "refusing to commit in {}: unmerged paths present \
                 (vau: a conflicted tree is not committable)",
                repo_dir.display()
            );
        }
        let staged = Command::new("git")
            .args(["diff", "--cached", "--check"])
            .current_dir(repo_dir)
            .output()
            .context("running git diff --cached --check")?;
        if !staged.status.success() {
            anyhow::bail!(
                "refusing to commit in {}: staged conflict markers detected by \
                 `git diff --cached --check` (vau: the 7ac40036 class — markers \
                 staged into a lockfile):\n{}",
                repo_dir.display(),
                String::from_utf8_lossy(&staged.stdout).trim()
            );
        }

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
        // vau PullOp = FastForward | RebaseOrAbort — NEVER a naive merge-pull.
        // The old `git pull origin <branch>` (merge default) auto-created a merge
        // commit OR left conflict markers in the tree that the ungated commit
        // path then staged + pushed (the live bug 7ac40036). This is Fix #1 and
        // is on its own sufficient to extinguish that class: markers are never
        // written to the working tree.
        //
        // 1) fast-forward (can never conflict);
        // 2) on divergence, rebase --autostash local onto origin;
        // 3) on a rebase conflict, ABORT (restore a clean tree) and bail —
        //    a conflicted state escalates to the operator, it is never committed.
        let ff = Command::new("git")
            .args(["pull", "--ff-only", "origin", branch])
            .current_dir(repo_dir)
            .output()
            .context("running git pull --ff-only")?;
        if ff.status.success() {
            return Ok(());
        }

        let rebase = Command::new("git")
            .args(["pull", "--rebase", "--autostash", "origin", branch])
            .current_dir(repo_dir)
            .output()
            .context("running git pull --rebase --autostash")?;
        if rebase.status.success() {
            return Ok(());
        }

        // Rebase hit a conflict — abort to keep the tree clean; never leave markers.
        let _ = Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(repo_dir)
            .output();
        anyhow::bail!(
            "git pull diverged and the rebase conflicted in {}; aborted to keep the \
             tree clean (vau: a conflicted tree is escalated, never committed). \
             ff_err={} rebase_err={}",
            repo_dir.display(),
            String::from_utf8_lossy(&ff.stderr).trim(),
            String::from_utf8_lossy(&rebase.stderr).trim(),
        );
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
