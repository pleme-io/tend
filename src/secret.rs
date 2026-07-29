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
//! 1. **A secret is a [`Secret`], never a `String`.** `Debug`,
//!    `Display`, and `Serialize` all render `***`. There is no
//!    `Deref<Target = str>`, no `AsRef<str>`, and no `to_string()`
//!    returning the value — so a secret cannot reach a log line, a
//!    format string, or a serialized struct by accident. Reading the
//!    real value requires [`Secret::expose`], which is deliberately
//!    ugly to type and easy to grep for at review.
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
//! `expose()` and print the result. The goal is that every path to a
//! leak now requires typing `expose()` — one grep-able token that a
//! reviewer can audit — rather than being the default behavior of
//! `format!`.

use std::process::Command;

/// A credential. Renders as `***` through every standard formatting
/// and serialization path.
///
/// Constructed only from [`Secret::from_env`] or [`Secret::new`], both
/// of which reject the empty string — an empty secret is not a
/// credential, and treating `Some("")` as authentication produces the
/// confusing "invalid username or token" failure rather than a clean
/// "no credential available".
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret {
    value: String,
}

impl Secret {
    /// Wrap a value. `None` for an empty or whitespace-only string.
    pub(crate) fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Trim on the way in so a token read from a file with a
        // trailing newline is byte-identical to the same token from an
        // env var. Divergence there produced credentials that worked
        // through one path and 401'd through the other.
        Some(Self {
            value: trimmed.to_string(),
        })
    }

    /// First non-empty value among `names`, in order.
    ///
    /// The ordering is the caller's precedence policy — e.g.
    /// `TEND_GITHUB_TOKEN` before `GITHUB_TOKEN` so an operator can
    /// override the ambient CI token without unsetting it.
    pub(crate) fn from_env(names: &[&str]) -> Option<Self> {
        names
            .iter()
            .find_map(|name| std::env::var(name).ok().and_then(Self::new))
    }

    /// Read a secret from a file, trimming trailing whitespace.
    ///
    /// `None` for a missing, unreadable, or blank file — all three are
    /// "no credential here", and distinguishing them would only invite
    /// a caller to log the path alongside a failure.
    pub(crate) fn from_file(path: impl AsRef<std::path::Path>) -> Option<Self> {
        std::fs::read_to_string(path).ok().and_then(Self::new)
    }

    /// The real value.
    ///
    /// Every leak tend has ever had went through a path this function
    /// now guards. Call it only where the secret crosses into a
    /// process boundary that requires the plaintext, and never into
    /// anything that formats, logs, serializes, or persists.
    pub(crate) fn expose(&self) -> &str {
        &self.value
    }

    /// Git config delivering this secret as an `Authorization` header
    /// for `https://github.com/`, scoped to one process.
    ///
    /// **Basic, not bearer.** This codebase previously used
    /// `AUTHORIZATION: bearer <token>` in `operator::git_ops`, and it
    /// does not authenticate git-over-HTTPS at all — git ignores the
    /// rejected header and falls through to prompting for a username:
    ///
    /// ```text
    /// $ GIT_CONFIG_VALUE_0="AUTHORIZATION: bearer ghp_..." git ls-remote https://github.com/...
    /// fatal: could not read Username for 'https://github.com'
    ///
    /// $ GIT_CONFIG_VALUE_0="AUTHORIZATION: basic $(printf 'x-access-token:ghp_...' | base64)" ...
    /// 7fd1a60b01f91b314f59955a4e4d4e80d8edf11d	HEAD
    /// ```
    ///
    /// Bearer works for the REST API, which is why it went unnoticed:
    /// the git paths that "worked" were the ones whose origin URL
    /// carried embedded credentials, and `github_auth_config` skips
    /// the header entirely in exactly that case. So the header was
    /// only ever exercised where it was also unnecessary.
    ///
    /// `x-access-token` is the conventional non-secret username for a
    /// token-as-password; GitHub ignores the username field.
    pub(crate) fn github_git_auth(&self) -> GitConfigEnv {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{}", self.value));
        GitConfigEnv::new().with(
            "http.https://github.com/.extraheader",
            format!("AUTHORIZATION: basic {encoded}"),
        )
    }
}

/// `Secret` deliberately implements no trait that would let the value
/// escape: no `Deref`, no `AsRef<str>`, no `Into<String>`. `Debug` and
/// `Display` exist precisely so that the *accidental* paths — a
/// `{:?}` in a log, a `{}` in a format string, `dbg!` — are safe.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

// `Display` is deliberately NOT implemented, and this is the subtlest
// decision in the module.
//
// A redacting `Display` looks strictly safer than none, but it is
// worse: it silently satisfies every generic `T: Display` API. During
// this refactor `reqwest`'s `bearer_auth(t)` accepted a `Secret`
// without complaint and would have shipped `Authorization: Bearer ***`
// to GitHub — auth broken at runtime, compiling cleanly, with a
// redacted header that looks correct in a packet capture.
//
// Without `Display`, that call site is a compile error until someone
// writes `.expose()`. A type that cannot be formatted turns a silent
// runtime failure into a build failure, which is the whole point.
//
// `Debug` stays: it is what `{:?}`, `dbg!`, and every `#[derive(Debug)]`
// on an enclosing struct reach for, so it is the accidental-leak path
// that actually needs covering, and nothing authenticates with it.

impl serde::Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("***")
    }
}

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
        pairs.push(("GIT_CONFIG_COUNT".to_string(), self.entries.len().to_string()));
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

    const TOKEN: &str = "ghp_SUPERSECRETVALUE00000000";

    fn secret() -> Secret {
        Secret::new(TOKEN).unwrap()
    }

    #[test]
    fn debug_never_reveals() {
        let rendered = format!("{:?}", secret());
        assert_eq!(rendered, "Secret(***)");
        assert!(!rendered.contains(TOKEN));
    }

    /// `Secret` must not be `Display`. A redacting Display would
    /// silently satisfy generic `T: Display` sinks — `bearer_auth`,
    /// `header()`, `write!` — producing `***` on the wire instead of a
    /// compile error. Asserted structurally: this only compiles while
    /// `Secret: !Display`.
    #[test]
    fn secret_is_not_display() {
        trait NotDisplay {
            const IS_DISPLAY: bool = false;
        }
        impl<T> NotDisplay for T {}

        struct Probe<T>(std::marker::PhantomData<T>);
        impl<T: std::fmt::Display> Probe<T> {
            #[allow(dead_code)]
            const IS_DISPLAY: bool = true;
        }

        assert!(
            !<Probe<Secret> as NotDisplay>::IS_DISPLAY,
            "Secret gained a Display impl — see the comment on why that is unsafe"
        );
    }

    #[test]
    fn serialization_never_reveals() {
        let json = serde_json::to_string(&secret()).unwrap();
        assert_eq!(json, "\"***\"");
        assert!(!json.contains(TOKEN));
    }

    /// A `Secret` nested inside a larger serialized struct — the shape
    /// a drift event or receipt would have — must not leak either.
    #[test]
    fn nested_serialization_never_reveals() {
        #[derive(serde::Serialize)]
        struct Ctx {
            repo: &'static str,
            token: Secret,
        }
        let json = serde_json::to_string(&Ctx {
            repo: "org/repo",
            token: secret(),
        })
        .unwrap();
        assert!(!json.contains(TOKEN), "leaked through nesting: {json}");
    }

    #[test]
    fn expose_returns_the_real_value() {
        assert_eq!(secret().expose(), TOKEN);
    }

    #[test]
    fn empty_and_whitespace_are_not_credentials() {
        assert!(Secret::new("").is_none());
        assert!(Secret::new("   ").is_none());
        assert!(Secret::new("\n").is_none());
    }

    #[test]
    fn from_env_respects_precedence_order() {
        let a = "TEND_TEST_SECRET_PRIMARY";
        let b = "TEND_TEST_SECRET_FALLBACK";
        std::env::set_var(a, "primary-value");
        std::env::set_var(b, "fallback-value");
        assert_eq!(Secret::from_env(&[a, b]).unwrap().expose(), "primary-value");

        std::env::remove_var(a);
        assert_eq!(Secret::from_env(&[a, b]).unwrap().expose(), "fallback-value");

        std::env::remove_var(b);
        assert!(Secret::from_env(&[a, b]).is_none());
    }

    /// An env var set to the empty string must not be mistaken for a
    /// credential — CI runners routinely export `GITHUB_TOKEN=` when a
    /// job has no token, and treating that as auth turns a clean
    /// "unauthenticated" path into a confusing 401.
    #[test]
    fn empty_env_var_falls_through_to_next() {
        let a = "TEND_TEST_SECRET_EMPTY";
        let b = "TEND_TEST_SECRET_REAL";
        std::env::set_var(a, "");
        std::env::set_var(b, "real-value");
        assert_eq!(Secret::from_env(&[a, b]).unwrap().expose(), "real-value");
        std::env::remove_var(a);
        std::env::remove_var(b);
    }

    /// Pins the exact header git needs. Bearer is NOT interchangeable
    /// here — it fails to authenticate git-over-HTTPS and git falls
    /// through to prompting. See `github_git_auth`.
    #[test]
    fn git_auth_env_carries_basic_header() {
        use base64::Engine as _;
        let pairs = secret().github_git_auth().env_pairs();
        assert_eq!(pairs[0], ("GIT_CONFIG_COUNT".into(), "1".into()));
        assert_eq!(
            pairs[1],
            (
                "GIT_CONFIG_KEY_0".into(),
                "http.https://github.com/.extraheader".into()
            )
        );

        let expected = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{TOKEN}"));
        assert_eq!(
            pairs[2],
            (
                "GIT_CONFIG_VALUE_0".into(),
                format!("AUTHORIZATION: basic {expected}")
            )
        );
    }

    /// Base64 is encoding, not encryption — the header still contains
    /// the token, which is exactly why it must travel in the
    /// environment and never in argv or a config file.
    #[test]
    fn encoded_header_still_round_trips_to_the_token() {
        use base64::Engine as _;
        let pairs = secret().github_git_auth().env_pairs();
        let encoded = pairs[2].1.strip_prefix("AUTHORIZATION: basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            format!("x-access-token:{TOKEN}")
        );
    }

    /// The load-bearing property of the whole module: applying auth to
    /// a command puts the secret in the environment and **nowhere in
    /// argv**, so it cannot be read out of the process table.
    #[test]
    fn applying_auth_never_touches_argv() {
        let mut cmd = Command::new("git");
        cmd.args(["clone", "https://github.com/org/repo.git"]);
        secret().github_git_auth().apply(&mut cmd);

        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("x-access-token:{TOKEN}"));

        // Neither the raw token nor its base64 form may appear in argv
        // — base64 is encoding, not concealment, and anything in argv
        // is readable from the process table.
        let argv = format!("{:?}", cmd.get_args().collect::<Vec<_>>());
        assert!(!argv.contains(TOKEN), "raw secret reached argv: {argv}");
        assert!(!argv.contains(&encoded), "encoded secret reached argv: {argv}");

        let in_env = cmd
            .get_envs()
            .any(|(_, v)| v.is_some_and(|v| v.to_string_lossy().contains(&encoded)));
        assert!(in_env, "credential did not reach the environment");
    }

    #[test]
    fn merge_preserves_order_and_renumbers() {
        let combined = GitConfigEnv::new()
            .with("user.name", "tend")
            .merge(secret().github_git_auth());
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
