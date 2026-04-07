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
    /// Last successful flake refresh timestamp per repo (Unix epoch seconds).
    #[serde(default)]
    pub flake_refresh_at: BTreeMap<String, u64>,
    /// Consecutive no-change count per repo for adaptive backoff.
    #[serde(default)]
    pub flake_refresh_misses: BTreeMap<String, u32>,
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
    crate::cache::tend_cache_root().join("watch")
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

    #[test]
    fn test_watch_state_multiple_repos() {
        let mut state = WatchState::default();
        state.repos.insert("repo-a".to_string(), RepoState {
            head: "aaa111".to_string(),
            latest_tag: Some("v1.0.0".to_string()),
            language: Some("rust".to_string()),
        });
        state.repos.insert("repo-b".to_string(), RepoState {
            head: "bbb222".to_string(),
            latest_tag: None,
            language: None,
        });

        let serialized = toml::to_string_pretty(&state).unwrap();
        let deserialized: WatchState = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.repos.len(), 2);
        assert_eq!(deserialized.repos["repo-b"].head, "bbb222");
        assert!(deserialized.repos["repo-b"].latest_tag.is_none());
        assert!(deserialized.repos["repo-b"].language.is_none());
    }

    #[test]
    fn test_watch_state_file_shas_roundtrip() {
        let mut state = WatchState::default();
        state.file_shas.insert(
            "org/repo/path.yaml".to_string(),
            "sha256abc".to_string(),
        );

        let serialized = toml::to_string_pretty(&state).unwrap();
        let deserialized: WatchState = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.file_shas.get("org/repo/path.yaml").unwrap(),
            "sha256abc"
        );
    }

    #[test]
    fn test_watch_state_flake_refresh_fields_roundtrip() {
        let mut state = WatchState::default();
        state.flake_refresh_at.insert("repo-a".to_string(), 1700000000);
        state.flake_refresh_misses.insert("repo-a".to_string(), 3);

        let serialized = toml::to_string_pretty(&state).unwrap();
        let deserialized: WatchState = toml::from_str(&serialized).unwrap();
        assert_eq!(*deserialized.flake_refresh_at.get("repo-a").unwrap(), 1700000000);
        assert_eq!(*deserialized.flake_refresh_misses.get("repo-a").unwrap(), 3);
    }

    #[test]
    fn test_flake_input_cache_entry_defaults() {
        let entry: FlakeInputCacheEntry = toml::from_str(r#"upstream_rev = "abc""#).unwrap();
        assert_eq!(entry.upstream_rev, "abc");
        assert!(entry.upstream_tag.is_none());
    }

    #[test]
    fn test_fs_watch_state_store_load_missing_file() {
        let store = FsWatchStateStore;
        let state = store.load("nonexistent-workspace-zzz-12345").unwrap();
        assert!(state.repos.is_empty());
        assert!(state.file_shas.is_empty());
        assert!(state.flake_inputs.is_empty());
    }

    #[test]
    fn test_fs_watch_state_store_save_and_load() {
        let store = FsWatchStateStore;
        let ws_name = &format!("tend-test-wss-{}", std::process::id());

        let mut state = WatchState::default();
        state.repos.insert("my-repo".to_string(), RepoState {
            head: "deadbeef".to_string(),
            latest_tag: Some("v2.0.0".to_string()),
            language: Some("go".to_string()),
        });
        state.file_shas.insert("org/repo/file".to_string(), "sha123".to_string());
        state.flake_inputs.insert("watch-1".to_string(), FlakeInputCacheEntry {
            upstream_rev: "upstream-abc".to_string(),
            upstream_tag: Some("v3.0.0".to_string()),
        });

        store.save(ws_name, &state).unwrap();
        let loaded = store.load(ws_name).unwrap();

        assert_eq!(loaded.repos.len(), 1);
        assert_eq!(loaded.repos["my-repo"].head, "deadbeef");
        assert_eq!(loaded.repos["my-repo"].latest_tag.as_deref(), Some("v2.0.0"));
        assert_eq!(loaded.file_shas["org/repo/file"], "sha123");
        assert_eq!(loaded.flake_inputs["watch-1"].upstream_rev, "upstream-abc");
        assert_eq!(loaded.flake_inputs["watch-1"].upstream_tag.as_deref(), Some("v3.0.0"));

        // Clean up
        let _ = std::fs::remove_file(cache_path(ws_name));
    }

    #[test]
    fn test_fs_watch_state_store_overwrite() {
        let store = FsWatchStateStore;
        let ws_name = &format!("tend-test-wss-ow-{}", std::process::id());

        let mut state1 = WatchState::default();
        state1.repos.insert("repo-1".to_string(), RepoState {
            head: "first".to_string(),
            latest_tag: None,
            language: None,
        });
        store.save(ws_name, &state1).unwrap();

        let mut state2 = WatchState::default();
        state2.repos.insert("repo-2".to_string(), RepoState {
            head: "second".to_string(),
            latest_tag: Some("v1.0.0".to_string()),
            language: Some("rust".to_string()),
        });
        store.save(ws_name, &state2).unwrap();

        let loaded = store.load(ws_name).unwrap();
        assert_eq!(loaded.repos.len(), 1);
        assert!(loaded.repos.contains_key("repo-2"));
        assert!(!loaded.repos.contains_key("repo-1"));

        let _ = std::fs::remove_file(cache_path(ws_name));
    }

    #[test]
    fn test_watch_state_default_is_empty() {
        let state = WatchState::default();
        assert!(state.repos.is_empty());
        assert!(state.file_shas.is_empty());
        assert!(state.flake_inputs.is_empty());
        assert!(state.flake_refresh_at.is_empty());
        assert!(state.flake_refresh_misses.is_empty());
    }

    #[test]
    fn test_repo_state_with_no_optional_fields() {
        let state = RepoState {
            head: "abc".to_string(),
            latest_tag: None,
            language: None,
        };
        let mut ws = WatchState::default();
        ws.repos.insert("bare".to_string(), state);
        let serialized = toml::to_string_pretty(&ws).unwrap();
        let deserialized: WatchState = toml::from_str(&serialized).unwrap();
        assert!(deserialized.repos["bare"].latest_tag.is_none());
        assert!(deserialized.repos["bare"].language.is_none());
    }

    #[test]
    fn test_flake_input_cache_entry_roundtrip() {
        let entry = FlakeInputCacheEntry {
            upstream_rev: "abc123".to_string(),
            upstream_tag: Some("v1.0.0".to_string()),
        };
        let serialized = toml::to_string(&entry).unwrap();
        let deserialized: FlakeInputCacheEntry = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.upstream_rev, "abc123");
        assert_eq!(deserialized.upstream_tag.as_deref(), Some("v1.0.0"));
    }

    #[test]
    fn test_cache_path_uses_workspace_name() {
        let path = cache_path("my-workspace");
        assert!(path.ends_with("my-workspace.toml"));
    }

    #[test]
    fn test_cache_dir_ends_with_tend_watch() {
        let dir = cache_dir();
        assert!(dir.ends_with("tend/watch"));
    }
}
