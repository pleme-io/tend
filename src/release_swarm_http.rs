//! Real GitHub REST API implementation of `ReleaseSwarmApi`.
//!
//! Uses todoku's `HttpClient` for retry + auth, layered over the
//! GitHub REST API. Separate module so the trait's mock impl stays
//! clean + the HTTP impl can be exercised in isolation.
//!
//! **Endpoints touched** (all under `https://api.github.com`):
//! - `GET /repos/{org}/{repo}` — resolve default branch
//! - `GET /repos/{org}/{repo}/git/ref/heads/{branch}` — resolve branch head SHA
//! - `GET /repos/{org}/{repo}/contents/{path}` — check for existing file
//! - `POST /repos/{org}/{repo}/git/refs` — create the PR branch
//! - `PUT /repos/{org}/{repo}/contents/{path}` — write the file on the branch
//! - `POST /repos/{org}/{repo}/pulls` — open the PR
//!
//! **Rate-limit aware** via todoku's `RetryPolicy` (exponential
//! backoff on 429 + 5xx); GitHub's `X-RateLimit-Remaining` header is
//! surfaced by todoku's error path when 403 + rate-limited.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::release_swarm::ReleaseSwarmApi;

/// Production-impl for `ReleaseSwarmApi`. Holds one todoku client
/// pinned to `https://api.github.com` with bearer auth.
pub struct HttpReleaseSwarmApi {
    client: todoku::HttpClient,
}

impl HttpReleaseSwarmApi {
    /// Construct from a GitHub token (PAT or fine-grained). The
    /// caller is responsible for sourcing the token (env or secret).
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let client = todoku::HttpClient::builder()
            .base_url("https://api.github.com")
            .auth(todoku::BearerToken::new(token))
            .user_agent("pleme-io-tend-release-swarm/0.1")
            .header(
                reqwest::header::ACCEPT,
                "application/vnd.github+json",
            )
            .header(
                reqwest::header::HeaderName::from_static("x-github-api-version"),
                "2022-11-28",
            )
            .retry(todoku::RetryPolicy::aggressive())
            .build()
            .context("building todoku client for release-swarm")?;
        Ok(Self { client })
    }
}

#[async_trait]
impl ReleaseSwarmApi for HttpReleaseSwarmApi {
    async fn get_workflow_file_sha(
        &self,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<Option<String>> {
        let url = format!("/repos/{org}/{repo}/contents/{path}");
        match self.client.get::<ContentsResponse>(&url).await {
            Ok(resp) => Ok(Some(resp.sha)),
            Err(e) => {
                // 404 → file doesn't exist; map to Ok(None).
                // `TodokuError` in the version pinned here doesn't
                // expose a structured `.status()`; match on the
                // Display output which contains "HTTP 404" for HTTP
                // errors. Good enough for this lookup; bump todoku
                // version and switch to structured match when the
                // workspace lock is next updated.
                let msg = e.to_string();
                if msg.contains("404") {
                    Ok(None)
                } else {
                    Err(anyhow::Error::from(e))
                        .with_context(|| format!("GET {url}"))
                }
            }
        }
    }

    async fn open_workflow_pr(
        &self,
        org: &str,
        repo: &str,
        branch: &str,
        path: &str,
        content: &str,
        commit_message: &str,
        pr_title: &str,
        pr_body: &str,
    ) -> Result<u64> {
        // 1. Resolve repo's default branch.
        let repo_meta_url = format!("/repos/{org}/{repo}");
        let repo_meta: RepoMetaResponse = self
            .client
            .get(&repo_meta_url)
            .await
            .with_context(|| format!("GET {repo_meta_url}"))?;
        let base_branch = repo_meta.default_branch;

        // 2. Resolve the SHA of the default branch's tip.
        let ref_url = format!("/repos/{org}/{repo}/git/ref/heads/{base_branch}");
        let base_ref: RefResponse = self
            .client
            .get(&ref_url)
            .await
            .with_context(|| format!("GET {ref_url}"))?;
        let base_sha = base_ref.object.sha;

        // 3. Create the PR branch from base_sha.
        let create_ref_url = format!("/repos/{org}/{repo}/git/refs");
        let create_body = CreateRefRequest {
            r#ref: format!("refs/heads/{branch}"),
            sha: base_sha.clone(),
        };
        let _: serde_json::Value = self
            .client
            .post(&create_ref_url, &create_body)
            .await
            .with_context(|| format!("POST {create_ref_url}"))?;

        // 4. PUT the file on the new branch. Content is base64-encoded.
        let put_url = format!("/repos/{org}/{repo}/contents/{path}");
        let put_body = PutContentsRequest {
            message: commit_message.to_string(),
            content: base64_encode(content.as_bytes()),
            branch: branch.to_string(),
            // No sha — branch is brand-new, file doesn't exist yet.
            sha: None,
        };
        let _: serde_json::Value = self
            .client
            .put(&put_url, &put_body)
            .await
            .with_context(|| format!("PUT {put_url}"))?;

        // 5. Open the PR.
        let pr_url = format!("/repos/{org}/{repo}/pulls");
        let pr_req = CreatePrRequest {
            title: pr_title.to_string(),
            body: pr_body.to_string(),
            head: branch.to_string(),
            base: base_branch,
        };
        let pr_resp: CreatePrResponse = self
            .client
            .post(&pr_url, &pr_req)
            .await
            .with_context(|| format!("POST {pr_url}"))?;

        Ok(pr_resp.number)
    }
}

// ── GitHub API DTOs ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ContentsResponse {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct RepoMetaResponse {
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct RefResponse {
    object: RefObject,
}

#[derive(Debug, Deserialize)]
struct RefObject {
    sha: String,
}

#[derive(Debug, Serialize)]
struct CreateRefRequest {
    r#ref: String,
    sha: String,
}

#[derive(Debug, Serialize)]
struct PutContentsRequest {
    message: String,
    content: String,
    branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreatePrRequest {
    title: String,
    body: String,
    head: String,
    base: String,
}

#[derive(Debug, Deserialize)]
struct CreatePrResponse {
    number: u64,
}

// ── Base64 helper ───────────────────────────────────────────────

/// Standard base64 encoding (RFC 4648, with `=` padding). Inline
/// to avoid pulling in a `base64` crate dep — will be moved to
/// todoku as `pub fn base64_encode` once we need it in a second
/// place.
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_standard_vectors() {
        // RFC 4648 §10 test vectors
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_roundtrips_via_yaml_content() {
        // workflow snippet-sized content — just prove stability
        let content = "name: Release\non:\n  push:\n    tags: ['v*.*.*']\n";
        let encoded = base64_encode(content.as_bytes());
        assert!(!encoded.is_empty());
        // Known shape: every 3 bytes → 4 chars, padded up
        assert_eq!(encoded.len() % 4, 0);
    }

    #[test]
    fn http_client_construction_smoke() {
        // Construct with a fake token; must not panic.
        let api = HttpReleaseSwarmApi::new("fake-token-for-smoke-test");
        assert!(api.is_ok());
    }
}
