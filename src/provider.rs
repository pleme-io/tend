use anyhow::{Context, Result};

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

/// Discover all repos in a GitHub org or user account via REST API.
/// Tries the /orgs endpoint first; falls back to /users on 404.
/// Uses TEND_GITHUB_TOKEN or GITHUB_TOKEN env var for auth (optional but needed for private repos).
pub async fn discover_github_repos(org: &str) -> Result<Vec<String>> {
    use todoku::{GitHubApi, OwnerType};

    let token = github_token();
    let client = todoku::GitHubClient::new(token.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))
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
        Err(e) => return Err(anyhow::anyhow!("{e}").context("fetching org repos")),
    }

    match client.list_repos(org, OwnerType::User).await {
        Ok(repos) => {
            let mut names: Vec<String> = repos
                .into_iter()
                .filter(|r| !r.archived)
                .map(|r| r.name)
                .collect();
            names.sort();
            Ok(names)
        }
        Err(e) => Err(anyhow::anyhow!("{e}").context("fetching user repos")),
    }
}

/// Get the auth token from environment (TEND_GITHUB_TOKEN or GITHUB_TOKEN).
pub fn github_token() -> Option<String> {
    std::env::var("TEND_GITHUB_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
}

/// Normalize a GitHub language name to lowercase conventions.
pub(crate) fn normalize_language(lang: &str) -> String {
    match lang {
        "Go" => "go".to_string(),
        "Rust" => "rust".to_string(),
        "Python" => "python".to_string(),
        "TypeScript" | "JavaScript" => "typescript".to_string(),
        "Java" => "java".to_string(),
        "C#" => "csharp".to_string(),
        "C++" => "cpp".to_string(),
        "C" => "c".to_string(),
        "Ruby" => "ruby".to_string(),
        "Shell" => "shell".to_string(),
        "Nix" => "nix".to_string(),
        "HCL" => "hcl".to_string(),
        other => other.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_language() {
        assert_eq!(normalize_language("Go"), "go");
        assert_eq!(normalize_language("Rust"), "rust");
        assert_eq!(normalize_language("TypeScript"), "typescript");
        assert_eq!(normalize_language("JavaScript"), "typescript");
        assert_eq!(normalize_language("Python"), "python");
        assert_eq!(normalize_language("Java"), "java");
        assert_eq!(normalize_language("C#"), "csharp");
        assert_eq!(normalize_language("Fortran"), "fortran");
    }
}
