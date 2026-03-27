use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Trait abstracting watch state persistence for testability.
pub trait WatchStateStore: Send + Sync {
    fn load(&self, workspace_name: &str) -> Result<WatchState>;
    fn save(&self, workspace_name: &str, state: &WatchState) -> Result<()>;
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct WatchState {
    #[serde(default)]
    pub repos: BTreeMap<String, RepoState>,
    /// Cached file blob SHAs for file watches. Key: "org/repo/path" → SHA.
    #[serde(default)]
    pub file_shas: BTreeMap<String, String>,
    /// Cached upstream state for flake input watches. Key: watch name.
    #[serde(default)]
    pub flake_inputs: BTreeMap<String, FlakeInputCacheEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RepoState {
    pub head: String,
    pub latest_tag: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct FlakeInputCacheEntry {
    pub upstream_rev: String,
    #[serde(default)]
    pub upstream_tag: Option<String>,
}

/// Real implementation backed by the filesystem.
pub struct FsWatchStateStore;

impl WatchStateStore for FsWatchStateStore {
    fn load(&self, workspace_name: &str) -> Result<WatchState> {
        let path = cache_path(workspace_name);
        if !path.exists() {
            return Ok(WatchState::default());
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading watch cache {}", path.display()))?;

        let state: WatchState = toml::from_str(&content)
            .with_context(|| format!("parsing watch cache {}", path.display()))?;

        Ok(state)
    }

    fn save(&self, workspace_name: &str, state: &WatchState) -> Result<()> {
        let dir = cache_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating watch cache dir {}", dir.display()))?;

        let content = toml::to_string_pretty(state)
            .context("serializing watch state")?;

        let path = cache_path(workspace_name);
        std::fs::write(&path, content)
            .with_context(|| format!("writing watch cache {}", path.display()))?;

        Ok(())
    }
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
        .join("watch")
}

fn cache_path(workspace_name: &str) -> PathBuf {
    cache_dir().join(format!("{workspace_name}.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_watch_state_deserialize_empty() {
        let state: WatchState = toml::from_str("").unwrap();
        assert!(state.repos.is_empty());
    }

    #[test]
    fn test_watch_state_deserialize_no_repos_key() {
        let state: WatchState = toml::from_str("# just a comment\n").unwrap();
        assert!(state.repos.is_empty());
    }

    #[test]
    fn test_watch_state_roundtrip() {
        let mut state = WatchState::default();
        state.repos.insert("test-repo".to_string(), RepoState {
            head: "abc123".to_string(),
            latest_tag: Some("v1.0.0".to_string()),
            language: Some("go".to_string()),
        });

        let serialized = toml::to_string_pretty(&state).unwrap();
        let deserialized: WatchState = toml::from_str(&serialized).unwrap();

        assert_eq!(deserialized.repos.len(), 1);
        let repo = &deserialized.repos["test-repo"];
        assert_eq!(repo.head, "abc123");
        assert_eq!(repo.latest_tag.as_deref(), Some("v1.0.0"));
        assert_eq!(repo.language.as_deref(), Some("go"));
    }
}
