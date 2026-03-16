use anyhow::Result;
use async_trait::async_trait;

/// Trait abstracting GitHub API interactions for testability.
///
/// Production code uses `HttpGitHubClient`, which makes real HTTP requests.
/// Tests can substitute a mock that returns predetermined responses.
#[async_trait]
pub trait GitHubClient: Send + Sync {
    /// Get the HEAD commit SHA for a repo.
    async fn get_repo_head(&self, org: &str, repo: &str) -> Result<String>;

    /// Get the latest tag for a repo (returns None if no tags).
    async fn get_latest_tag(&self, org: &str, repo: &str) -> Result<Option<String>>;

    /// Detect the primary language of a repo.
    async fn detect_repo_language(&self, org: &str, repo: &str) -> Result<Option<String>>;

    /// Get the git blob SHA, size, and download URL for a file in a repo.
    /// Uses: GET /repos/{org}/{repo}/contents/{path}
    async fn get_file_sha(
        &self,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<(String, u64, String)>;
}

/// Real implementation using reqwest HTTP client.
pub struct HttpGitHubClient {
    client: reqwest::Client,
    token: Option<String>,
}

impl HttpGitHubClient {
    pub fn new() -> Result<Self> {
        let client = crate::provider::build_github_client()?;
        let token = crate::provider::github_token();
        Ok(Self { client, token })
    }
}

#[async_trait]
impl GitHubClient for HttpGitHubClient {
    async fn get_repo_head(&self, org: &str, repo: &str) -> Result<String> {
        crate::provider::get_repo_head(&self.client, self.token.as_deref(), org, repo).await
    }

    async fn get_latest_tag(&self, org: &str, repo: &str) -> Result<Option<String>> {
        crate::provider::get_latest_tag(&self.client, self.token.as_deref(), org, repo).await
    }

    async fn detect_repo_language(&self, org: &str, repo: &str) -> Result<Option<String>> {
        crate::provider::detect_repo_language(&self.client, self.token.as_deref(), org, repo).await
    }

    async fn get_file_sha(
        &self,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<(String, u64, String)> {
        crate::provider::get_file_sha(&self.client, self.token.as_deref(), org, repo, path).await
    }
}
