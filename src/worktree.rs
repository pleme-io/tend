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

/// Branch a session's work lands on: `worktree-<slug>`.
///
/// Matches `claude --worktree`'s own naming so `list`/`land`/`prune` recognise
/// worktrees whoever created them.
#[must_use]
pub fn branch_name(slug: &str) -> String {
    let mut b = String::from(BRANCH_PREFIX);
    b.push_str(slug);
    b
}

/// Prefix identifying a session worktree branch — Claude Code's convention.
pub const BRANCH_PREFIX: &str = "worktree-";

/// Where a session's worktree lives: `<root>/<slug>`.
///
/// Deliberately OUTSIDE the repo. A worktree nested inside its own parent shows
/// up in that parent's `git status` as an untracked directory, which is exactly
/// the "unknown directory, might be unpushed work" shape tend refuses to touch —
/// self-inflicted, and it would also sweep into a `git add -A` in the parent,
/// which is the failure this module exists to prevent.
#[must_use]
pub fn worktree_path(root: &Path, slug: &str) -> PathBuf {
    root.join(slug)
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
            Removal::HasUncommitted => String::from(
                "REFUSED: uncommitted changes in the worktree — commit or discard them first",
            ),
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

/// Root for a repo's session worktrees: `<repo>/.claude/worktrees`.
///
/// ★ THIS IS CLAUDE CODE'S OWN LOCATION, not a tend convention. `claude
/// --worktree <name>` creates `<repo>/.claude/worktrees/<name>` on branch
/// `worktree-<name>`, locked (verified by running it, 2026-08-03). tend manages
/// THOSE worktrees rather than a parallel set of its own — two mechanisms that
/// cannot see each other would be worse than either alone.
///
/// Inside the repo is the right place and not an accident: CLAUDE.md discovery
/// walks cwd upward, so a worktree here keeps the repo's own CLAUDE.md AND the
/// `~/code/github/<org>` chain above it. An earlier tend-only design put these
/// under `$XDG_DATA_HOME`, which was tidier and silently stripped the fleet's
/// entire context from any agent working there.
///
/// The cost of being inside the repo is that `git add -A` in the MAIN checkout
/// stages the worktree unless `.claude/worktrees` is ignored — handled globally
/// by `blackmatter.components.gitconfig.globalIgnore`, because a per-repo
/// `.gitignore` leaves every new or cloned repo unsafe again.
#[must_use]
pub fn default_root(repo: &Path) -> PathBuf {
    repo.join(".claude").join("worktrees")
}

/// Mint a session name for a launch that has no id to inherit.
///
/// `CLAUDE_CODE_SESSION_ID` is published INSIDE a claude session for its
/// subprocesses. At LAUNCH there is no session yet, so the operator's own
/// launcher can never supply one — and a verb that errors there is not a
/// launcher. Measured 2026-08-03: pointing `cld` at `worktree session` broke it
/// in every directory with `no session id`, because the check ran in an
/// interactive shell rather than in a claude subprocess.
///
/// Pure: the caller supplies both parts, so every property is testable without
/// a clock or an RNG. `hhmm` + 7 hex fits SLUG_LEN (12) and stays `[a-z0-9-]`.
/// A collision is benign rather than destructive — `enter` ADOPTS an existing
/// worktree of the same slug instead of resetting it.
#[must_use]
pub fn mint_slug(hhmm: &str, rand_hex: &str) -> String {
    let mut raw = String::from(hhmm);
    raw.push_str(rand_hex);
    session_slug(&raw)
}

/// Impure companion to [`mint_slug`] — reads the clock and the pid.
///
/// Deliberately not `rand`: that dependency is optional in this crate and a
/// benign-collision slug does not justify enabling it. Minute-of-day plus 7 hex
/// of (nanos ^ pid<<32) is ~268M values per minute, and `enter` adopts rather
/// than resets on a repeat, so the failure mode of a collision is two launches
/// sharing a worktree, not lost work. No `format!` — TYPED EMISSION.
#[must_use]
pub fn mint_slug_now() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0u128, |d| d.as_nanos());

    let minute_of_day = ((nanos / 1_000_000_000) % 86_400) / 60;
    let mut hhmm = String::new();
    if minute_of_day < 1000 {
        hhmm.push('0');
    }
    if minute_of_day < 100 {
        hhmm.push('0');
    }
    if minute_of_day < 10 {
        hhmm.push('0');
    }
    hhmm.push_str(&minute_of_day.to_string());

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mixed = (nanos as u64) ^ (u64::from(std::process::id()) << 32);
    let mut rand_hex = String::new();
    for i in 0..7_u32 {
        let nibble = ((mixed >> (i * 4)) & 0xf) as usize;
        rand_hex.push(char::from(HEX[nibble]));
    }

    mint_slug(&hhmm, &rand_hex)
}

/// Is `dir` inside a git work tree?
///
/// The delegation path passes `--worktree`, which claude cannot honour outside a
/// repository. The operator's launcher (`cld`) is used EVERYWHERE, not only in
/// repos, so making it the isolated path requires the flag to be conditional —
/// and the condition belongs here, in the typed tool, rather than in a shell
/// conditional in an alias (NO SHELL). A false answer degrades to running
/// claude plainly, which is the pre-existing behaviour, so the worst case of a
/// misdetection is no isolation rather than a broken launcher.
#[must_use]
pub fn is_inside_work_tree(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out == "true")
}

/// Is `command` a set of extra flags for claude rather than a program to run?
///
/// `worktree session` has two shapes and this decides between them. A command
/// that is empty, or whose first token is a FLAG, means "run claude with these
/// extra args" and is delegated to claude's own `--worktree`. A command whose
/// first token is a program name gets a tend-made worktree instead, since
/// `--worktree` is claude's flag and nothing else has it.
///
/// WHY THIS EXISTS: before it, only the bare form delegated, so the launcher
/// the operator actually types — `claude --dangerously-skip-permissions` —
/// had no isolated form at all. `session -- --dangerously-skip-permissions`
/// took the program branch and tried to `exec` a flag. Measured 2026-08-03:
/// zero session worktrees existed across a night in which concurrent sessions
/// committed conflict markers to `main` three times, because the isolated path
/// could not express the way sessions are actually started.
#[must_use]
pub fn is_claude_args(command: &[String]) -> bool {
    command.first().is_none_or(|first| first.starts_with('-'))
}

/// Claude Code's project-state directory for a given working directory.
///
/// The slug is the absolute path with every `/` replaced by `-`, verified
/// against the live `~/.claude/projects/` listing.
#[must_use]
pub fn claude_project_dir(home: &Path, cwd: &Path) -> PathBuf {
    let slug: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .collect();
    home.join(".claude").join("projects").join(slug)
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
    fn a_launch_with_no_session_id_still_gets_a_usable_slug() {
        // CLAUDE_CODE_SESSION_ID exists only INSIDE a session, so a launcher
        // never has one. Measured 2026-08-03: pointing `cld` at
        // `worktree session` broke it in EVERY directory with
        // `Error: no session id`, because it had only been checked from a
        // claude subprocess (which does carry the variable) rather than from an
        // interactive shell.
        let s = mint_slug("0731", "a1b2c3d");
        assert!(!s.is_empty(), "a minted slug must never be empty");
        assert!(s.len() <= SLUG_LEN, "must fit the slug budget: {s}");
        assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "must be path- and ref-safe: {s}"
        );
        // Idempotent under the sanitiser — a minted slug is already legal, so
        // passing it through `session_slug` again cannot change it.
        assert_eq!(session_slug(&s), s, "minted slug must be a fixed point");

        // Distinct inputs stay distinct, so two launches in the same minute do
        // not silently share a worktree.
        assert_ne!(mint_slug("0731", "a1b2c3d"), mint_slug("0731", "e4f5a6b"));

        // And the live minter obeys the same contract.
        let live = mint_slug_now();
        assert!(live.len() <= SLUG_LEN, "live slug too long: {live}");
        assert_eq!(session_slug(&live), live, "live slug must be a fixed point");
    }

    #[test]
    fn resolve_refuses_to_invent_a_worktree_to_land() {
        // `land` used `enter`, which CREATES. Landing a session whose worktree
        // directory was gone therefore built a fresh one, and `enter`'s old
        // `-B` reset the branch that held the only copy of the work: the rebase
        // became a no-op, the push said `Everything up-to-date`, and it printed
        // `landed <sha>` over destroyed commits. Landing must resolve, never
        // invent.
        let tmp = std::env::temp_dir().join("tend-resolve-probe-does-not-exist");
        let err = resolve(&tmp, "sessionthatnevermadeaworktree")
            .expect_err("resolve must fail when no worktree exists");
        let msg = err.to_string();
        assert!(
            msg.contains("nothing to land"),
            "the error must say what the operator has to do, got: {msg}"
        );
    }

    #[test]
    fn claude_flags_delegate_but_a_program_does_not() {
        // The whole point: the launcher the operator types is
        // `claude --dangerously-skip-permissions`, so its isolated form must be
        // expressible. Before this, only the EMPTY command delegated and
        // `session -- --dangerously-skip-permissions` fell through to the
        // program branch, which tried to exec a flag.
        assert!(is_claude_args(&[]), "bare session must delegate to claude");
        assert!(
            is_claude_args(&[String::from("--dangerously-skip-permissions")]),
            "claude flags must delegate, not be exec'd as a program"
        );
        assert!(
            is_claude_args(&[String::from("-w")]),
            "short flags must delegate too"
        );

        // A program name still gets a tend-made worktree — `--worktree` is
        // claude's flag and nothing else has it.
        assert!(
            !is_claude_args(&[String::from("echo"), String::from("hi")]),
            "a program must NOT be turned into claude args"
        );
        assert!(
            !is_claude_args(&[String::from("claude")]),
            "an explicit `claude` is a program invocation, not a flag list"
        );
    }

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
                out.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "slug {out:?} from {raw:?} kept an unsafe character"
            );
            assert!(!out.is_empty(), "slug from {raw:?} was empty");
        }
    }

    #[test]
    fn empty_or_punctuation_only_session_never_yields_an_empty_ref() {
        // `worktree-` alone would collide across every id-less session.
        for raw in ["", "   ", "---", "~^:"] {
            assert_eq!(session_slug(raw), "unknown", "raw={raw:?}");
            assert_eq!(branch_name(&session_slug(raw)), "worktree-unknown");
        }
    }

    #[test]
    fn distinct_sessions_get_distinct_paths_and_branches() {
        let root = Path::new("/data/wt");
        let a = session_slug("aaaaaaaa-1111-4444-8888-000000000000");
        let b = session_slug("bbbbbbbb-2222-4444-8888-000000000000");
        assert_ne!(a, b);
        assert_ne!(worktree_path(root, &a), worktree_path(root, &b));
        assert_ne!(branch_name(&a), branch_name(&b));
    }

    #[test]
    fn same_session_is_stable_across_calls() {
        // The whole point: every tool call in one session lands in ONE worktree.
        let id = "adc3b17e-85cb-4428-8d96-f8003ef9c601";
        assert_eq!(session_slug(id), session_slug(id));
        let root = Path::new("/repo/.claude/worktrees");
        assert_eq!(
            worktree_path(root, &session_slug(id)),
            worktree_path(root, &session_slug(id))
        );
    }

    #[test]
    fn worktree_uses_claude_codes_native_location() {
        // tend must manage the SAME worktrees `claude --worktree` creates;
        // a parallel set the two mechanisms cannot see would be worse than either.
        let repo = Path::new("/Users/x/code/github/pleme-io/nix");
        assert_eq!(default_root(repo), repo.join(".claude/worktrees"));
        assert!(worktree_path(&default_root(repo), "abc123").ends_with(".claude/worktrees/abc123"));
        // Branch naming matches too, or list/prune would not recognise them.
        assert_eq!(branch_name("abc123"), "worktree-abc123");
        assert!(branch_name("x").starts_with(BRANCH_PREFIX));
    }

    #[test]
    fn worktree_root_keeps_the_claude_md_ancestor_chain() {
        // The org dir MUST remain an ancestor: CLAUDE.md discovery walks cwd
        // upward, and ~/code + ~/code/github + ~/code/github/<org> all live
        // above the repo. A root outside that chain silently strips the fleet's
        // entire context from any agent working in the worktree.
        let repo = Path::new("/Users/x/code/github/pleme-io/nix");
        let root = default_root(repo);
        let wt = worktree_path(&root, "abc123");
        for ancestor in [
            "/Users/x/code",
            "/Users/x/code/github",
            "/Users/x/code/github/pleme-io",
        ] {
            assert!(
                wt.starts_with(ancestor),
                "{ancestor} must stay an ancestor of {wt:?}"
            );
        }
        // INSIDE the repo, which also keeps the repo's own CLAUDE.md. The
        // `git add -A` exposure that creates is closed globally by
        // blackmatter.components.gitconfig.globalIgnore, not per-repo.
        assert!(wt.starts_with(repo));
    }

    #[test]
    fn project_dir_matches_claude_codes_cwd_slug() {
        // Verified against the live ~/.claude/projects listing: the slug is the
        // absolute path with every '/' replaced by '-'.
        let d = claude_project_dir(
            Path::new("/Users/x"),
            Path::new("/Users/x/code/github/pleme-io/nix"),
        );
        assert_eq!(
            d,
            Path::new("/Users/x/.claude/projects/-Users-x-code-github-pleme-io-nix")
        );
        // A worktree therefore gets a DIFFERENT dir — which is why `enter`
        // symlinks its memory back at the canonical one.
        let w = claude_project_dir(
            Path::new("/Users/x"),
            Path::new("/Users/x/code/github/pleme-io/.worktrees/nix/abc"),
        );
        assert_ne!(d, w);
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
        assert!(
            msg.contains("definitely-not-a-real-binary-xyz"),
            "got {msg:?}"
        );
        assert!(msg.contains('/'), "got {msg:?}");
    }

    #[test]
    fn refusal_reasons_say_what_to_do_next() {
        assert!(removal_verdict(false, 0)
            .reason()
            .contains("commit or discard"));
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
    let path = worktree_path(root, &slug);
    if path.join(".git").exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(path.parent().unwrap_or(root)).context("creating worktree root")?;

    let branch = branch_name(&slug);
    let base = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let p = path.to_string_lossy().to_string();

    // ADOPT an existing branch; never reset it.
    //
    // This used `-B`, which RESETS the branch to base when it already exists.
    // The case it was meant to smooth — a worktree directory removed while its
    // branch survived — is exactly the case where the branch is the ONLY place
    // that session's commits still live, and `-B` discarded them. Worse via
    // `land`, which called `enter` first: the reset threw the commits away, the
    // rebase became a no-op, the push said `Everything up-to-date`, and it
    // printed `landed <sha>` over destroyed work.
    //
    // So: if the ref exists, check it out as-is; only create when it does not.
    // Re-entry still does not fail on a leftover ref, which was the original
    // goal, and no path resets a branch that may hold unpushed work.
    let branch_ref = {
        let mut r = String::from("refs/heads/");
        r.push_str(&branch);
        r
    };
    let exists = git(repo, &["rev-parse", "--verify", "--quiet", &branch_ref]).is_ok();
    if exists {
        git(repo, &["worktree", "add", &p, &branch])?;
    } else {
        git(repo, &["worktree", "add", "-b", &branch, &p, &base])?;
    }
    link_project_memory(repo, &path);
    Ok(path)
}

/// Resolve a session's EXISTING worktree, or fail.
///
/// `land` must never create. It used `enter`, which creates on demand — so
/// landing a session whose worktree directory was gone silently built a fresh
/// one and (before the fix above) reset the branch that held the only copy of
/// the work. Landing is an operation ON existing work; if there is none to
/// find, that is an error the operator must see, not a thing to invent.
pub fn resolve(root: &Path, session: &str) -> Result<PathBuf> {
    let slug = session_slug(session);
    let path = worktree_path(root, &slug);
    if path.join(".git").exists() {
        return Ok(path);
    }
    let mut m = String::from("no worktree for session ");
    m.push_str(&slug);
    m.push_str(" at ");
    m.push_str(&path.to_string_lossy());
    m.push_str(" — nothing to land. `tend worktree list` shows what exists.");
    bail!(m)
}

/// Point the worktree's Claude-Code project memory at the canonical repo's.
///
/// Isolation must not cost memory. Project state is keyed by cwd, so a worktree
/// gets its OWN `~/.claude/projects/<slug>/` — empty. Every reference file,
/// every recorded incident and the MEMORY.md index would be invisible, and an
/// agent that has forgotten why a pin is load-bearing will helpfully remove it.
///
/// So `memory` is symlinked to the repo's project memory: one shared, accumulating
/// store, while transcripts stay per-worktree (those SHOULD be separate — they are
/// the session, not the knowledge).
///
/// Best-effort and silent: this is a convenience, and a `tend worktree` that
/// failed because `~/.claude` was missing would be worse than one that isolates
/// without sharing memory.
fn link_project_memory(repo: &Path, worktree: &Path) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let canonical = claude_project_dir(&home, repo).join("memory");
    if !canonical.is_dir() {
        return; // nothing to share yet
    }
    let mine = claude_project_dir(&home, worktree);
    if std::fs::create_dir_all(&mine).is_err() {
        return;
    }
    let link = mine.join("memory");
    if link.exists() || link.symlink_metadata().is_ok() {
        return; // already linked, or a real dir we must not clobber
    }
    let _ = std::os::unix::fs::symlink(&canonical, &link);
}

/// Rebase this worktree's branch onto the remote's default branch and push it.
///
/// The verb `prune` already advertised ("`tend worktree land` them first") and
/// that promise was unkept until now — a refusal pointing at a command that does
/// not exist is worse than no advice.
///
/// Refuses rather than improvises on anything ambiguous: a dirty tree (commit or
/// discard first — landing would silently leave it behind) and a rebase conflict
/// (left in place for a human to resolve IN the worktree, which is exactly what
/// the worktree is for).
pub fn land(worktree: &Path, base: &str) -> Result<String> {
    if !git(worktree, &["--no-optional-locks", "status", "--porcelain"])?.is_empty() {
        bail!("REFUSED: uncommitted changes — commit or discard them before landing");
    }
    git(worktree, &["fetch", "origin"])?;

    let onto = {
        let mut t = String::from("origin/");
        t.push_str(base);
        t
    };
    if git(worktree, &["rebase", &onto]).is_err() {
        let _ = git(worktree, &["rebase", "--abort"]);
        let mut m = String::from("REFUSED: rebase onto ");
        m.push_str(&onto);
        m.push_str(
            " conflicts. Resolve it in the worktree, then land again \
                    (the worktree is exactly where that work belongs).",
        );
        bail!(m);
    }

    let mut refspec = String::from("HEAD:");
    refspec.push_str(base);
    git(worktree, &["push", "origin", &refspec])?;
    git(worktree, &["rev-parse", "--short", "HEAD"])
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
                if p != repo && branch.starts_with(BRANCH_PREFIX) {
                    let clean = git(&p, &["--no-optional-locks", "status", "--porcelain"])
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
                    out.push(Entry {
                        path: p,
                        branch: branch.clone(),
                        clean,
                        ahead,
                    });
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
    // `claude --worktree` LOCKS the worktree it creates, so a plain remove
    // fails with "is locked". Unlock first — but only after the safety verdict
    // above has already cleared, so the lock is never the thing standing
    // between a mistake and lost work.
    git(repo, &["worktree", "unlock", &p]).ok();
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
