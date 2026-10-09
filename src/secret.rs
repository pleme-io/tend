//! Typed secrets, and the only sanctioned ways to hand one to git.
//!
//! # Why this module exists
//!
//! On 2026-07-29 a fleet sweep found a GitHub token fossilized in the
//! `.git/config` of 25 repos across two orgs — three distinct tokens,
//! one still valid. The cause was `sync::inject_github_token`, which
//! rewrote clone URLs to
//! `https://x-access-token:<token>@github.com/...` so container clones
//! could authenticate without a prompt. Git persists a clone URL
//! verbatim, so every clone permanently recorded whatever token was
//! live at the time.
//!
//! That bug was possible because a secret was an ordinary `String`.
//! Nothing in the type system distinguished it from a URL, a path, or
//! a log line, so `format!` interpolated it into a URL that got
//! written to disk and nobody noticed for months.
//!
//! Auditing the rest of tend for the same shape turned up two more
//! instances of the class:
//!
//!   - `operator::git_ops` passed `-c http...extraheader=AUTHORIZATION:
//!     bearer <token>` as a **command-line argument**, which puts the
//!     token in the process table where any user on the host can read
//!     it out of `ps`.
//!   - `operator::gates` interpolated the token into `NIX_CONFIG`.
//!
//! # The two rules
//!
//! 1. **A credential is a typed token, never a `String`.** Today that
//!    type is shikumi's `GithubToken` (see `src/gh_auth.rs`), which
//!    replaced this module's own `Secret` when the credential became
//!    config (`github_auth:`): its `Debug`/`Display` redact, it has no
//!    `Deref<Target = str>`, and reading the value means calling one of
//!    its renderers (`expose_for_header()`, `git_extraheader()`,
//!    `nix_access_tokens_line()`) — grep-able at review. `Secret` held
//!    the same contract for a bare string and was deleted rather than
//!    kept as a second carrier for the same credential.
//!
//! 2. **A secret reaches a subprocess only through
//!    [`GitConfigEnv`].** Not argv (the process table is world-
//!    readable), not a URL (git persists those), not a config file.
//!    Environment-scoped git config lives for exactly one process and
//!    is written nowhere.
//!
//! # What this module does not defend against
//!
//! Tier-honest, per theory/UNREPRESENTABILITY.md: this makes leaks
//! unrepresentable *by accident*. A determined author can still call
//! `expose_for_header()` and print the result. The goal is that every path
//! to a leak requires typing a renderer name — one grep-able token that a
//! reviewer can audit — rather than being the default behavior of
//! `format!`.

use std::process::Command;

/// Git configuration delivered to a child process through the
/// environment, via git's `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` /
/// `GIT_CONFIG_VALUE_<n>` protocol (git >= 2.31).
///
/// This is the sanctioned carrier for anything sensitive, and the
/// reason is the two alternatives are both leaks:
///
///   - `-c key=value` on the command line puts the value in the
///     process table, readable by `ps` from any account on the host.
///     This is what `operator::git_ops` did before this module.
///   - Credentials in the remote URL get persisted by git into
///     `.git/config` on clone. This is what `sync` did, and it is the
///     bug that produced the 25-repo leak.
///
/// Environment config has neither property: it exists for the lifetime
/// of one process and is written to no file. Non-sensitive config
/// (committer identity) rides the same carrier for uniformity — one
/// mechanism to reason about rather than a sensitive path and a
/// separate ordinary one.
#[derive(Debug, Clone, Default)]
pub(crate) struct GitConfigEnv {
    entries: Vec<(String, String)>,
}

impl GitConfigEnv {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.entries.push((key.into(), value.into()));
        self
    }

    /// Merge another set, preserving order. Lets a call site compose
    /// auth config with identity config without either knowing about
    /// the other.
    pub(crate) fn merge(mut self, other: GitConfigEnv) -> Self {
        self.entries.extend(other.entries);
        self
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The environment pairs git expects. Materialized separately from
    /// [`Self::apply`] so both the sync and async `Command` types can
    /// consume it, and so tests can assert on the exact pairs.
    pub(crate) fn env_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = Vec::with_capacity(self.entries.len() * 2 + 1);
        pairs.push((
            "GIT_CONFIG_COUNT".to_string(),
            self.entries.len().to_string(),
        ));
        for (i, (key, value)) in self.entries.iter().enumerate() {
            pairs.push((format!("GIT_CONFIG_KEY_{i}"), key.clone()));
            pairs.push((format!("GIT_CONFIG_VALUE_{i}"), value.clone()));
        }
        pairs
    }

    pub(crate) fn apply(&self, cmd: &mut Command) {
        if self.is_empty() {
            return;
        }
        for (k, v) in self.env_pairs() {
            cmd.env(k, v);
        }
    }

    pub(crate) fn apply_async(&self, cmd: &mut tokio::process::Command) {
        if self.is_empty() {
            return;
        }
        for (k, v) in self.env_pairs() {
            cmd.env(k, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gh_auth::tests::{creds, literal, TOKEN};

    fn auth() -> GitConfigEnv {
        crate::gh_auth::git_auth(&creds(&[literal(TOKEN)]).token().unwrap())
    }

    /// Pins the exact header git needs. Bearer is NOT interchangeable
    /// here — it fails to authenticate git-over-HTTPS and git falls
    /// through to prompting.
    #[test]
    fn git_auth_env_carries_basic_header() {
        use base64::Engine as _;
        let pairs = auth().env_pairs();
        assert_eq!(pairs[0], ("GIT_CONFIG_COUNT".into(), "1".into()));
        assert_eq!(
            pairs[1],
            (
                "GIT_CONFIG_KEY_0".into(),
                "http.https://github.com/.extraheader".into()
            )
        );

        let expected =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"));
        assert_eq!(
            pairs[2],
            (
                "GIT_CONFIG_VALUE_0".into(),
                format!("AUTHORIZATION: basic {expected}")
            )
        );
    }

    /// The load-bearing property of the whole module: applying auth to
    /// a command puts the secret in the environment and **nowhere in
    /// argv**, so it cannot be read out of the process table.
    #[test]
    fn applying_auth_never_touches_argv() {
        let mut cmd = Command::new("git");
        cmd.args(["clone", "https://github.com/org/repo.git"]);
        auth().apply(&mut cmd);

        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{TOKEN}"));

        // Neither the raw token nor its base64 form may appear in argv
        // — base64 is encoding, not concealment, and anything in argv
        // is readable from the process table.
        let argv = format!("{:?}", cmd.get_args().collect::<Vec<_>>());
        assert!(!argv.contains(TOKEN), "raw secret reached argv: {argv}");
        assert!(
            !argv.contains(&encoded),
            "encoded secret reached argv: {argv}"
        );

        let in_env = cmd
            .get_envs()
            .any(|(_, v)| v.is_some_and(|v| v.to_string_lossy().contains(&encoded)));
        assert!(in_env, "credential did not reach the environment");
    }

    #[test]
    fn merge_preserves_order_and_renumbers() {
        let combined = GitConfigEnv::new().with("user.name", "tend").merge(auth());
        let pairs = combined.env_pairs();

        assert_eq!(pairs[0], ("GIT_CONFIG_COUNT".into(), "2".into()));
        assert_eq!(pairs[1].1, "user.name");
        assert_eq!(pairs[3].1, "http.https://github.com/.extraheader");
    }

    /// An empty config set must not export `GIT_CONFIG_COUNT=0` — git
    /// accepts it, but leaving the environment untouched keeps the
    /// no-credential path byte-identical to not calling this at all.
    #[test]
    fn empty_config_leaves_command_untouched() {
        let mut cmd = Command::new("git");
        GitConfigEnv::new().apply(&mut cmd);
        assert_eq!(cmd.get_envs().count(), 0);
    }
}
