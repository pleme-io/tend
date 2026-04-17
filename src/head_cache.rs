//! TTL-cached upstream HEAD SHA lookup.
//!
//! The flake-update daemon runs on a short cycle; we don't want to hammer the
//! GitHub API asking for the same repo's HEAD SHA every minute. This module
//! provides a filesystem-backed cache with a short TTL (default 60s) and a
//! trait-boundaried fetcher so tests can inject fixtures.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::github::GitHubClient;

const DEFAULT_TTL_SECS: u64 = 60;

#[derive(Serialize, Deserialize)]
struct HeadCacheEntry {
    owner: String,
    repo: String,
    branch: String,
    rev: String,
    timestamp: u64,
}

fn cache_dir() -> PathBuf {
    crate::cache::tend_cache_root().join("head")
}

fn cache_key(owner: &str, repo: &str, branch: &str) -> String {
    // Path-safe key. Repo names may contain dots; keep them as-is for readability.
    format!("{owner}__{repo}__{branch}.json")
}

fn cache_path(owner: &str, repo: &str, branch: &str) -> PathBuf {
    cache_dir().join(cache_key(owner, repo, branch))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a cached HEAD rev if it exists and is within TTL.
pub fn read(owner: &str, repo: &str, branch: &str, ttl_secs: u64) -> Option<String> {
    let path = cache_path(owner, repo, branch);
    let content = std::fs::read_to_string(&path).ok()?;
    let entry: HeadCacheEntry = serde_json::from_str(&content).ok()?;
    let now = now_secs();
    if now.saturating_sub(entry.timestamp) > ttl_secs {
        return None;
    }
    Some(entry.rev)
}

pub fn write(owner: &str, repo: &str, branch: &str, rev: &str) -> Result<()> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir)?;
    let entry = HeadCacheEntry {
        owner: owner.to_string(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        rev: rev.to_string(),
        timestamp: now_secs(),
    };
    let json = serde_json::to_string(&entry)?;
    std::fs::write(cache_path(owner, repo, branch), json)?;
    Ok(())
}

/// Trait wrapper around HEAD lookup so callers can inject a cached resolver or
/// a test fixture without dragging the full `GitHubClient` trait through.
#[async_trait]
pub trait UpstreamHead: Send + Sync {
    async fn head_sha(&self, owner: &str, repo: &str, branch: &str) -> Result<String>;
}

/// Production resolver: cached first, then delegates to a `GitHubClient`.
///
/// Note: the underlying `GitHubClient::get_repo_head` queries the default
/// branch; if callers want a non-default branch and the repo's default is
/// different, extend `GitHubClient` with a branch-aware method.
pub struct CachedGitHubHead<'a, G: GitHubClient + ?Sized> {
    client: &'a G,
    ttl_secs: u64,
}

impl<'a, G: GitHubClient + ?Sized> CachedGitHubHead<'a, G> {
    pub fn new(client: &'a G) -> Self {
        Self {
            client,
            ttl_secs: DEFAULT_TTL_SECS,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_ttl(mut self, ttl_secs: u64) -> Self {
        self.ttl_secs = ttl_secs;
        self
    }
}

#[async_trait]
impl<G: GitHubClient + ?Sized> UpstreamHead for CachedGitHubHead<'_, G> {
    async fn head_sha(&self, owner: &str, repo: &str, branch: &str) -> Result<String> {
        if let Some(cached) = read(owner, repo, branch, self.ttl_secs) {
            return Ok(cached);
        }
        let rev = self.client.get_repo_head(owner, repo).await?;
        let _ = write(owner, repo, branch, &rev);
        Ok(rev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingClient {
        calls: Arc<AtomicUsize>,
        rev: String,
    }

    #[async_trait]
    impl GitHubClient for CountingClient {
        async fn get_repo_head(&self, _: &str, _: &str) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.rev.clone())
        }
        async fn get_latest_tag(&self, _: &str, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn detect_repo_language(&self, _: &str, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn get_file_sha(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(String, u64, String)> {
            Ok((String::new(), 0, String::new()))
        }
    }

    #[test]
    fn test_cache_key_is_stable() {
        let k = cache_key("org", "repo.with.dots", "main");
        assert_eq!(k, "org__repo.with.dots__main.json");
    }

    #[test]
    fn test_read_write_roundtrip() {
        let owner = "tend-test-head";
        let repo = &format!("repo-{}", std::process::id());
        write(owner, repo, "main", "abc123").unwrap();
        let loaded = read(owner, repo, "main", 3600);
        assert_eq!(loaded.as_deref(), Some("abc123"));
        let _ = std::fs::remove_file(cache_path(owner, repo, "main"));
    }

    #[test]
    fn test_read_returns_none_when_expired() {
        let owner = "tend-test-head-exp";
        let repo = &format!("repo-{}", std::process::id());
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let entry = HeadCacheEntry {
            owner: owner.to_string(),
            repo: repo.to_string(),
            branch: "main".to_string(),
            rev: "old".to_string(),
            timestamp: 0,
        };
        std::fs::write(
            cache_path(owner, repo, "main"),
            serde_json::to_string(&entry).unwrap(),
        )
        .unwrap();
        assert!(read(owner, repo, "main", 60).is_none());
        let _ = std::fs::remove_file(cache_path(owner, repo, "main"));
    }

    #[tokio::test]
    async fn test_cached_github_head_hits_cache_on_second_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = CountingClient {
            calls: calls.clone(),
            rev: "cafe".to_string(),
        };
        let owner = "tend-test-cached";
        let repo = &format!("r-{}", std::process::id());
        let _ = std::fs::remove_file(cache_path(owner, repo, "main"));

        let resolver = CachedGitHubHead::new(&client).with_ttl(3600);
        let r1 = resolver.head_sha(owner, repo, "main").await.unwrap();
        let r2 = resolver.head_sha(owner, repo, "main").await.unwrap();
        assert_eq!(r1, "cafe");
        assert_eq!(r2, "cafe");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "second call should hit cache");

        let _ = std::fs::remove_file(cache_path(owner, repo, "main"));
    }
}
