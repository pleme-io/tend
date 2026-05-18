//! Placeholder markers — typed empty-dir intent.
//!
//! Operators sometimes want a directory under a workspace's base_dir
//! that tend should NOT try to clone, sync, or pull into. Use cases:
//! - A scratch dir for ad-hoc experiments that aren't yet a git repo.
//! - A symlink or mount point managed by another tool.
//! - A reserved name to prevent tend from re-cloning a repo the
//!   operator deliberately deleted on disk.
//!
//! M3 from the original tend plan + SHIGOTO.md §IV.4. The marker is a
//! file named `PLACEHOLDER_FILENAME` (`.tend-placeholder`) inside the
//! directory. The `NotPlaceholderGate` (jobs/gates.rs) consults this
//! to Skip reconcile Jobs against marked dirs.

use std::path::Path;

/// Filename that marks a directory as an intentional placeholder.
/// Lives inside the directory itself so the marker travels with the
/// path (a `cp -r` or `mv` preserves the marker).
pub(crate) const PLACEHOLDER_FILENAME: &str = ".tend-placeholder";

/// True iff `path` is a directory containing `.tend-placeholder`.
/// Tolerates missing parent dirs (returns false).
#[must_use]
pub(crate) fn is_placeholder(path: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    path.join(PLACEHOLDER_FILENAME).exists()
}

/// Mark `path` as a placeholder. Creates the directory if it doesn't
/// exist, then writes an empty `.tend-placeholder` file. Idempotent:
/// re-marking an already-marked dir is a no-op success.
pub(crate) fn mark_placeholder(path: &Path) -> anyhow::Result<()> {
    use anyhow::Context;
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating placeholder dir {}", path.display()))?;
    std::fs::write(path.join(PLACEHOLDER_FILENAME), "")
        .with_context(|| format!("writing placeholder marker in {}", path.display()))?;
    Ok(())
}

/// Remove the placeholder marker from `path`. Does nothing if the
/// marker is absent. Returns Ok in both cases (idempotent).
pub(crate) fn unmark_placeholder(path: &Path) -> anyhow::Result<()> {
    let marker = path.join(PLACEHOLDER_FILENAME);
    if marker.exists() {
        std::fs::remove_file(&marker).map_err(|e| {
            anyhow::anyhow!("removing placeholder marker {}: {e}", marker.display())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_dir_is_not_placeholder() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("empty");
        std::fs::create_dir(&dir).unwrap();
        assert!(!is_placeholder(&dir));
    }

    #[test]
    fn missing_dir_is_not_placeholder() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(!is_placeholder(&missing));
    }

    #[test]
    fn marked_dir_is_placeholder() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("marked");
        mark_placeholder(&dir).unwrap();
        assert!(is_placeholder(&dir));
    }

    #[test]
    fn mark_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("twice");
        mark_placeholder(&dir).unwrap();
        mark_placeholder(&dir).unwrap();
        assert!(is_placeholder(&dir));
    }

    #[test]
    fn unmark_removes_marker() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("temp");
        mark_placeholder(&dir).unwrap();
        unmark_placeholder(&dir).unwrap();
        assert!(!is_placeholder(&dir));
    }

    #[test]
    fn unmark_unmarked_dir_is_ok() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("plain");
        std::fs::create_dir(&dir).unwrap();
        unmark_placeholder(&dir).unwrap();
        assert!(!is_placeholder(&dir));
    }
}
