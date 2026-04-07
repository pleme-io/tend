use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

const DEFAULT_TTL_SECS: u64 = 6 * 3600; // 6 hours

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    org: String,
    repos: Vec<String>,
    timestamp: u64, // unix epoch seconds
}

/// Resolve the `tend` cache root: `$XDG_CACHE_HOME/tend` or `~/.cache/tend`.
#[must_use]
pub(crate) fn tend_cache_root() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cache")
        })
        .join("tend")
}

fn cache_dir() -> PathBuf {
    tend_cache_root().join("discovery")
}

fn cache_path(org: &str) -> PathBuf {
    cache_dir().join(format!("{org}.json"))
}

/// Read cached discovery results for an org, returning `None` if missing or expired.
#[must_use]
pub(crate) fn read(org: &str) -> Option<Vec<String>> {
    let path = cache_path(org);
    let content = std::fs::read_to_string(&path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&content).ok()?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if now.saturating_sub(entry.timestamp) > DEFAULT_TTL_SECS {
        return None;
    }

    Some(entry.repos)
}

/// Write discovery results for an org to the cache directory.
pub(crate) fn write(org: &str, repos: &[String]) -> Result<()> {
    let dir = cache_dir();
    std::fs::create_dir_all(&dir)?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();

    let entry = CacheEntry {
        org: org.to_string(),
        repos: repos.to_vec(),
        timestamp: now,
    };

    let json = serde_json::to_string_pretty(&entry)?;
    std::fs::write(cache_path(org), json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_entry_roundtrip() {
        let entry = CacheEntry {
            org: "test-org".to_string(),
            repos: vec!["repo-a".to_string(), "repo-b".to_string()],
            timestamp: 1700000000,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: CacheEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.org, "test-org");
        assert_eq!(parsed.repos, vec!["repo-a", "repo-b"]);
        assert_eq!(parsed.timestamp, 1700000000);
    }

    #[test]
    fn test_cache_dir_contains_tend_discovery() {
        let dir = cache_dir();
        assert!(dir.ends_with("tend/discovery"));
    }

    #[test]
    fn test_cache_path_uses_org_name() {
        let path = cache_path("my-org");
        assert!(path.ends_with("my-org.json"));
    }

    #[test]
    fn test_read_returns_none_for_missing_file() {
        let result = read("nonexistent-org-zzzz-12345");
        assert!(result.is_none());
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let org = &format!("tend-test-cache-rt-{}", std::process::id());
        let repos = vec!["alpha".to_string(), "beta".to_string()];
        write(org, &repos).unwrap();

        let loaded = read(org);
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap(), repos);

        // Clean up
        let _ = std::fs::remove_file(cache_path(org));
    }

    #[test]
    fn test_read_returns_none_for_expired_cache() {
        let org = &format!("tend-test-cache-exp-{}", std::process::id());
        let expired_entry = CacheEntry {
            org: org.to_string(),
            repos: vec!["repo".to_string()],
            timestamp: 0, // Epoch — always expired
        };
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let json = serde_json::to_string(&expired_entry).unwrap();
        std::fs::write(cache_path(org), json).unwrap();

        let result = read(org);
        assert!(result.is_none(), "expired cache should return None");

        let _ = std::fs::remove_file(cache_path(org));
    }

    #[test]
    fn test_read_returns_none_for_corrupt_json() {
        let org = &format!("tend-test-cache-bad-{}", std::process::id());
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(cache_path(org), "not valid json!!!").unwrap();

        let result = read(org);
        assert!(result.is_none(), "corrupt JSON should return None");

        let _ = std::fs::remove_file(cache_path(org));
    }

    #[test]
    fn test_write_creates_cache_directory() {
        let org = &format!("tend-test-cache-dir-{}", std::process::id());
        write(org, &["repo".to_string().into()]).unwrap();
        assert!(cache_path(org).exists());
        let _ = std::fs::remove_file(cache_path(org));
    }

    #[test]
    fn test_write_empty_repos_list() {
        let org = &format!("tend-test-cache-empty-{}", std::process::id());
        write(org, &[]).unwrap();
        let loaded = read(org).unwrap();
        assert!(loaded.is_empty());
        let _ = std::fs::remove_file(cache_path(org));
    }

    #[test]
    fn test_write_overwrites_previous_cache() {
        let org = &format!("tend-test-cache-ow-{}", std::process::id());
        write(org, &["first".to_string()]).unwrap();
        write(org, &["second".to_string(), "third".to_string()]).unwrap();
        let loaded = read(org).unwrap();
        assert_eq!(loaded, vec!["second", "third"]);
        let _ = std::fs::remove_file(cache_path(org));
    }

    #[test]
    fn test_cache_path_special_characters() {
        let path = cache_path("org-with-dashes_and_underscores");
        assert!(path.ends_with("org-with-dashes_and_underscores.json"));
    }
}
