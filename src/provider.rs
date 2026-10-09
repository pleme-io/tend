use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::reach::{Denial, DiscoveryAnswer, Freshness};

/// The GitHub REST API root discovery talks to.
pub(crate) const GITHUB_API: &str = "https://api.github.com";

/// A REST client authenticated by the configured `github_auth` chain
/// (unauthenticated when no source yields a token — the public floor).
fn github_client() -> Result<todoku::GitHubClient, todoku::TodokuError> {
    crate::gh_auth::credentials().todoku_client(GITHUB_API)
}

/// Cached wrapper around `discover_github_repos`.
/// Returns cached results if fresh (within TTL); otherwise hits the API and writes cache.
/// Pass `refresh = true` to bypass the cache and always hit the API.
pub async fn discover_github_repos_cached(org: &str, refresh: bool) -> Result<Vec<String>> {
    if !refresh {
        if let Some(repos) = cache::read(org) {
            return Ok(repos);
        }
    }

    let repos = discover_github_repos(org).await?;
    let _ = cache::write(org, &repos); // best-effort cache write
    Ok(repos)
}

/// Discovery that answers instead of throwing, with cache-backed recovery.
///
/// ── ★ THE SHAPE, AND WHY IT IS NOT A `Result` ───────────────────────────
/// [`discover_github_repos_cached`] returns `Result<Vec<String>>`, so every
/// failure — a revoked credential, a throttle, DNS down, an org that does
/// not exist — arrives as one opaque `anyhow::Error`, and the six CLI call
/// sites turn it into an abort. That is how one unreadable org took down
/// discovery for four readable ones.
///
/// This returns a [`DiscoveryAnswer`], which forces the caller to
/// distinguish *nothing there* from *could not look*. See `reach`'s module
/// docs for why blindness is represented rather than swallowed or thrown.
///
/// ── ★ RECOVERY POLICY ───────────────────────────────────────────────────
/// On a **transient** denial (5xx, 429, timeout, transport) we fall back to
/// the discovery cache *past its TTL* and return `Found` marked
/// [`Freshness::Stale`] with a measured age. On a **non-transient** one
/// (401/403-permission) we do not: a revoked credential is a state change,
/// and quietly serving yesterday's list would hide it behind an answer that
/// looks fine. `Denial::is_transient` is that gate and it is unit-tested.
///
/// No retry loop here — todoku already retries 429/5xx honouring
/// `Retry-After`, so wrapping it would quadruple our request rate against
/// a forge that is already throttling us.
pub async fn discover_answered(org: &str, refresh: bool) -> DiscoveryAnswer<Vec<String>> {
    if !refresh {
        if let Some(repos) = cache::read(org) {
            return DiscoveryAnswer::Found {
                value: repos,
                freshness: Freshness::Live,
            };
        }
    }

    match discover_github_repos_classified(org).await {
        Ok(names) => {
            let _ = cache::write(org, &names); // best-effort
            if names.is_empty() {
                // A real, observed absence — the org exists and holds no
                // non-archived repos. A finding, not a failure.
                DiscoveryAnswer::Empty {
                    of: format!("`{org}` — no non-archived repositories"),
                }
            } else {
                DiscoveryAnswer::Found {
                    value: names,
                    freshness: Freshness::Live,
                }
            }
        }
        Err(denial) => {
            if denial.is_transient() {
                if let Some((repos, age_secs)) = cache::read_stale(org) {
                    let because = denial
                        .clone()
                        .into_answer::<()>()
                        .because()
                        .unwrap_or("unknown")
                        .to_owned();
                    return DiscoveryAnswer::Found {
                        value: repos,
                        freshness: Freshness::Stale { age_secs, because },
                    };
                }
            }
            denial.into_answer()
        }
    }
}

/// `discover_github_repos` with its error taxonomy intact.
///
/// Identical logic to [`discover_github_repos`] — org endpoint, then user
/// endpoint on 404 — but every failure is classified by
/// [`crate::reach::classify`] instead of being flattened into `anyhow`.
async fn discover_github_repos_classified(org: &str) -> Result<Vec<String>, Denial> {
    // Building the client failed, so we never asked anything.
    let client = github_client().map_err(|e| crate::reach::classify(org, &e))?;
    discover_with(&client, org).await
}

/// Discovery against an explicit client — the seam the private-repo test
/// drives against a local mock of the GitHub API.
pub(crate) async fn discover_with(
    client: &todoku::GitHubClient,
    org: &str,
) -> Result<Vec<String>, Denial> {
    use todoku::{GitHubApi, OwnerType};

    match client.list_repos(org, OwnerType::Org).await {
        Ok(repos) => return Ok(live_names(repos)),
        Err(todoku::TodokuError::Http { status: 404, .. }) => {
            // Not an org. Fall through and try the user endpoint — a 404
            // here is not yet evidence of absence.
        }
        Err(e) => return Err(crate::reach::classify(org, &e)),
    }

    match client.list_repos(org, OwnerType::User).await {
        Ok(repos) => Ok(live_names(repos)),
        // Both endpoints 404: now it is genuinely absent, and `classify`
        // maps that to `Empty` rather than to a failure.
        Err(e) => Err(crate::reach::classify(org, &e)),
    }
}

/// Non-archived repo names, sorted. Shared by both endpoint arms so the
/// two cannot drift in what they filter.
fn live_names(repos: Vec<todoku::GitHubRepo>) -> Vec<String> {
    let mut names: Vec<String> = repos
        .into_iter()
        .filter(|r| !r.archived)
        .map(|r| r.name)
        .collect();
    names.sort();
    names
}

/// Per-repo extended state. M4 surface: extends the name-only
/// discovery output with the GitHub-side fields the substrate cares
/// about (default branch, archived, fork, primary language). This is
/// the typed input for richer drift detection (e.g. an archived repo
/// still cloned locally is drift).
///
/// Excluded fields: `topics` would need a separate per-repo API call
/// and isn't yet in `todoku::GitHubRepo`. Adding it lands in a later
/// chunk if a consumer needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoState {
    pub name: String,
    pub default_branch: Option<String>,
    pub archived: bool,
    pub fork: bool,
    pub language: Option<String>,
}

impl RepoState {
    /// Lossy projection back to the legacy name-only surface. Used
    /// by call sites that still consume `Vec<String>` (the daemon's
    /// resolve_repos → reconcile path) while the consumer-by-
    /// consumer migration to `RepoState` is in flight.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Cached wrapper around `discover_github_repo_states`. Same TTL +
/// XDG-cache semantics as `cache::DiscoveryCache`, but stores the
/// richer `Vec<RepoState>` under a separate filename so it can
/// coexist with the legacy Vec<String> cache without schema
/// conflicts. Pass `refresh = true` to bypass the cache.
pub async fn discover_github_repo_states_cached(
    org: &str,
    refresh: bool,
) -> Result<Vec<RepoState>> {
    if !refresh {
        if let Some(repos) = cache::read_rich(org) {
            return Ok(repos);
        }
    }
    let states = discover_github_repo_states(org).await?;
    let _ = cache::write_rich(org, &states); // best-effort
    Ok(states)
}

/// Discover repos for an org/user as `RepoState` values. Same
/// org-endpoint-then-user-endpoint fallback as `discover_github_repos`;
/// preserves the archived-repos-excluded filter so consumers don't
/// have to repeat it.
///
/// Cost: same as `discover_github_repos` — one or two REST calls per
/// org. No per-repo follow-up (topics intentionally excluded; see
/// `RepoState`).
pub async fn discover_github_repo_states(org: &str) -> Result<Vec<RepoState>> {
    use todoku::{GitHubApi, OwnerType};

    let client = github_client().context("building GitHub client")?;

    let raw = match client.list_repos(org, OwnerType::Org).await {
        Ok(r) => r,
        Err(todoku::TodokuError::Http { status: 404, .. }) => client
            .list_repos(org, OwnerType::User)
            .await
            .context("fetching user repos")?,
        Err(e) => return Err(anyhow::Error::from(e).context("fetching org repos")),
    };

    let mut states: Vec<RepoState> = raw
        .into_iter()
        .filter(|r| !r.archived)
        .map(|r| RepoState {
            name: r.name,
            default_branch: r.default_branch,
            archived: r.archived,
            fork: r.fork,
            language: r.language,
        })
        .collect();
    states.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(states)
}

/// Discover all repos in a GitHub org or user account via REST API.
/// Tries the /orgs endpoint first; falls back to /users on 404.
/// Authenticated by the configured `github_auth` chain (optional, but needed
/// for private repos).
pub async fn discover_github_repos(org: &str) -> Result<Vec<String>> {
    use todoku::{GitHubApi, OwnerType};

    let client = github_client().context("building GitHub client")?;

    // Try org endpoint first, then user endpoint on 404
    match client.list_repos(org, OwnerType::Org).await {
        Ok(repos) => {
            let mut names: Vec<String> = repos
                .into_iter()
                .filter(|r| !r.archived)
                .map(|r| r.name)
                .collect();
            names.sort();
            return Ok(names);
        }
        Err(todoku::TodokuError::Http { status: 404, .. }) => {
            // org endpoint returned 404, try user endpoint
        }
        Err(e) => return Err(anyhow::Error::from(e).context("fetching org repos")),
    }

    let repos = client
        .list_repos(org, OwnerType::User)
        .await
        .context("fetching user repos")?;
    let mut names: Vec<String> = repos
        .into_iter()
        .filter(|r| !r.archived)
        .map(|r| r.name)
        .collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_state_serde_roundtrip() {
        let state = RepoState {
            name: "tend".into(),
            default_branch: Some("main".into()),
            archived: false,
            fork: false,
            language: Some("Rust".into()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: RepoState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn repo_state_name_accessor() {
        let state = RepoState {
            name: "shigoto".into(),
            default_branch: None,
            archived: true,
            fork: false,
            language: None,
        };
        assert_eq!(state.name(), "shigoto");
    }

    /// The default credential chain keeps the old precedence —
    /// `TEND_GITHUB_TOKEN` before `GITHUB_TOKEN`, so an operator can override
    /// an ambient CI token without unsetting it — and gains `GH_TOKEN` and
    /// `gh auth token` before the old file fallback. Asserted on the config
    /// value, not by mutating the process env.
    #[test]
    fn default_github_auth_chain_order() {
        let chain = serde_json::to_value(crate::config::GithubAuthSources::default()).unwrap();
        assert_eq!(
            chain,
            serde_json::json!([
                { "token": { "env": "TEND_GITHUB_TOKEN" } },
                { "token": { "env": "GITHUB_TOKEN" } },
                { "token": { "env": "GH_TOKEN" } },
                { "gh_cli": { "host": "github.com" } },
                { "token": { "file": "~/.config/github/token" } },
            ])
        );
    }

    /// A one-request-per-connection mock of the GitHub REST API: answers
    /// `/orgs/<org>/repos` with one PRIVATE repo, but only to a request
    /// carrying `Authorization: Bearer <token>` — exactly what GitHub does
    /// (an unauthenticated list omits private repos). Returns the base URL.
    async fn mock_github(token: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf[n..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(k) => n += k,
                    }
                }
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let authed = req.contains(&format!(
                    "authorization: bearer {}",
                    token.to_ascii_lowercase()
                ));
                let body = if authed {
                    r#"[{"name":"private-repo","archived":false},{"name":"public-repo"}]"#
                } else {
                    r#"[{"name":"public-repo"}]"#
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    /// Private discovery goes through the resolver: the second chain
    /// element yields the token, the client sends it, and the private repo
    /// is listed. With no source yielding, the same mock lists only the
    /// public one — discovery degrades to the public floor, it does not fail.
    #[tokio::test]
    async fn private_discovery_uses_the_github_auth_chain() {
        use crate::gh_auth::tests::{creds, literal, TOKEN};
        use shikumi::secret::{SecretBackend, SecretSource};

        let base = mock_github(TOKEN).await;
        let unset = shikumi::github::GithubAuth::Token(SecretSource::Backend(SecretBackend::Env(
            "TEND_TEST_DEFINITELY_UNSET_VAR_d41d".into(),
        )));

        let authed = creds(&[unset.clone(), literal(TOKEN)]);
        let client = authed.todoku_client(&base).unwrap();
        let names = discover_with(&client, "pleme-io").await.unwrap();
        assert_eq!(names, vec!["private-repo", "public-repo"]);

        let anonymous = creds(&[unset]);
        let client = anonymous.todoku_client(&base).unwrap();
        let names = discover_with(&client, "pleme-io").await.unwrap();
        assert_eq!(names, vec!["public-repo"]);
    }
}
