//! Minimal flake.lock parser.
//!
//! flake.lock is a JSON document maintained by Nix that maps each named flake
//! input to its currently locked revision. We only need read-only access to
//! (owner, repo, locked rev, ref) for GitHub-hosted inputs, which is enough to
//! decide whether a given input is converged against its upstream HEAD.
//!
//! Unsupported input types (git+, tarball, path, etc.) are surfaced as `None`
//! from `locked_input()` — callers should treat them as "can't prove converged"
//! and fall back to running `nix flake update`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct LockFileRaw {
    nodes: HashMap<String, NodeRaw>,
}

#[derive(Debug, Deserialize)]
struct NodeRaw {
    #[serde(default)]
    locked: Option<LockedRaw>,
}

#[derive(Debug, Deserialize)]
struct LockedRaw {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    rev: Option<String>,
    #[serde(rename = "ref", default)]
    r#ref: Option<String>,
}

/// A resolved GitHub-hosted input entry from a flake.lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedInput {
    pub owner: String,
    pub repo: String,
    pub rev: String,
    /// Branch or tag the input tracks (e.g. "main"). Defaults to "main" when absent.
    pub tracked_ref: String,
}

/// In-memory view of a parsed flake.lock.
pub struct FlakeLock {
    nodes: HashMap<String, LockedInput>,
}

impl FlakeLock {
    /// Parse a flake.lock file from disk.
    pub fn read(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&content)
    }

    /// Parse a flake.lock from a JSON string.
    pub fn parse(content: &str) -> Result<Self> {
        let raw: LockFileRaw = serde_json::from_str(content)
            .context("parsing flake.lock as JSON")?;
        let mut nodes = HashMap::new();
        for (name, node) in raw.nodes {
            let Some(locked) = node.locked else { continue };
            if locked.kind.as_deref() != Some("github") {
                continue;
            }
            let (Some(owner), Some(repo), Some(rev)) =
                (locked.owner, locked.repo, locked.rev)
            else {
                continue;
            };
            nodes.insert(
                name,
                LockedInput {
                    owner,
                    repo,
                    rev,
                    tracked_ref: locked.r#ref.unwrap_or_else(|| "main".to_string()),
                },
            );
        }
        Ok(Self { nodes })
    }

    /// Look up a GitHub-hosted input by its flake input name.
    /// Returns `None` if the input doesn't exist or isn't a github-type input.
    #[must_use]
    pub fn locked_input(&self, input_name: &str) -> Option<&LockedInput> {
        self.nodes.get(input_name)
    }

    /// Iterate over all GitHub-hosted inputs as `(input_name, LockedInput)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &LockedInput)> {
        self.nodes.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "nodes": {
        "blackmatter-shell": {
          "locked": {
            "lastModified": 1,
            "narHash": "sha256-xxx",
            "owner": "pleme-io",
            "repo": "blackmatter-shell",
            "rev": "caa5246abc",
            "ref": "main",
            "type": "github"
          },
          "original": {
            "owner": "pleme-io",
            "repo": "blackmatter-shell",
            "type": "github"
          }
        },
        "compass-nvim": {
          "locked": {
            "owner": "pleme-io",
            "repo": "compass.nvim",
            "rev": "deadbeef",
            "type": "github"
          }
        },
        "root": {
          "inputs": {}
        },
        "some-git-input": {
          "locked": {
            "type": "git",
            "url": "https://example.com/foo.git",
            "rev": "abc"
          }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    #[test]
    fn parses_github_inputs() {
        let lock = FlakeLock::parse(SAMPLE).unwrap();
        let shell = lock.locked_input("blackmatter-shell").unwrap();
        assert_eq!(shell.owner, "pleme-io");
        assert_eq!(shell.repo, "blackmatter-shell");
        assert_eq!(shell.rev, "caa5246abc");
        assert_eq!(shell.tracked_ref, "main");
    }

    #[test]
    fn preserves_repo_name_when_differs_from_input() {
        let lock = FlakeLock::parse(SAMPLE).unwrap();
        let c = lock.locked_input("compass-nvim").unwrap();
        assert_eq!(c.repo, "compass.nvim");
    }

    #[test]
    fn defaults_missing_ref_to_main() {
        let lock = FlakeLock::parse(SAMPLE).unwrap();
        let c = lock.locked_input("compass-nvim").unwrap();
        assert_eq!(c.tracked_ref, "main");
    }

    #[test]
    fn skips_non_github_inputs() {
        let lock = FlakeLock::parse(SAMPLE).unwrap();
        assert!(lock.locked_input("some-git-input").is_none());
    }

    #[test]
    fn skips_synthetic_root_node() {
        let lock = FlakeLock::parse(SAMPLE).unwrap();
        assert!(lock.locked_input("root").is_none());
    }

    #[test]
    fn unknown_input_returns_none() {
        let lock = FlakeLock::parse(SAMPLE).unwrap();
        assert!(lock.locked_input("nonexistent").is_none());
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(FlakeLock::parse("not json").is_err());
    }
}
