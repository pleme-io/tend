use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache;
use crate::secret::Secret;

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

    let token = github_token();
    let client = todoku::GitHubClient::new(token.as_ref().map(Secret::expose))
        .context("building GitHub client")?;

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
/// Uses TEND_GITHUB_TOKEN or GITHUB_TOKEN env var for auth (optional but needed for private repos).
pub async fn discover_github_repos(org: &str) -> Result<Vec<String>> {
    use todoku::{GitHubApi, OwnerType};

    let token = github_token();
    let client = todoku::GitHubClient::new(token.as_ref().map(Secret::expose))
        .context("building GitHub client")?;

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

/// The GitHub credential, from the environment or the on-disk
/// fallback. **The single source of truth** — every path that
/// authenticates to GitHub resolves it here.
///
/// It was previously three functions: this one (env only),
/// `operator::load_github_token` (env plus `~/.config/github/token`),
/// and `operator::throttle::load_github_token` (env only, but
/// trimming where the others did not). They disagreed on precedence,
/// on whether an empty value counted, and on trimming — so the same
/// deployment could authenticate through one code path and 401 through
/// another. Consolidated here; the file fallback is preserved.
///
/// Precedence: `TEND_GITHUB_TOKEN`, then `GITHUB_TOKEN`, then
/// `~/.config/github/token`. The tend-specific variable wins so an
/// operator can override an ambient CI token without unsetting it.
#[must_use]
pub fn github_token() -> Option<Secret> {
    if let Some(secret) = Secret::from_env(&["TEND_GITHUB_TOKEN", "GITHUB_TOKEN"]) {
        return Some(secret);
    }
    dirs::home_dir().and_then(|home| Secret::from_file(home.join(".config/github/token")))
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


    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_github_token_prefers_tend_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let orig_tend = std::env::var("TEND_GITHUB_TOKEN").ok();
        let orig_gh = std::env::var("GITHUB_TOKEN").ok();

        std::env::set_var("TEND_GITHUB_TOKEN", "tend-token-123");
        std::env::set_var("GITHUB_TOKEN", "gh-token-456");
        assert_eq!(github_token().unwrap().expose(), "tend-token-123");

        // Restore
        match orig_tend {
            Some(v) => std::env::set_var("TEND_GITHUB_TOKEN", v),
            None => std::env::remove_var("TEND_GITHUB_TOKEN"),
        }
        match orig_gh {
            Some(v) => std::env::set_var("GITHUB_TOKEN", v),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    #[test]
    fn test_github_token_falls_back_to_github_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let orig_tend = std::env::var("TEND_GITHUB_TOKEN").ok();
        let orig_gh = std::env::var("GITHUB_TOKEN").ok();

        std::env::remove_var("TEND_GITHUB_TOKEN");
        std::env::set_var("GITHUB_TOKEN", "gh-token-789");
        assert_eq!(github_token().unwrap().expose(), "gh-token-789");

        // Restore
        match orig_tend {
            Some(v) => std::env::set_var("TEND_GITHUB_TOKEN", v),
            None => std::env::remove_var("TEND_GITHUB_TOKEN"),
        }
        match orig_gh {
            Some(v) => std::env::set_var("GITHUB_TOKEN", v),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }
}
