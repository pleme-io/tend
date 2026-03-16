use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

use crate::cache;

#[derive(Debug, Deserialize)]
struct GitHubRepo {
    name: String,
    archived: bool,
}

#[derive(Debug, Deserialize)]
struct GitHubCommit {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct GitHubTag {
    name: String,
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

/// Discover all repos in a GitHub org or user account via REST API.
/// Tries the /orgs endpoint first; falls back to /users on 404.
/// Uses TEND_GITHUB_TOKEN or GITHUB_TOKEN env var for auth (optional but needed for private repos).
pub async fn discover_github_repos(org: &str) -> Result<Vec<String>> {
    let token = std::env::var("TEND_GITHUB_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok();

    let client = reqwest::Client::builder()
        .user_agent("tend/0.1.0")
        .build()
        .context("building HTTP client")?;

    // Try org endpoint first, then user endpoint on 404
    for endpoint in ["orgs", "users"] {
        match fetch_repos(&client, token.as_deref(), endpoint, org).await {
            Ok(repos) => return Ok(repos),
            Err(e) if endpoint == "orgs" && is_not_found(&e) => {
                // org endpoint returned 404, try user endpoint
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(Vec::new())
}

/// Returns true if the error is a GitHub 404 Not Found.
fn is_not_found(err: &anyhow::Error) -> bool {
    err.to_string().contains("404 Not Found")
}

/// Fetch all non-archived repos from a GitHub API endpoint.
async fn fetch_repos(
    client: &reqwest::Client,
    token: Option<&str>,
    endpoint: &str,
    name: &str,
) -> Result<Vec<String>> {
    let mut all_repos = Vec::new();
    let mut page = 1u32;

    loop {
        let url = format!(
            "https://api.github.com/{endpoint}/{name}/repos?per_page=100&page={page}&type=all"
        );

        let mut req = client.get(&url);
        if let Some(token) = token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("fetching repos for {name} (page {page})"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API returned {status}: {body}");
        }

        let repos: Vec<GitHubRepo> = resp
            .json()
            .await
            .context("parsing GitHub API response")?;

        if repos.is_empty() {
            break;
        }

        for repo in &repos {
            if !repo.archived {
                all_repos.push(repo.name.clone());
            }
        }

        page += 1;
    }

    all_repos.sort();
    Ok(all_repos)
}

/// Build a GitHub API client with auth from TEND_GITHUB_TOKEN or GITHUB_TOKEN.
pub fn build_github_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("tend/0.1.0")
        .build()
        .context("building HTTP client")
}

/// Get the auth token from environment (TEND_GITHUB_TOKEN or GITHUB_TOKEN).
pub fn github_token() -> Option<String> {
    std::env::var("TEND_GITHUB_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()
}

/// Get the HEAD commit SHA for a repo.
///
/// Note: For workspaces with many repos (80+), this is called once per repo per
/// watch cycle. Be mindful of GitHub API rate limits (5000 req/hr authenticated,
/// 60 req/hr unauthenticated).
pub async fn get_repo_head(
    client: &reqwest::Client,
    token: Option<&str>,
    org: &str,
    repo: &str,
) -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{org}/{repo}/commits?per_page=1"
    );

    let mut req = client.get(&url);
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("fetching HEAD for {org}/{repo}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {status} for {org}/{repo} commits: {body}");
    }

    let commits: Vec<GitHubCommit> = resp
        .json()
        .await
        .with_context(|| format!("parsing commits for {org}/{repo}"))?;

    commits
        .into_iter()
        .next()
        .map(|c| c.sha)
        .ok_or_else(|| anyhow::anyhow!("no commits found for {org}/{repo}"))
}

/// Get the latest tag for a repo (returns None if no tags).
///
/// Note: Tags are returned sorted by creation date (newest first) by the API.
pub async fn get_latest_tag(
    client: &reqwest::Client,
    token: Option<&str>,
    org: &str,
    repo: &str,
) -> Result<Option<String>> {
    let url = format!(
        "https://api.github.com/repos/{org}/{repo}/tags?per_page=1"
    );

    let mut req = client.get(&url);
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("fetching tags for {org}/{repo}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {status} for {org}/{repo} tags: {body}");
    }

    let tags: Vec<GitHubTag> = resp
        .json()
        .await
        .with_context(|| format!("parsing tags for {org}/{repo}"))?;

    Ok(tags.into_iter().next().map(|t| t.name))
}

/// Get the git blob SHA, size, and download URL for a file in a repo.
///
/// Uses the GitHub Contents API: GET /repos/{org}/{repo}/contents/{path}
/// Returns: (sha, size, download_url)
pub async fn get_file_sha(
    client: &reqwest::Client,
    token: Option<&str>,
    org: &str,
    repo: &str,
    path: &str,
) -> Result<(String, u64, String)> {
    let url = format!(
        "https://api.github.com/repos/{org}/{repo}/contents/{path}"
    );

    let mut req = client.get(&url);
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("fetching file SHA for {org}/{repo}/{path}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {status} for {org}/{repo}/contents/{path}: {body}");
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("parsing contents response for {org}/{repo}/{path}"))?;

    let sha = json["sha"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let size = json["size"]
        .as_u64()
        .unwrap_or(0);
    let download_url = json["download_url"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok((sha, size, download_url))
}

/// Detect the primary language of a repo via the GitHub languages endpoint.
///
/// Returns the top language, normalized to lowercase conventions:
/// "Go" → "go", "Rust" → "rust", "Python" → "python",
/// "TypeScript" or "JavaScript" → "typescript", etc.
pub async fn detect_repo_language(
    client: &reqwest::Client,
    token: Option<&str>,
    org: &str,
    repo: &str,
) -> Result<Option<String>> {
    let url = format!(
        "https://api.github.com/repos/{org}/{repo}/languages"
    );

    let mut req = client.get(&url);
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req
        .send()
        .await
        .with_context(|| format!("fetching languages for {org}/{repo}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API returned {status} for {org}/{repo} languages: {body}");
    }

    let languages: HashMap<String, u64> = resp
        .json()
        .await
        .with_context(|| format!("parsing languages for {org}/{repo}"))?;

    // Pick the language with the most bytes
    let top = languages
        .into_iter()
        .max_by_key(|(_, bytes)| *bytes)
        .map(|(lang, _)| lang);

    Ok(top.map(|lang| normalize_language(&lang)))
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
