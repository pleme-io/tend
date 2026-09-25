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
//!
//! ★ An INPUT name is not a NODE name. Nix names lock nodes in the order it
//! walks the graph, so a transitive dependency can take the plain name
//! (`nixpkgs`) while the root's own input lands on a suffixed node
//! (`nixpkgs_15`). Measured 2026-09-25 in a real lock: node `nixpkgs` was
//! crate2nix's 2025-12-08 pin, the root's `nixpkgs` was `nixpkgs_15`. So a
//! lookup by input name resolves through the root node's `inputs` map
//! (following `follows` chains), never by node key.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// A resolved GitHub-hosted input entry from a flake.lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedInput {
    pub owner: String,
    pub repo: String,
    pub rev: String,
    /// Branch or tag the input tracks, as declared (`original.ref`; nix never
    /// records a ref in `locked` for a github input). "main" when undeclared.
    pub tracked_ref: String,
}

/// In-memory view of a parsed flake.lock: the ROOT's inputs, by input name.
pub struct FlakeLock {
    inputs: HashMap<String, LockedInput>,
}

impl FlakeLock {
    /// Parse a flake.lock file from disk.
    pub fn read(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&content)
    }

    /// Parse a flake.lock from a JSON string.
    pub fn parse(content: &str) -> Result<Self> {
        Ok(Self::from_extended(&ExtendedLockFile::parse(content)?))
    }

    /// The narrow view over the one typed parse: each root input resolved to
    /// the node it locks to, kept when that node is a github input.
    fn from_extended(lock: &ExtendedLockFile) -> Self {
        let mut inputs = HashMap::new();
        for name in lock.root_input_names() {
            let Some(node) = lock.root_input(&name) else {
                continue;
            };
            let Some(locked) = &node.locked else { continue };
            if locked.kind != "github" {
                continue;
            }
            let (Some(owner), Some(repo), Some(rev)) =
                (locked.owner.clone(), locked.repo.clone(), locked.rev.clone())
            else {
                continue;
            };
            let tracked_ref = node
                .original
                .as_ref()
                .and_then(|o| o.r#ref.clone())
                .or_else(|| locked.r#ref.clone())
                .unwrap_or_else(|| "main".to_string());
            inputs.insert(
                name,
                LockedInput {
                    owner,
                    repo,
                    rev,
                    tracked_ref,
                },
            );
        }
        Self { inputs }
    }

    /// Look up a GitHub-hosted input by its flake input name (a key of the
    /// root's `inputs`, `follows` resolved). Returns `None` if the root
    /// declares no such input or it isn't a github-type input.
    #[must_use]
    pub fn locked_input(&self, input_name: &str) -> Option<&LockedInput> {
        self.inputs.get(input_name)
    }

    /// Iterate over all GitHub-hosted inputs as `(input_name, LockedInput)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &LockedInput)> {
        self.inputs.iter()
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

    /// A commit the declaration pins (`github:o/r/<sha>` or `?rev=<sha>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,

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

    #[serde(
        rename = "lastModified",
        default,
        skip_serializing_if = "Option::is_none"
    )]
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
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
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

    /// Every input the root declares, `follows` ones included.
    #[must_use]
    pub fn root_input_names(&self) -> Vec<String> {
        self.nodes
            .get(&self.root)
            .map(|root| root.inputs.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// How the root declares input `name`: a node, or a `follows` path.
    #[must_use]
    pub fn root_input_ref(&self, name: &str) -> Option<&ExtendedInputRef> {
        self.nodes.get(&self.root)?.inputs.get(name)
    }

    /// The node root input `name` locks to, `follows` chains resolved.
    #[must_use]
    pub fn root_input(&self, name: &str) -> Option<&ExtendedNode> {
        let node = self.resolve_ref(self.root_input_ref(name)?)?;
        self.nodes.get(&node)
    }

    /// Resolve an `inputs[*]` ref to the target node name.
    fn resolve_ref(&self, r: &ExtendedInputRef) -> Option<String> {
        self.resolve_bounded(r, self.nodes.len())
    }

    /// A `follows` path is walked from the root, and a hop may itself be a
    /// `follows` (a follow of a follow). `budget` bounds the walk so a cyclic
    /// lock resolves to `None` instead of looping.
    fn resolve_bounded(&self, r: &ExtendedInputRef, budget: usize) -> Option<String> {
        match r {
            ExtendedInputRef::Direct(name) => Some(name.clone()),
            ExtendedInputRef::Follows(chain) => {
                let budget = budget.checked_sub(1)?;
                let mut current = self.root.clone();
                for hop in chain {
                    let next = self.nodes.get(&current)?.inputs.get(hop)?;
                    current = self.resolve_bounded(next, budget)?;
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
          "inputs": {
            "blackmatter-shell": "blackmatter-shell",
            "compass-nvim": "compass-nvim",
            "some-git-input": "some-git-input"
          }
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

    /// The shape of a real lock (nupastel, 2026-09-25): a transitive
    /// dependency holds the plain node name `nixpkgs`, the root's own input
    /// is `nixpkgs_2`, and a third input follows through `substrate`.
    const SHADOWED: &str = r#"{
      "nodes": {
        "crate2nix": { "inputs": { "nixpkgs": "nixpkgs" },
          "locked": { "type": "github", "owner": "nix-community", "repo": "crate2nix", "rev": "c2n" } },
        "nixpkgs": {
          "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "old-transitive" },
          "original": { "type": "github", "owner": "NixOS", "ref": "nixos-unstable", "repo": "nixpkgs" } },
        "nixpkgs_2": {
          "locked": { "type": "github", "owner": "nixos", "repo": "nixpkgs", "rev": "root-own" },
          "original": { "type": "github", "owner": "nixos", "ref": "nixos-25.11", "repo": "nixpkgs" } },
        "nixpkgs_3": {
          "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "substrate-pin" },
          "original": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "substrate-pin" } },
        "substrate": { "inputs": { "nixpkgs": "nixpkgs_3" },
          "locked": { "type": "github", "owner": "pleme-io", "repo": "substrate", "rev": "sub" } },
        "blue": { "inputs": { "nixpkgs": ["substrate", "nixpkgs"], "crate2nix": "crate2nix" },
          "locked": { "type": "github", "owner": "pleme-io", "repo": "blue", "rev": "b" } },
        "root": { "inputs": {
          "crate2nix": "crate2nix",
          "nixpkgs": "nixpkgs_2",
          "substrate": "substrate",
          "blue": "blue",
          "pkgs-via-blue": ["blue", "nixpkgs"],
          "pkgs-via-substrate": ["substrate", "nixpkgs"]
        } }
      },
      "root": "root",
      "version": 7
    }"#;

    #[test]
    fn an_input_name_resolves_through_the_root_not_the_node_key() {
        let lock = FlakeLock::parse(SHADOWED).unwrap();
        let np = lock.locked_input("nixpkgs").unwrap();
        assert_eq!(np.rev, "root-own", "read the transitive node that holds the plain name");
        assert_eq!(np.tracked_ref, "nixos-25.11", "the declared branch lives in `original`");
    }

    #[test]
    fn a_follows_input_resolves_to_the_node_it_follows() {
        let lock = FlakeLock::parse(SHADOWED).unwrap();
        assert_eq!(lock.locked_input("pkgs-via-substrate").unwrap().rev, "substrate-pin");
        // A follow of a follow: blue's nixpkgs itself follows substrate/nixpkgs.
        assert_eq!(lock.locked_input("pkgs-via-blue").unwrap().rev, "substrate-pin");
    }

    #[test]
    fn a_cyclic_follows_resolves_to_nothing() {
        let cyclic = r#"{ "nodes": {
            "a": { "inputs": { "x": ["b", "x"] } },
            "b": { "inputs": { "x": ["a", "x"] } },
            "root": { "inputs": { "a": "a", "b": "b", "x": ["a", "x"] } } },
          "root": "root", "version": 7 }"#;
        let lock = ExtendedLockFile::parse(cyclic).unwrap();
        assert!(lock.root_input("x").is_none());
        assert!(FlakeLock::parse(cyclic).unwrap().locked_input("x").is_none());
    }

    #[test]
    fn a_pinned_commit_is_recorded_in_original() {
        let lock = ExtendedLockFile::parse(SHADOWED).unwrap();
        let sub = lock.root_input("pkgs-via-substrate").unwrap();
        assert_eq!(sub.original.as_ref().unwrap().rev.as_deref(), Some("substrate-pin"));
    }
}
