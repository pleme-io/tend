//! Persistent HEAD-lookup cache backed by a JSON file on the
//! workspace PVC.
//!
//! Why persist: ETag-aware conditional requests are only free *after*
//! we've stored the etag. Pod restarts wipe an in-memory cache, so
//! the first cycle after every restart pays full quota for every
//! input. With a file-backed cache on the PVC, restarts pick up
//! exactly where they left off.
//!
//! Storage format: a single JSON object keyed on
//! `Display(UpstreamId)` strings. ~1KB per entry × ~30 inputs ≈
//! 30KB total — far below filesystem and atomic-write limits.
//! Atomic write via tmpfile + rename so a crashed pod can never
//! corrupt the cache.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::upstream::{CachedHead, UpstreamId};

/// Persistent in-memory cache with file backing. Thread-safe; shared
/// via Arc. Mutations are buffered in memory; `flush()` writes them
/// to disk atomically.
pub struct PersistentHeadCache {
    path: PathBuf,
    inner: tokio::sync::Mutex<HashMap<UpstreamId, CachedHead>>,
}

impl PersistentHeadCache {
    /// Construct from a file path. If the file exists and parses as
    /// JSON, the contents seed the cache; otherwise we start empty.
    /// Failure to read is logged but never fatal — a corrupt cache
    /// just means slower first cycle.
    pub async fn load_or_default(path: PathBuf) -> Self {
        let inner = match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<HashMap<String, CachedHead>>(&bytes) {
                Ok(serialized) => {
                    // Re-key from Display strings back to UpstreamId.
                    // Entries that fail to round-trip are dropped —
                    // safer than a parse error blocking startup.
                    let map: HashMap<UpstreamId, CachedHead> = serialized
                        .into_iter()
                        .filter_map(|(k, v)| parse_id(&k).map(|id| (id, v)))
                        .collect();
                    tracing::info!(
                        count = map.len(),
                        path = %path.display(),
                        "head cache loaded from disk"
                    );
                    map
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        "head cache parse failed; starting empty"
                    );
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "head cache file absent; starting empty");
                HashMap::new()
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "head cache read failed; starting empty"
                );
                HashMap::new()
            }
        };
        Self {
            path,
            inner: tokio::sync::Mutex::new(inner),
        }
    }

    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, HashMap<UpstreamId, CachedHead>> {
        self.inner.lock().await
    }

    /// Atomically write the current cache to disk. Tmpfile + rename
    /// so a crashed pod can't corrupt the cache.
    pub async fn flush(&self) -> Result<()> {
        let snapshot: HashMap<String, CachedHead> = {
            let guard = self.inner.lock().await;
            guard.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating cache dir {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec(&snapshot).context("serializing head cache")?;
        atomic_write(&self.path, &bytes)
            .await
            .with_context(|| format!("writing head cache to {}", self.path.display()))
    }
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{stem}.tmp.{pid}.{ts}"));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Parse an `UpstreamId` from its `Display` form (e.g. `github:owner/repo@HEAD`).
/// Round-trips with `UpstreamId::Display`. Returns None on a
/// shape we don't recognize — the cache entry is dropped rather
/// than poisoning startup.
fn parse_id(s: &str) -> Option<UpstreamId> {
    let (prefix, rest) = s.split_once(':')?;
    let (key, r#ref) = rest.split_once('@')?;
    let source = match prefix {
        "github" => super::upstream::SourceKind::Github,
        "oci" => super::upstream::SourceKind::OciRegistry,
        "crate" => super::upstream::SourceKind::CratesIo,
        "image" => super::upstream::SourceKind::ImageRegistry,
        _ => return None,
    };
    Some(UpstreamId {
        source,
        key: key.to_string(),
        r#ref: r#ref.to_string(),
    })
}

/// Convenience constructor returning an Arc'd instance for use in
/// `Context.head_cache`.
pub async fn open_at(path: PathBuf) -> Arc<PersistentHeadCache> {
    Arc::new(PersistentHeadCache::load_or_default(path).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::upstream::HeadInfo;

    fn sample_entry() -> (UpstreamId, CachedHead) {
        (
            UpstreamId::new_github("nixos", "nixpkgs", "HEAD"),
            CachedHead {
                info: HeadInfo {
                    upstream_rev: "abc123".into(),
                    upstream_modified: 1700000000,
                },
                etag: Some("\"deadbeef\"".into()),
                fetched_at: chrono::Utc::now(),
            },
        )
    }

    #[tokio::test]
    async fn load_or_default_handles_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = PersistentHeadCache::load_or_default(dir.path().join("missing.json")).await;
        assert!(cache.lock().await.is_empty());
    }

    #[tokio::test]
    async fn flush_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subdir/cache.json"); // tests dir creation
        let (id, entry) = sample_entry();

        {
            let cache = PersistentHeadCache::load_or_default(path.clone()).await;
            cache.lock().await.insert(id.clone(), entry.clone());
            cache.flush().await.unwrap();
        }

        // Reload — entry should survive.
        let cache2 = PersistentHeadCache::load_or_default(path).await;
        let guard = cache2.lock().await;
        let restored = guard.get(&id).expect("id round-trips through cache");
        assert_eq!(restored.info.upstream_rev, "abc123");
        assert_eq!(restored.etag.as_deref(), Some("\"deadbeef\""));
    }

    #[tokio::test]
    async fn corrupt_file_starts_empty_and_doesnt_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.json");
        tokio::fs::write(&path, b"not valid json").await.unwrap();
        let cache = PersistentHeadCache::load_or_default(path).await;
        assert!(cache.lock().await.is_empty());
    }

    #[test]
    fn parse_id_round_trips_all_source_kinds() {
        for id in [
            UpstreamId::new_github("nixos", "nixpkgs", "HEAD"),
            UpstreamId::new_github("pleme-io", "typemill", "v0.8.18-pleme.1"),
            UpstreamId::new_oci("ghcr.io", "pleme-io/charts/foo", "amd64-2216c1d"),
            UpstreamId::new_crate("serde", "stable"),
            UpstreamId::new_image("ghcr.io", "pleme-io/tend", "amd64-latest"),
        ] {
            let s = id.to_string();
            let parsed = parse_id(&s).expect("must parse what Display emits");
            assert_eq!(parsed, id, "round-trip mismatch for {s}");
        }
    }

    #[test]
    fn parse_id_rejects_unknown_prefixes() {
        assert!(parse_id("unknown:foo@bar").is_none());
        assert!(parse_id("nat-shaped string").is_none());
    }
}
