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

// ─── Extended view for the operator ────────────────────────────────
//
// `FlakeLock` above is intentionally narrow — read-only, GitHub-only,
// drops fields the watch loop doesn't need. The operator needs:
//   - narHash + lastModified (for write_pin to update atomically)
//   - the inputs/follows graph (for DAG edge derivation)
//   - non-GitHub input types preserved (so write_pin doesn't drop them)
//
// `ExtendedFlakeLock` adds those. Existing FlakeLock callers stay
// untouched.

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ExtendedLockFile {
    pub nodes: std::collections::BTreeMap<String, ExtendedNode>,
    pub root: String,
    pub version: u32,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ExtendedNode {
    /// Map of local-input-name → reference into `nodes` (either a
    /// direct node name string or a `follows` chain). Empty for leaf nodes.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub inputs: std::collections::BTreeMap<String, ExtendedInputRef>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<ExtendedLocked>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<ExtendedOriginal>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flake: Option<bool>,
}

/// The `original` block declares what the user *asked for* (URL +
/// optional ref/branch/tag). For tag-pinned inputs (e.g.
/// `github:pleme-io/typemill/v0.8.18-pleme.1`), the `locked` block
/// records only the resolved rev — `locked.ref` is empty. Discovery
/// must check upstream at `original.ref`, not at the default branch
/// HEAD, or it would falsely flag tags as advancing every time the
/// repo's main branch moves.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ExtendedOriginal {
    #[serde(rename = "type", default)]
    pub kind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum ExtendedInputRef {
    /// Direct reference to another node by name.
    Direct(String),
    /// `follows` chain — e.g. ["substrate", "nixpkgs"] = follows root.substrate.nixpkgs
    Follows(Vec<String>),
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ExtendedLocked {
    /// "github" | "git" | "tarball" | "path" | etc.
    #[serde(rename = "type")]
    pub kind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,

    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,

    #[serde(rename = "narHash", default, skip_serializing_if = "Option::is_none")]
    pub nar_hash: Option<String>,

    #[serde(rename = "lastModified", default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl ExtendedNode {
    /// The ref (branch/tag/HEAD) the user is *tracking*, derived
    /// from `original.ref` first (what was declared in flake.nix)
    /// then `locked.ref` (recorded at flake-update time) then "HEAD"
    /// as last resort. Tag-pinned inputs only have `original.ref`
    /// populated, so without this fallback discovery would query
    /// upstream's default branch and falsely flag tags as advancing.
    #[must_use]
    pub fn tracking_ref(&self) -> &str {
        if let Some(orig) = &self.original {
            if let Some(r) = orig.r#ref.as_deref() {
                if !r.is_empty() {
                    return r;
                }
            }
        }
        if let Some(locked) = &self.locked {
            if let Some(r) = locked.r#ref.as_deref() {
                if !r.is_empty() {
                    return r;
                }
            }
        }
        "HEAD"
    }
}

impl ExtendedLockFile {
    pub fn parse(content: &str) -> Result<Self> {
        serde_json::from_str(content).context("parsing flake.lock as JSON (extended)")
    }

    pub fn read(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&content)
    }

    /// Emit `(parent, child)` edges from the inputs graph. Both
    /// endpoints are node names (lookup keys into `self.nodes`).
    /// `Follows` chains resolve through the root node.
    #[must_use]
    pub fn edges(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (parent_name, parent_node) in &self.nodes {
            if parent_name == &self.root {
                // Root node's inputs are the top-level user-declared
                // inputs; we still emit edges from root so they
                // participate in the DAG.
            }
            for (_local_name, input_ref) in &parent_node.inputs {
                if let Some(child) = self.resolve_ref(input_ref) {
                    out.push((parent_name.clone(), child));
                }
            }
        }
        out
    }

    /// Direct root inputs — the entries that appear in the user's
    /// `flake.nix` `inputs.<name>` block. Returns `(local_name, node_name)`
    /// pairs where `local_name` is what `nix flake update --update-input`
    /// accepts and `node_name` is the lookup key into `self.nodes`.
    ///
    /// Critically excludes transitive lock entries (cargo deps,
    /// nested input pins, alias suffixes like `nixpkgs_2`) — those
    /// can't be directly bumped, so generating proposals for them is
    /// noise. `Follows` chains are also excluded since they resolve
    /// through other inputs and aren't independently advanceable.
    #[must_use]
    pub fn root_input_nodes(&self) -> Vec<(String, String)> {
        let Some(root) = self.nodes.get(&self.root) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (local_name, input_ref) in &root.inputs {
            if let ExtendedInputRef::Direct(node_name) = input_ref {
                out.push((local_name.clone(), node_name.clone()));
            }
        }
        out
    }

    /// Resolve an `inputs[*]` ref to the target node name.
    fn resolve_ref(&self, r: &ExtendedInputRef) -> Option<String> {
        match r {
            ExtendedInputRef::Direct(name) => Some(name.clone()),
            ExtendedInputRef::Follows(chain) => {
                let mut current = self.root.clone();
                for hop in chain {
                    let node = self.nodes.get(&current)?;
                    let next = node.inputs.get(hop)?;
                    current = match next {
                        ExtendedInputRef::Direct(n) => n.clone(),
                        ExtendedInputRef::Follows(_) => return None,
                    };
                }
                Some(current)
            }
        }
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
