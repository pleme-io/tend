//! Token-aware git commit + push primitive.
//!
//! Every controller domain (flake apply today, helm release adapter
//! Phase 2, image tag bumps Phase 4) eventually writes to a git repo
//! and pushes. The shape is identical: stage files, commit with a
//! deterministic author, push to origin, optionally with an injected
//! GitHub token because in-cluster pods don't have ambient credentials.
//!
//! Token injection sets `http.<url>.extraheader` through the
//! environment (`crate::secret::GitConfigEnv`), so the credential
//! lands in neither `~/.gitconfig`, the remote URL, nor argv.
//!
//! It used to travel as `-c http.<url>.extraheader=...`, which kept it
//! out of the config files but put it in the **process table**, where
//! any account on the host could read it from `ps`. The env-scoped
//! carrier closes that; see `crate::secret` for the full rationale.
//!
//! Commit author is also passed per-call rather than baked into the
//! shell environment, so different controllers can sign their commits
//! distinctly (`tend-flake-bot` vs `tend-helm-bot`) for blame clarity.
//!
//! No force-push, no rebase on conflict — this is a "single operator
//! per cluster, no concurrent writers" primitive. Conflicts surface
//! as errors and the proposal goes to Failed; the next reconcile
//! replays from a fresh clone.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use tokio::process::Command;

use crate::secret::{GitConfigEnv, Secret};

#[derive(Debug, Clone)]
pub struct GitCommitter {
    pub name: String,
    pub email: String,
}

impl GitCommitter {
    /// Default identity for the tend operator's automated commits.
    /// Operators can override per-domain (e.g. `tend-helm-bot` for
    /// HelmRelease pin advances) to make `git blame` legible.
    #[must_use]
    pub fn tend_bot() -> Self {
        Self {
            name: "tend-operator[bot]".into(),
            email: "tend-operator@pleme.io".into(),
        }
    }
}

/// Fetch + hard-reset `repo_dir` to `origin/<branch>`. Use before
/// any apply step that intends to push: it guarantees the local
/// working tree is exactly origin's HEAD so the subsequent commit is
/// fast-forwardable. Without this, the operator's clone drifts from
/// origin every time someone else pushes — and the next apply hits
/// `! [rejected] main -> main (fetch first)`, which is non-recoverable
/// without intervention.
///
/// Discards any local changes. That's intentional — the operator's
/// clone is a scratch workspace, not a long-lived working tree, and
/// `apply_pin` rewrites flake.lock from scratch anyway.
///
/// The `token` argument is ignored when origin already carries an
/// embedded token (the pod's clone is set up by `tend sync` with
/// `https://x-access-token:<token>@github.com/...`). Stacking a
/// bearer header on top of URL basic-auth makes GitHub reject the
/// request with "invalid credentials" — origin URL credentials win.
pub async fn fetch_and_reset_to_origin(
    repo_dir: &Path,
    branch: &str,
    token: Option<&Secret>,
) -> Result<()> {
    let auth = github_auth_config(repo_dir, token).await;

    // Defensive: clear any stuck rebase / cherry-pick / merge state
    // that earlier failed apply attempts may have left behind. Each
    // command is best-effort — if there's nothing to abort, git
    // returns non-zero with "no rebase in progress" which we want to
    // ignore. The lockfile cleanup is the load-bearing part: stuck
    // rebases keep `.git/rebase-merge/` populated, and the next
    // `git commit` refuses with "interactive rebase in progress".
    let _ = clean_repo_state(repo_dir).await;

    git(repo_dir, &["fetch", "origin", branch], &auth)
        .await
        .context("git fetch origin")?;

    let target = format!("origin/{branch}");
    git(
        repo_dir,
        &["reset", "--hard", &target],
        &GitConfigEnv::new(),
    )
    .await
    .context("git reset --hard origin/<branch>")?;
    Ok(())
}

/// Build the ephemeral git config that authenticates a GitHub request.
///
/// Empty when there is no token, and empty when origin's URL already
/// carries basic-auth credentials — stacking a bearer header on top of
/// URL basic-auth makes GitHub reject the request as "invalid
/// credentials". Legacy clones can still be in that state until
/// `tend reconcile` heals them (see `jobs::remediate_remote`).
///
/// The config travels in the environment, never in argv. `-c
/// http...extraheader=AUTHORIZATION: bearer <token>` — what this code
/// did previously — put the token in the process table, where any
/// account on the host could read it out of `ps`.
async fn github_auth_config(repo_dir: &Path, token: Option<&Secret>) -> GitConfigEnv {
    let Some(secret) = token else {
        return GitConfigEnv::new();
    };
    if origin_has_embedded_credentials(repo_dir)
        .await
        .unwrap_or(false)
    {
        return GitConfigEnv::new();
    }
    secret.github_git_auth()
}

/// Best-effort cleanup of in-progress rebase / cherry-pick / merge /
/// am state. Each command swallows its own failure because "nothing
/// to abort" returns non-zero — that's fine, we want a clean tree
/// and don't care which mechanism left the dirty state.
async fn clean_repo_state(repo_dir: &Path) -> Result<()> {
    for cmd in [
        &["rebase", "--abort"][..],
        &["cherry-pick", "--abort"][..],
        &["merge", "--abort"][..],
        &["am", "--abort"][..],
    ] {
        let _ = Command::new("git")
            .args(cmd)
            .current_dir(repo_dir)
            .output()
            .await;
    }
    // Nuke any stranded `.git/index.lock` from a crashed previous git
    // invocation (the operator's old shell-out path could leave one
    // if the pod was OOM-killed mid-commit).
    let lock = repo_dir.join(".git/index.lock");
    let _ = tokio::fs::remove_file(&lock).await;
    Ok(())
}

/// True if `origin`'s URL embeds basic-auth credentials (e.g. the
/// `https://x-access-token:<token>@github.com/...` form `tend sync`
/// uses). Matters because layering a bearer-auth header on top makes
/// GitHub reject the combined request as "invalid credentials".
async fn origin_has_embedded_credentials(repo_dir: &Path) -> Result<bool> {
    let out = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_dir)
        .output()
        .await?;
    if !out.status.success() {
        return Ok(false);
    }
    let url = String::from_utf8_lossy(&out.stdout);
    Ok(crate::remote_url::has_embedded_credential(url.trim()))
}

/// Stage `paths` (relative to `repo_dir`), commit with `message` as
/// `committer`, push to `origin`. Returns the new commit sha.
///
/// `token`: when `Some`, injected as `Authorization: bearer <token>`
/// only for the `git push` invocation, against any `https://github.com/`
/// URL. Skopeo-style auth — never touches `.gitconfig`.
pub async fn commit_and_push(
    repo_dir: &Path,
    paths: &[&str],
    message: &str,
    committer: &GitCommitter,
    token: Option<&Secret>,
) -> Result<String> {
    let mut add = vec!["add"];
    add.extend(paths.iter().copied());
    git(repo_dir, &add, &GitConfigEnv::new())
        .await
        .context("git add")?;

    // Committer identity is per-call and must not persist to
    // .gitconfig. It rides the same environment carrier as auth —
    // one mechanism for all ephemeral git config rather than a
    // sensitive path and a separate ordinary one.
    let identity = GitConfigEnv::new()
        .with("user.name", &committer.name)
        .with("user.email", &committer.email);
    git(repo_dir, &["commit", "-m", message], &identity)
        .await
        .context("git commit")?;

    // Refspec must be fully qualified on both sides because the apply
    // path runs from a detached-HEAD state (we reset --hard to
    // origin/main earlier, then commit on top — `HEAD` alone has no
    // branch context to imply `refs/heads/main`).
    let auth = github_auth_config(repo_dir, token).await;
    git(repo_dir, &["push", "origin", "HEAD:refs/heads/main"], &auth)
        .await
        .context("git push")?;

    let head = git_capture(repo_dir, &["rev-parse", "HEAD"])
        .await
        .context("git rev-parse HEAD")?;
    Ok(head.trim().to_string())
}

/// Run git with `config` delivered through the environment.
///
/// Taking `&GitConfigEnv` rather than the previous ignored
/// `_token: Option<&str>` is deliberate: the old signature accepted a
/// secret and silently dropped it, so every call site had to remember
/// to *also* build `-c` args by hand. That is exactly how the token
/// ended up in argv. Now the only way to pass credentials is the
/// carrier that cannot reach the process table.
async fn git(repo_dir: &Path, args: &[&str], config: &GitConfigEnv) -> Result<()> {
    // Capture stdout+stderr instead of inheriting — git's actual error
    // ("[rejected] main -> main", "Permission denied", "infinite recursion
    // in submodule") needs to land in the anyhow chain so the reconciler
    // can pattern-match on it (e.g. push-race retry) and operators see
    // it in `kubectl get fpr -o yaml`. Without this, a non-zero exit
    // surfaces only as "exit 128" with no diagnostic context.
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo_dir);
    config.apply_async(&mut cmd);

    let output = cmd
        .output()
        .await
        .with_context(|| format!("spawning git {args:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "git {} failed in {} (exit {}): {}",
            args.iter()
                .map(|a| sanitize_for_log(a))
                .collect::<Vec<_>>()
                .join(" "),
            repo_dir.display(),
            output.status.code().unwrap_or(-1),
            // Combined stderr+stdout, trimmed; both can carry diagnostic
            // detail depending on the git command (push uses stderr,
            // fetch sometimes stdout). Truncate at 1 KiB to keep the
            // status field readable.
            tail(&format!("{stdout}\n{stderr}").trim(), 1024),
        ));
    }
    Ok(())
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let start = s.len() - max;
        // Snap to a UTF-8 boundary forward.
        let mut start = start;
        let bytes = s.as_bytes();
        while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
            start += 1;
        }
        format!("…{}", &s[start..])
    }
}

async fn git_capture(repo_dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .await
        .with_context(|| format!("spawning git {args:?}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Redact bearer tokens before any logging. Cheap — every `-c` arg
/// containing `extraheader=AUTHORIZATION:` is replaced with a marker.
fn sanitize_for_log(arg: &str) -> String {
    if arg.contains("extraheader=AUTHORIZATION:") {
        "extraheader=<redacted>".into()
    } else {
        arg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_redacts_bearer_token() {
        let s = sanitize_for_log(
            "http.https://github.com/.extraheader=AUTHORIZATION: bearer ghp_abc123",
        );
        assert!(!s.contains("ghp_abc123"));
        assert!(s.contains("redacted"));
    }

    #[test]
    fn sanitize_passes_through_safe_args() {
        assert_eq!(sanitize_for_log("push"), "push");
        assert_eq!(sanitize_for_log("origin"), "origin");
        assert_eq!(sanitize_for_log("HEAD"), "HEAD");
    }

    #[tokio::test]
    async fn commit_and_push_to_local_bare_repo_succeeds() {
        // End-to-end against a bare repo on the filesystem — the only
        // moving piece in apply we can exercise without network.
        // Token path is exercised by sanitize_redacts_bearer_token; the
        // shell composition is tested implicitly here.
        let bare = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(bare.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(work.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["remote", "add", "origin"])
            .arg(bare.path())
            .current_dir(work.path())
            .output()
            .unwrap();
        // Seed an initial commit so HEAD exists for git rev-parse.
        std::fs::write(work.path().join("a.txt"), "hello").unwrap();
        let committer = GitCommitter::tend_bot();
        let sha = commit_and_push(
            work.path(),
            &["a.txt"],
            // A real subject on purpose: this exercises the PRODUCTION commit path,
            // which must stay subject to the global commit-msg hook. Bypassing
            // hooks here would test something tend never actually does.
            "test: fixture commit exercising the bare-repo push path",
            &committer,
            None,
        )
        .await
        .unwrap();
        assert_eq!(sha.len(), 40, "expected 40-char sha, got `{sha}`");

        // Push another change to verify idempotent re-use.
        std::fs::write(work.path().join("a.txt"), "world").unwrap();
        let sha2 = commit_and_push(
            work.path(),
            &["a.txt"],
            "test: fixture second commit on the push path",
            &committer,
            None,
        )
        .await
        .unwrap();
        assert_ne!(sha, sha2);
    }
}
