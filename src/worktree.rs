//! Per-session git worktrees — real isolation for concurrent agents.
//!
//! WHY (measured 2026-08-02, pleme-io/nix): multiple
//! `claude --dangerously-skip-permissions` sessions share one working directory.
//! Eleven commits landed in ~28 hours with the subject `init`, several carrying
//! files a DIFFERENT session had staged seconds earlier; one pushed another
//! agent's half-written module. The mechanism is the SHARED GIT INDEX — one
//! `git add -A` stages whatever is in the tree, including someone else's work.
//!
//! Two mitigations already exist and neither is isolation:
//!   * a `commit-msg` hook refusing placeholder subjects — fixes the meaningless
//!     MESSAGE, not the contents;
//!   * a guardrail `warn` on whole-tree staging — advisory, and a warning is not
//!     a boundary.
//! A worktree gives each session its own index AND its own checkout, so the
//! failure becomes unconstructible rather than discouraged: session A's staged
//! files are not in session B's index because it is a different index file.
//!
//! WHAT THIS IS NOT. It does not make concurrent agents coordinate — it makes
//! them not collide. Landing work is still an explicit merge (`land`), because
//! two sessions editing the same file remains a real conflict that a human or a
//! rebase must resolve. Isolation removes accidental entanglement, not the need
//! to integrate.
//!
//! CAN'T-LOSE-WORK is the load-bearing invariant here, not a nicety. tend's own
//! operating rule is that an unrecognised directory may be unpushed work and is
//! NEVER removed. Every destructive path in this module goes through
//! [`removal_verdict`], which refuses on uncommitted changes or unpushed commits
//! and is a pure function so it can be exhaustively tested without a repo.

use std::path::{Path, PathBuf};

/// Longest session-id prefix kept in a path/branch name.
///
/// Claude session ids are UUIDs; the full 36 chars make unreadable paths while
/// 12 hex chars is ~10^14 values — collision is not a practical concern for the
/// handful of sessions alive at once, and the prefix stays greppable against the
/// full id in logs.
const SLUG_LEN: usize = 12;

/// Reduce a session identifier to something safe for a path and a git ref.
///
/// Git refs forbid a lot (`~^:?*[\`, `..`, trailing `.lock`, spaces), and a
/// session id is externally supplied — so this keeps only `[a-z0-9-]` rather
/// than trying to enumerate what is illegal. Empty input yields `unknown`, never
/// an empty ref, because `session/` alone is a valid-looking prefix that would
/// collide across every session with a missing id.
#[must_use]
pub fn session_slug(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        return String::from("unknown");
    }
    trimmed.chars().take(SLUG_LEN).collect()
}

/// Branch a session's work lands on: `session/<slug>`.
#[must_use]
pub fn branch_name(slug: &str) -> String {
    let mut b = String::from("session/");
    b.push_str(slug);
    b
}

/// Where a session's worktree lives: `<root>/<repo>/<slug>`.
///
/// Deliberately OUTSIDE the repo. A worktree nested inside its own parent shows
/// up in that parent's `git status` as an untracked directory, which is exactly
/// the "unknown directory, might be unpushed work" shape tend refuses to touch —
/// self-inflicted, and it would also sweep into a `git add -A` in the parent,
/// which is the failure this module exists to prevent.
#[must_use]
pub fn worktree_path(root: &Path, repo_name: &str, slug: &str) -> PathBuf {
    root.join(repo_name).join(slug)
}

/// Whether a worktree may be removed.
///
/// Pure so every branch is testable without a repo — the destructive decision is
/// the one thing here that must never be reasoned about loosely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Clean tree, nothing unpushed — safe to remove.
    Safe,
    /// Working tree has uncommitted changes.
    HasUncommitted,
    /// Commits exist that the remote does not have.
    HasUnpushed(usize),
}

impl Removal {
    #[must_use]
    pub const fn is_safe(self) -> bool {
        matches!(self, Removal::Safe)
    }

    /// Operator-facing reason. `Display`-family text, never `format!` —
    /// TYPED EMISSION.
    #[must_use]
    pub fn reason(self) -> String {
        match self {
            Removal::Safe => String::from("clean and fully pushed"),
            Removal::HasUncommitted => {
                String::from("REFUSED: uncommitted changes in the worktree — commit or discard them first")
            }
            Removal::HasUnpushed(n) => {
                let mut s = String::from("REFUSED: ");
                s.push_str(&n.to_string());
                s.push_str(" commit(s) not on the remote — `tend worktree land` them first");
                s
            }
        }
    }
}

/// Decide whether a worktree can be removed.
///
/// Order matters: uncommitted work is reported before unpushed commits because
/// it is the more urgent loss (it exists nowhere else at all), and a tree can
/// easily be both.
#[must_use]
pub const fn removal_verdict(clean: bool, ahead: usize) -> Removal {
    if !clean {
        return Removal::HasUncommitted;
    }
    if ahead > 0 {
        return Removal::HasUnpushed(ahead);
    }
    Removal::Safe
}

/// Default root for worktrees: `$XDG_DATA_HOME/tend/worktrees`, else
/// `~/.local/share/tend/worktrees`.
///
/// Not under `~/code`: that tree is what tend's own discovery walks, and a
/// worktree there would read as an unknown repo on every `tend status`.
#[must_use]
pub fn default_root() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("tend").join("worktrees")
}

/// Session id from the environment, if an agent runtime published one.
///
/// `CLAUDE_CODE_SESSION_ID` is set per Claude Code session (verified live
/// 2026-08-02), which is exactly the identity this needs: stable for the whole
/// session, distinct between concurrent ones. Falling back to the PID would be
/// wrong — a session runs many processes, so each tool call would land in a
/// different worktree.
#[must_use]
pub fn session_from_env() -> Option<String> {
    for key in ["CLAUDE_CODE_SESSION_ID", "TEND_SESSION_ID"] {
        if let Some(v) = std::env::var_os(key) {
            let s = v.to_string_lossy().to_string();
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_ref_safe_and_bounded() {
        // A real Claude session id.
        let s = session_slug("adc3b17e-85cb-4428-8d96-f8003ef9c601");
        assert_eq!(s, "adc3b17e-85c");
        assert!(s.len() <= SLUG_LEN);
        // Characters git refs forbid are dropped, not escaped.
        for raw in ["a~b^c:d?e*f[g\\h", "with spaces", "UPPER", "..dots.."] {
            let out = session_slug(raw);
            assert!(
                out.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "slug {out:?} from {raw:?} kept an unsafe character"
            );
            assert!(!out.is_empty(), "slug from {raw:?} was empty");
        }
    }

    #[test]
    fn empty_or_punctuation_only_session_never_yields_an_empty_ref() {
        // `session/` alone would collide across every id-less session.
        for raw in ["", "   ", "---", "~^:"] {
            assert_eq!(session_slug(raw), "unknown", "raw={raw:?}");
            assert_eq!(branch_name(&session_slug(raw)), "session/unknown");
        }
    }

    #[test]
    fn distinct_sessions_get_distinct_paths_and_branches() {
        let root = Path::new("/data/wt");
        let a = session_slug("aaaaaaaa-1111-4444-8888-000000000000");
        let b = session_slug("bbbbbbbb-2222-4444-8888-000000000000");
        assert_ne!(a, b);
        assert_ne!(worktree_path(root, "nix", &a), worktree_path(root, "nix", &b));
        assert_ne!(branch_name(&a), branch_name(&b));
    }

    #[test]
    fn same_session_is_stable_across_calls() {
        // The whole point: every tool call in one session lands in ONE worktree.
        let id = "adc3b17e-85cb-4428-8d96-f8003ef9c601";
        assert_eq!(session_slug(id), session_slug(id));
        let root = Path::new("/data/wt");
        assert_eq!(
            worktree_path(root, "nix", &session_slug(id)),
            worktree_path(root, "nix", &session_slug(id))
        );
    }

    #[test]
    fn worktree_lives_outside_the_repo() {
        let p = worktree_path(Path::new("/data/wt"), "nix", "abc123");
        assert!(!p.starts_with("/Users/x/code"), "must not nest under the source tree");
        assert!(p.ends_with("nix/abc123"));
    }

    #[test]
    fn removal_refuses_to_lose_work() {
        // Uncommitted beats unpushed: it exists nowhere else at all.
        assert_eq!(removal_verdict(false, 0), Removal::HasUncommitted);
        assert_eq!(removal_verdict(false, 7), Removal::HasUncommitted);
        assert_eq!(removal_verdict(true, 3), Removal::HasUnpushed(3));
        assert_eq!(removal_verdict(true, 0), Removal::Safe);

        assert!(removal_verdict(true, 0).is_safe());
        assert!(!removal_verdict(false, 0).is_safe());
        assert!(!removal_verdict(true, 1).is_safe());
    }

    #[test]
    fn a_branch_with_no_upstream_is_never_treated_as_safe() {
        // The fallback must land on "has work", not "clean". A never-pushed
        // branch reporting Safe is how a worktree full of work gets deleted.
        assert!(!removal_verdict(true, 1).is_safe());
        assert_eq!(removal_verdict(true, 1), Removal::HasUnpushed(1));
    }

    #[test]
    fn exec_failure_names_the_command_and_the_directory() {
        // exec_in only returns on failure, and that error is the only thing the
        // operator sees when a launcher is misconfigured — it must say WHAT could
        // not run and WHERE, not just "No such file or directory".
        let err = exec_in(Path::new("/"), "definitely-not-a-real-binary-xyz", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("definitely-not-a-real-binary-xyz"), "got {msg:?}");
        assert!(msg.contains('/'), "got {msg:?}");
    }

    #[test]
    fn refusal_reasons_say_what_to_do_next() {
        assert!(removal_verdict(false, 0).reason().contains("commit or discard"));
        let r = removal_verdict(true, 2).reason();
        assert!(r.contains('2') && r.contains("land"), "got {r:?}");
        assert!(removal_verdict(true, 0).reason().contains("clean"));
    }
}

// ═══════════════════════════════════════════════════════════════════
// Operations
// ═══════════════════════════════════════════════════════════════════

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Run git in `dir`, returning trimmed stdout. Errors carry git's own stderr —
/// a wrapper that swallows it makes every failure look identical.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| String::from("running git"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let mut m = String::from("git ");
        m.push_str(&args.join(" "));
        m.push_str(" failed: ");
        m.push_str(stderr.trim());
        bail!(m);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Repo directory name, used as the middle path segment.
fn repo_name(repo: &Path) -> String {
    repo.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| String::from("repo"))
}

/// Create (or reuse) this session's worktree and return its path.
///
/// Idempotent by design: every tool call in a session calls this, and it must
/// land in the SAME place rather than accumulating worktrees.
pub fn enter(repo: &Path, session: &str, root: &Path) -> Result<PathBuf> {
    let slug = session_slug(session);
    let path = worktree_path(root, &repo_name(repo), &slug);
    if path.join(".git").exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap_or(root)).context("creating worktree root")?;

    let branch = branch_name(&slug);
    let base = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let p = path.to_string_lossy().to_string();

    // `git worktree add -B` resets the branch if it already exists (a worktree
    // removed without its branch), so a re-entered session does not fail on a
    // leftover ref.
    git(repo, &["worktree", "add", "-B", &branch, &p, &base])?;
    Ok(path)
}

/// One worktree's state.
pub struct Entry {
    pub path: PathBuf,
    pub branch: String,
    pub clean: bool,
    pub ahead: usize,
}

impl Entry {
    #[must_use]
    pub fn verdict(&self) -> Removal {
        removal_verdict(self.clean, self.ahead)
    }
}

/// Every worktree of `repo` except the main checkout.
pub fn list(repo: &Path) -> Result<Vec<Entry>> {
    let raw = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch = String::new();
    for line in raw.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
            branch.clear();
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = b.trim_start_matches("refs/heads/").to_string();
        } else if line.is_empty() {
            if let Some(p) = path.take() {
                // Skip the main checkout — it is not session-owned and must
                // never be offered to a removal path.
                if p != repo && branch.starts_with("session/") {
                    let clean = git(&p, &["status", "--porcelain"])
                        .map(|s| s.is_empty())
                        .unwrap_or(false);
                    let ahead = git(&p, &["rev-list", "--count", "@{upstream}..HEAD"])
                        .ok()
                        .and_then(|s| s.parse().ok())
                        // No upstream => fall back to "commits on no remote at
                        // all". Counting 0 here would make a never-pushed branch
                        // look safe to delete, which is the one mistake this
                        // module must not make.
                        //
                        // CONSEQUENCE, measured and deliberate: in a repo with NO
                        // remote, even the base commit is on no remote, so every
                        // worktree reads as holding unpushed work and `prune`
                        // removes nothing. That is the correct answer — with no
                        // remote, nothing is backed up anywhere — not a bug to
                        // "fix" by relaxing the count. Verified against a repo
                        // WITH a remote: a clean, fully-pushed worktree is
                        // reported Safe and pruned.
                        .unwrap_or_else(|| {
                            git(&p, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
                                .ok()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(1)
                        });
                    out.push(Entry { path: p, branch: branch.clone(), clean, ahead });
                }
            }
        }
    }
    Ok(out)
}

/// Remove a worktree, refusing if that would lose work.
pub fn remove(repo: &Path, entry: &Entry) -> Result<()> {
    let verdict = entry.verdict();
    if !verdict.is_safe() {
        bail!(verdict.reason());
    }
    let p = entry.path.to_string_lossy().to_string();
    git(repo, &["worktree", "remove", &p])?;
    git(repo, &["branch", "-D", &entry.branch]).ok();
    Ok(())
}

/// Replace this process with `command` running inside `dir`.
///
/// `exec`, not spawn-and-wait: the launched agent should BE this process, so
/// signals, the tty, and the exit code pass straight through. A wrapper that
/// waits would swallow Ctrl-C and report its own status — the same
/// exit-code-masking class as piping through `tail`.
///
/// # Errors
/// Only returns if the exec itself failed; on success it never returns.
pub fn exec_in(dir: &Path, command: &str, args: &[String]) -> Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    let err = Command::new(command).args(args).current_dir(dir).exec();
    let mut m = String::from("exec ");
    m.push_str(command);
    m.push_str(" in ");
    m.push_str(&dir.to_string_lossy());
    m.push_str(": ");
    m.push_str(&err.to_string());
    bail!(m)
}
