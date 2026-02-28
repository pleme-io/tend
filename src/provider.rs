use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct GitHubRepo {
    name: String,
    archived: bool,
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
