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

fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".cache")
        })
        .join("tend")
        .join("discovery")
}

fn cache_path(org: &str) -> PathBuf {
    cache_dir().join(format!("{org}.json"))
}

pub fn read(org: &str) -> Option<Vec<String>> {
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

pub fn write(org: &str, repos: &[String]) -> Result<()> {
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
