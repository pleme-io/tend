//! Token-aware git commit + push primitive.
//!
//! Every controller domain (flake apply today, helm release adapter
//! Phase 2, image tag bumps Phase 4) eventually writes to a git repo
//! and pushes. The shape is identical: stage files, commit with a
//! deterministic author, push to origin, optionally with an injected
//! GitHub token because in-cluster pods don't have ambient credentials.
//!
//! Token injection uses `git -c http.<url>.extraheader=...` so the
//! credential never lands in `~/.gitconfig` or the remote URL —
//! cleaner than rewriting `origin` to embed the secret, idempotent
//! across pod restarts, and survives `git remote set-url`.
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
pub async fn fetch_and_reset_to_origin(
    repo_dir: &Path,
    branch: &str,
    token: Option<&str>,
) -> Result<()> {
    let extra_header;
    let auth_args: Vec<&str> = if let Some(t) = token {
        extra_header = format!(
            "http.https://github.com/.extraheader=AUTHORIZATION: bearer {t}"
        );
        vec!["-c", &extra_header]
    } else {
        Vec::new()
    };

    let mut fetch = auth_args.clone();
    fetch.extend_from_slice(&["fetch", "origin", branch]);
    git(repo_dir, &fetch, None).await.context("git fetch origin")?;

    let target = format!("origin/{branch}");
    git(repo_dir, &["reset", "--hard", &target], None)
        .await
        .context("git reset --hard origin/<branch>")?;
    Ok(())
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
    token: Option<&str>,
) -> Result<String> {
    let mut add = vec!["add"];
    add.extend(paths.iter().copied());
    git(repo_dir, &add, None).await.context("git add")?;

    let commit_args: Vec<&str> = vec![
        "-c",
        // committer identity is per-call; doesn't persist to .gitconfig
    ];
    let _ = commit_args; // placeholder — using -c for both name+email below

    let name_kv = format!("user.name={}", committer.name);
    let email_kv = format!("user.email={}", committer.email);
    let commit = vec![
        "-c", name_kv.as_str(),
        "-c", email_kv.as_str(),
        "commit", "-m", message,
    ];
    git(repo_dir, &commit, None).await.context("git commit")?;

    let mut push = vec!["push", "origin", "HEAD"];
    let extra_header;
    if let Some(t) = token {
        // Authorization header avoids embedding the token in the
        // remote URL; works for every https://github.com/* repo
        // regardless of which one origin points to.
        extra_header = format!(
            "http.https://github.com/.extraheader=AUTHORIZATION: bearer {t}"
        );
        push = vec!["-c", &extra_header, "push", "origin", "HEAD"];
    }
    git(repo_dir, &push, token).await.context("git push")?;

    let head = git_capture(repo_dir, &["rev-parse", "HEAD"])
        .await
        .context("git rev-parse HEAD")?;
    Ok(head.trim().to_string())
}

async fn git(repo_dir: &Path, args: &[&str], _token: Option<&str>) -> Result<()> {
    // Capture stdout+stderr instead of inheriting — git's actual error
    // ("[rejected] main -> main", "Permission denied", "infinite recursion
    // in submodule") needs to land in the anyhow chain so the reconciler
    // can pattern-match on it (e.g. push-race retry) and operators see
    // it in `kubectl get fpr -o yaml`. Without this, a non-zero exit
    // surfaces only as "exit 128" with no diagnostic context.
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .await
        .with_context(|| format!("spawning git {args:?}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "git {} failed in {} (exit {}): {}",
            args.iter().map(|a| sanitize_for_log(a)).collect::<Vec<_>>().join(" "),
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
        let sha = commit_and_push(work.path(), &["a.txt"], "init", &committer, None)
            .await
            .unwrap();
        assert_eq!(sha.len(), 40, "expected 40-char sha, got `{sha}`");

        // Push another change to verify idempotent re-use.
        std::fs::write(work.path().join("a.txt"), "world").unwrap();
        let sha2 =
            commit_and_push(work.path(), &["a.txt"], "update", &committer, None)
                .await
                .unwrap();
        assert_ne!(sha, sha2);
    }
}
