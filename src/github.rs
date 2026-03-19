use anyhow::Result;
use async_trait::async_trait;

/// Trait abstracting GitHub API interactions for testability.
///
/// Production code uses `HttpGitHubClient`, which delegates to todoku's GitHub client.
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

/// Real implementation backed by todoku's GitHub client.
pub struct HttpGitHubClient {
    inner: todoku::GitHubClient,
}

impl HttpGitHubClient {
    pub fn new() -> Result<Self> {
        let token = crate::provider::github_token();
        let inner = todoku::GitHubClient::new(token.as_deref())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl GitHubClient for HttpGitHubClient {
    async fn get_repo_head(&self, org: &str, repo: &str) -> Result<String> {
        use todoku::GitHubApi;
        self.inner
            .get_repo_head(org, repo)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn get_latest_tag(&self, org: &str, repo: &str) -> Result<Option<String>> {
        use todoku::GitHubApi;
        self.inner
            .get_latest_tag(org, repo)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn detect_repo_language(&self, org: &str, repo: &str) -> Result<Option<String>> {
        use todoku::GitHubApi;
        self.inner
            .get_primary_language(org, repo)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn get_file_sha(
        &self,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<(String, u64, String)> {
        use todoku::GitHubApi;
        let info = self
            .inner
            .get_file_info(org, repo, path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((info.sha, info.size, info.download_url))
    }
}
