use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache;

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
    let client =
        todoku::GitHubClient::new(token.as_deref()).context("building GitHub client")?;

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
    let client = todoku::GitHubClient::new(token.as_deref())
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

/// Get the auth token from environment (TEND_GITHUB_TOKEN or GITHUB_TOKEN).
#[must_use]
pub fn github_token() -> Option<String> {
    std::env::var("TEND_GITHUB_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
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
        assert_eq!(github_token(), Some("tend-token-123".to_string()));

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
        assert_eq!(github_token(), Some("gh-token-789".to_string()));

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
