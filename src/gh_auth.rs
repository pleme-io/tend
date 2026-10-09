//! The one GitHub credential handle.
//!
//! # What this replaced
//!
//! Before this module tend resolved its GitHub credential four ways that did
//! not agree: `provider::github_token()` (`TEND_GITHUB_TOKEN` →
//! `GITHUB_TOKEN` → `~/.config/github/token`), `sync::github_clone_auth`
//! (env only, so a clone could fail where discovery had just succeeded),
//! `--github-token-file` (which `set_var("GITHUB_TOKEN")`d the process
//! environment, and existed on only two subcommands), and the daemon's
//! SIGHUP handler (which re-read that file back into the environment). None
//! of them could hold a GitHub App, `gh auth token`, or any secret backend.
//!
//! # The shape now
//!
//! The credential is CONFIG: `github_auth:` in tend's config (see
//! [`crate::config::GithubAuthSources`]), an ordered list of shikumi
//! [`GithubAuth`] sources whose default is shikumi's
//! [`GithubAuth::default_chain`]`("tend")` — `TEND_GITHUB_TOKEN`,
//! `GITHUB_TOKEN`, `GH_TOKEN`, `gh auth token`, `~/.config/github/token`.
//! Every flag that names a credential (`--github-token-file`,
//! `--github-app-*`) is a partial over that config, so a credential reaches
//! tend through the same fold as every other setting.
//!
//! [`install`] is called by the one config load path
//! ([`crate::config_layers::ConfigLoader::load`]) with the resolved sources;
//! every consumer asks [`token`]. Resolution is shikumi's
//! [`GithubAuthResolver`], held for the life of the process so GitHub App
//! installation tokens are cached and refreshed before expiry rather than
//! minted per request.
//!
//! # Rendering
//!
//! A [`GithubToken`] is handed to a consumer only through one of its
//! renderers: `authorization_header()`/`expose_for_header()` for REST,
//! [`git_auth`] (an `http.https://github.com/.extraheader` scoped to ONE
//! child process via [`GitConfigEnv`]) for git, `nix_access_tokens_line()`
//! for nix. Its `Debug`/`Display` redact. The provenance (which chain element
//! answered) is written to the audit log once per cycle, never the value.
//!
//! # Tier
//!
//! The resolution order is typed (a `Vec<GithubAuth>` folded like any other
//! config value) — parse-time-rejected for malformed sources. Whether a
//! source actually yields a token is a runtime fact, so "no credential" is
//! still only-mitigated: it is reported (audit log + `Degradations` at the
//! discovery sites), not made unrepresentable.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use shikumi::github::{GithubAuth, GithubAuthResolver, GithubToken};

use crate::secret::GitConfigEnv;

/// How long a resolved token is reused before the chain is walked again.
///
/// The chain is cheap for env and file sources but `gh auth token` is a
/// process spawn, and a clone loop over hundreds of repos would otherwise
/// spawn it hundreds of times. Short enough that a rotated file or env
/// source is picked up within one daemon cycle; App tokens additionally
/// carry their own expiry, honoured below.
const MEMO_TTL: Duration = Duration::from_secs(60);

/// A memoized token is dropped this long before its own expiry, so a request
/// is never sent with a token that dies in flight.
const EXPIRY_MARGIN: Duration = Duration::from_secs(120);

/// The GitHub credential for this process: the configured source chain plus
/// the resolver that answers it.
pub(crate) struct GithubCredentials {
    auth: GithubAuth,
    resolver: Arc<GithubAuthResolver>,
    memo: Mutex<Memo>,
    audit: crate::audit::AuditLog,
}

#[derive(Default)]
struct Memo {
    token: Option<(GithubToken, Instant)>,
    /// Provenance last written to the audit log — `None` re-arms the log.
    logged: Option<String>,
}

impl GithubCredentials {
    /// Credentials over `sources`, resolved by `resolver`.
    ///
    /// Several sources become a [`GithubAuth::Chain`]; one is used as is.
    pub(crate) fn new(sources: &[GithubAuth], resolver: Arc<GithubAuthResolver>) -> Self {
        let auth = match sources {
            [one] => one.clone(),
            many => GithubAuth::Chain(many.to_vec()),
        };
        Self {
            auth,
            resolver,
            memo: Mutex::new(Memo::default()),
            audit: crate::audit::AuditLog::default_path(),
        }
    }

    /// Write the provenance records to `audit` instead of the default log.
    #[cfg(test)]
    pub(crate) fn with_audit(mut self, audit: crate::audit::AuditLog) -> Self {
        self.audit = audit;
        self
    }

    /// The source chain these credentials resolve.
    pub(crate) fn auth(&self) -> &GithubAuth {
        &self.auth
    }

    /// The current token, or `None` when no source in the chain yields one.
    ///
    /// Blocking (a `gh` spawn, or HTTPS to mint an App token) — and safe to
    /// call from async code: resolution runs on a scoped OS thread, because
    /// shikumi's App path uses `reqwest::blocking`, which panics when driven
    /// from inside a tokio runtime thread.
    pub(crate) fn token(&self) -> Option<GithubToken> {
        let mut memo = self.memo.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((token, at)) = &memo.token {
            if at.elapsed() < MEMO_TTL && !expiring(token) {
                return Some(token.clone());
            }
        }
        let outcome = std::thread::scope(|s| {
            s.spawn(|| self.resolver.resolve(&self.auth))
                .join()
                .unwrap_or_else(|_| {
                    Err(shikumi::github::GithubAuthError::Chain {
                        failures: Vec::new(),
                    })
                })
        });
        match outcome {
            Ok(token) => {
                let source = token.provenance().to_string();
                if memo.logged.as_deref() != Some(source.as_str()) {
                    self.audit.log(
                        "github_auth_resolved",
                        serde_json::json!({
                            "source": source,
                            "kind": token.provenance().kind.as_str(),
                            "chain_path": token.provenance().chain_path,
                        }),
                    );
                    memo.logged = Some(source);
                }
                memo.token = Some((token.clone(), Instant::now()));
                Some(token)
            }
            Err(error) => {
                memo.token = None;
                let message = error.to_string();
                if memo.logged.as_deref() != Some(message.as_str()) {
                    self.audit.log(
                        "github_auth_unresolved",
                        serde_json::json!({ "error": message }),
                    );
                    memo.logged = Some(message);
                }
                None
            }
        }
    }

    /// Forget the memoized token and re-arm the provenance log — the next
    /// [`Self::token`] walks the chain again. Called at the start of every
    /// daemon cycle and on SIGHUP, so a rotated credential is picked up and
    /// the source in force is recorded once per reconcile.
    pub(crate) fn begin_cycle(&self) {
        let mut memo = self.memo.lock().unwrap_or_else(PoisonError::into_inner);
        memo.token = None;
        memo.logged = None;
    }

    /// A todoku GitHub REST client against `base_url`, authenticated with
    /// the current token when there is one (unauthenticated otherwise — the
    /// public-repo floor, which is what tend has always done).
    pub(crate) fn todoku_client(
        &self,
        base_url: &str,
    ) -> Result<todoku::GitHubClient, todoku::TodokuError> {
        let mut builder = todoku::HttpClient::builder()
            .base_url(base_url)
            .header(reqwest::header::ACCEPT, "application/vnd.github.v3+json");
        if let Some(token) = self.token() {
            builder = builder.auth(todoku::BearerToken::new(token.expose_for_header()));
        }
        Ok(todoku::GitHubClient::from_client(builder.build()?))
    }
}

fn expiring(token: &GithubToken) -> bool {
    token
        .expires_at()
        .is_some_and(|at| at <= SystemTime::now() + EXPIRY_MARGIN)
}

/// The process resolver. One for the life of the process: it holds the App
/// installation-token cache, and it owns a `reqwest::blocking::Client`,
/// which must never be dropped on a runtime thread — a static is never
/// dropped at all.
fn process_resolver() -> Arc<GithubAuthResolver> {
    static RESOLVER: OnceLock<Arc<GithubAuthResolver>> = OnceLock::new();
    Arc::clone(RESOLVER.get_or_init(|| Arc::new(GithubAuthResolver::new())))
}

fn slot() -> &'static Mutex<Option<Arc<GithubCredentials>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<GithubCredentials>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install the credential chain from a freshly resolved config.
///
/// Re-installing the SAME chain keeps the existing handle (and its memo);
/// a changed chain replaces it, so a config edit takes effect on the next
/// [`token`] without a restart.
pub(crate) fn install(sources: &[GithubAuth]) {
    let mut guard = slot().lock().unwrap_or_else(PoisonError::into_inner);
    let fresh = GithubCredentials::new(sources, process_resolver());
    let unchanged = guard
        .as_ref()
        .is_some_and(|cur| same_auth(cur.auth(), fresh.auth()));
    if !unchanged {
        *guard = Some(Arc::new(fresh));
    }
}

fn same_auth(a: &GithubAuth, b: &GithubAuth) -> bool {
    // GithubAuth has no PartialEq; its serialization is total and canonical.
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// The installed credentials — or, before any config was loaded (the
/// operator's controllers, a unit test), shikumi's default chain for tend.
pub(crate) fn credentials() -> Arc<GithubCredentials> {
    let mut guard = slot().lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(guard.get_or_insert_with(|| {
        Arc::new(GithubCredentials::new(
            &crate::config::GithubAuthSources::default().0,
            process_resolver(),
        ))
    }))
}

/// The current GitHub token from the installed chain.
pub(crate) fn token() -> Option<GithubToken> {
    credentials().token()
}

/// Git config authenticating `https://github.com/` for ONE child process.
///
/// shikumi's `git_extraheader()` renderer, carried by tend's
/// [`GitConfigEnv`] so it composes with other per-process config (committer
/// identity) and never reaches argv or `.git/config`. Basic, not bearer —
/// see [`GitConfigEnv`]'s history for why bearer silently fails for git.
pub(crate) fn git_auth(token: &GithubToken) -> GitConfigEnv {
    GitConfigEnv::new().with(
        "http.https://github.com/.extraheader",
        token.git_extraheader(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use shikumi::secret::{SecretBackend, SecretSource};

    pub(crate) const TOKEN: &str = "ghp_TENDTESTTOKEN0000000000000000000";

    pub(crate) fn literal(value: &str) -> GithubAuth {
        GithubAuth::Token(SecretSource::Backend(SecretBackend::Literal(value.into())))
    }

    /// Credentials whose audit records go to a throwaway file, never the
    /// operator's real log.
    pub(crate) fn creds(sources: &[GithubAuth]) -> GithubCredentials {
        let sink = std::env::temp_dir().join(format!(
            "tend-gh-auth-test-{}-{}.jsonl",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        GithubCredentials::new(sources, Arc::new(GithubAuthResolver::new()))
            .with_audit(crate::audit::AuditLog::new(sink))
    }

    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[test]
    fn a_chain_answers_from_the_first_source_that_yields() {
        let c = creds(&[
            GithubAuth::Token(SecretSource::Backend(SecretBackend::Env(
                "TEND_TEST_DEFINITELY_UNSET_VAR_7f3a".into(),
            ))),
            literal(TOKEN),
        ]);
        let token = c.token().expect("second source yields");
        assert_eq!(token.expose_for_header(), TOKEN);
        assert_eq!(token.provenance().chain_path, vec![1]);
    }

    #[test]
    fn no_source_yields_none_rather_than_an_empty_credential() {
        let c = creds(&[GithubAuth::Token(SecretSource::Backend(
            SecretBackend::Env("TEND_TEST_DEFINITELY_UNSET_VAR_9c1b".into()),
        ))]);
        assert!(c.token().is_none());
    }

    #[test]
    fn token_resolution_is_safe_inside_a_tokio_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let got = rt.block_on(async { creds(&[literal(TOKEN)]).token() });
        assert!(got.is_some());
    }

    #[test]
    fn git_auth_is_a_basic_extraheader_scoped_to_github() {
        let token = creds(&[literal(TOKEN)]).token().unwrap();
        let pairs = git_auth(&token).env_pairs();
        assert_eq!(pairs[1].1, "http.https://github.com/.extraheader");
        assert!(pairs[2].1.starts_with("AUTHORIZATION: basic "));
        assert!(
            !pairs[2].1.contains(TOKEN),
            "the header is base64, never raw"
        );
    }

    /// No token in `Debug`/`Display`.
    #[test]
    fn debug_and_display_never_reveal_the_token() {
        let token = creds(&[literal(TOKEN)]).token().unwrap();
        assert!(!format!("{token:?}").contains(TOKEN));
        assert!(!format!("{token}").contains(TOKEN));
    }

    /// The provenance is logged once per cycle — WHICH source answered —
    /// and the token never is.
    #[test]
    fn provenance_is_logged_once_per_cycle_and_never_the_token() {
        let sink = std::env::temp_dir().join(format!(
            "tend-gh-auth-provenance-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sink);
        let c = GithubCredentials::new(&[literal(TOKEN)], Arc::new(GithubAuthResolver::new()))
            .with_audit(crate::audit::AuditLog::new(sink.clone()));
        c.token();
        c.token();
        let log = std::fs::read_to_string(&sink).unwrap();
        assert_eq!(log.matches("github_auth_resolved").count(), 1, "{log}");
        c.begin_cycle();
        c.token();
        let log = std::fs::read_to_string(&sink).unwrap();
        assert_eq!(log.matches("github_auth_resolved").count(), 2, "{log}");
        assert!(
            log.contains("token from literal"),
            "names the source: {log}"
        );
        assert!(!log.contains(TOKEN), "the token reached the log: {log}");
        let _ = std::fs::remove_file(&sink);
    }
}
