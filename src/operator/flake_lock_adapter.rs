//! `LockFormat` impl for Nix `flake.lock`.
//!
//! Uses `crate::flake_lock::ExtendedLockFile` (the operator-extended
//! parser that captures narHash, lastModified, and the inputs/follows
//! graph). The narrow `FlakeLock` parser stays for the existing watch
//! daemon.

use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

use super::crds::FlakeRev;
use super::lock_format::LockFormat;
use crate::flake_lock::ExtendedLockFile;

pub struct FlakeLockAdapter;

impl FlakeLockAdapter {
    /// Format an `ExtendedLocked` block as a flake-input URL string,
    /// matching what `nix flake metadata` prints.
    fn format_url(locked: &crate::flake_lock::ExtendedLocked) -> String {
        match locked.kind.as_str() {
            "github" => match (&locked.owner, &locked.repo) {
                (Some(o), Some(r)) => format!("github:{o}/{r}"),
                _ => locked.url.clone().unwrap_or_default(),
            },
            "git" | "tarball" | "file" => locked.url.clone().unwrap_or_default(),
            "path" => locked.url.clone().unwrap_or_default(),
            other => locked.url.clone().unwrap_or_else(|| format!("{other}:?")),
        }
    }
}

impl LockFormat for FlakeLockAdapter {
    type Pin = FlakeRev;

    fn lock_path(&self) -> &'static Path {
        Path::new("flake.lock")
    }

    fn parse(&self, contents: &str) -> Result<BTreeMap<String, FlakeRev>> {
        let lock = ExtendedLockFile::parse(contents)?;
        let mut out = BTreeMap::new();
        for (name, node) in &lock.nodes {
            if name == &lock.root {
                continue;
            }
            let Some(locked) = &node.locked else { continue };
            let Some(rev) = locked.rev.clone() else {
                continue;
            };
            out.insert(
                name.clone(),
                FlakeRev {
                    url: Self::format_url(locked),
                    rev,
                    nar_hash: locked.nar_hash.clone().unwrap_or_default(),
                    last_modified: locked.last_modified.unwrap_or(0),
                },
            );
        }
        Ok(out)
    }

    fn edges(&self, _parsed: &BTreeMap<String, FlakeRev>) -> Vec<(String, String)> {
        // _parsed is the flat pin map; edges live in the original JSON.
        // Caller pattern: parse() once for pins, then call edges_from_raw()
        // separately when graph is needed. Default impl returns empty;
        // see `edges_from_raw` below for the real method.
        Vec::new()
    }

    fn write_pin(&self, contents: &str, target: &str, new: &FlakeRev) -> Result<String> {
        let mut value: serde_json::Value =
            serde_json::from_str(contents).context("parsing flake.lock as JSON")?;

        // Navigate nodes.<target>.locked and replace fields in-place,
        // preserving formatting and untouched neighbors.
        let nodes = value
            .get_mut("nodes")
            .and_then(|n| n.as_object_mut())
            .ok_or_else(|| anyhow!("flake.lock: missing or invalid `nodes` map"))?;

        let node = nodes
            .get_mut(target)
            .ok_or_else(|| anyhow!("flake.lock: no node named `{target}`"))?
            .as_object_mut()
            .ok_or_else(|| anyhow!("flake.lock: node `{target}` is not an object"))?;

        let locked = node
            .get_mut("locked")
            .ok_or_else(|| {
                anyhow!(
                    "flake.lock: node `{target}` has no `locked` block — \
                                    refusing to write a pin to a non-locked input"
                )
            })?
            .as_object_mut()
            .ok_or_else(|| anyhow!("flake.lock: `locked` for `{target}` is not an object"))?;

        locked.insert("rev".into(), serde_json::Value::String(new.rev.clone()));
        if !new.nar_hash.is_empty() {
            locked.insert(
                "narHash".into(),
                serde_json::Value::String(new.nar_hash.clone()),
            );
        }
        if new.last_modified > 0 {
            locked.insert(
                "lastModified".into(),
                serde_json::Value::Number(new.last_modified.into()),
            );
        }

        // Pretty-print with 2-space indent to match Nix's own output.
        let mut buf = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"  ");
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
        value
            .serialize(&mut ser)
            .context("serializing flake.lock JSON")?;
        let mut out = String::from_utf8(buf).context("flake.lock not UTF-8")?;
        // Nix appends a trailing newline; preserve that convention.
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}

impl FlakeLockAdapter {
    /// Edges derived from the raw lock contents — companion to `parse()`
    /// because the trait's `edges()` signature only sees the pin map.
    pub fn edges_from_raw(&self, contents: &str) -> Result<Vec<(String, String)>> {
        let lock = ExtendedLockFile::parse(contents)?;
        Ok(lock.edges())
    }
}

use serde::Serialize;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "nodes": {
        "root": {
          "inputs": {
            "substrate": "substrate",
            "nixpkgs": "nixpkgs"
          }
        },
        "substrate": {
          "inputs": { "nixpkgs": ["nixpkgs"] },
          "locked": {
            "lastModified": 1700000000,
            "narHash": "sha256-old=",
            "owner": "pleme-io",
            "repo": "substrate",
            "rev": "abc123",
            "type": "github"
          },
          "original": { "owner": "pleme-io", "repo": "substrate", "type": "github" }
        },
        "nixpkgs": {
          "locked": {
            "lastModified": 1700000001,
            "narHash": "sha256-nixpkgs=",
            "owner": "nixos",
            "repo": "nixpkgs",
            "rev": "def456",
            "type": "github"
          },
          "original": { "owner": "nixos", "repo": "nixpkgs", "type": "github" }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    #[test]
    fn parses_pins() {
        let pins = FlakeLockAdapter.parse(SAMPLE).unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins["substrate"].rev, "abc123");
        assert_eq!(pins["substrate"].url, "github:pleme-io/substrate");
        assert_eq!(pins["nixpkgs"].rev, "def456");
    }

    #[test]
    fn derives_edges_with_follows_resolution() {
        let edges = FlakeLockAdapter.edges_from_raw(SAMPLE).unwrap();
        // root → substrate, root → nixpkgs, substrate → nixpkgs (via follows)
        assert!(edges.contains(&("root".into(), "substrate".into())));
        assert!(edges.contains(&("root".into(), "nixpkgs".into())));
        assert!(edges.contains(&("substrate".into(), "nixpkgs".into())));
    }

    #[test]
    fn write_pin_updates_rev_and_hash() {
        let new = FlakeRev {
            url: "github:pleme-io/substrate".into(),
            rev: "fedcba".into(),
            nar_hash: "sha256-new=".into(),
            last_modified: 1700001000,
        };
        let updated = FlakeLockAdapter
            .write_pin(SAMPLE, "substrate", &new)
            .unwrap();
        assert!(updated.contains(r#""rev": "fedcba""#));
        assert!(updated.contains(r#""narHash": "sha256-new=""#));
        assert!(updated.contains(r#""lastModified": 1700001000"#));
        // Untouched node stays intact.
        assert!(updated.contains(r#""rev": "def456""#));
    }

    #[test]
    fn write_pin_unknown_target_errors() {
        let new = FlakeRev {
            url: "x".into(),
            rev: "y".into(),
            nar_hash: "z".into(),
            last_modified: 1,
        };
        let err = FlakeLockAdapter
            .write_pin(SAMPLE, "nonexistent", &new)
            .unwrap_err();
        assert!(err.to_string().contains("no node named"));
    }

    /// Smoke against the real pleme-io/nix flake.lock — skipped when
    /// the path doesn't exist (CI / clean checkouts won't have it).
    #[test]
    fn smoke_real_pleme_io_nix_flake_lock() {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::path::PathBuf::from(home).join("code/github/pleme-io/nix/flake.lock");
        if !path.exists() {
            eprintln!("smoke skipped: {} does not exist", path.display());
            return;
        }
        let contents = std::fs::read_to_string(&path).unwrap();

        let pins = FlakeLockAdapter
            .parse(&contents)
            .expect("parse real flake.lock");
        assert!(
            pins.len() >= 10,
            "expected ≥10 GitHub pins, got {}",
            pins.len()
        );
        assert!(pins.contains_key("nixpkgs"), "nixpkgs missing");

        let edges = FlakeLockAdapter
            .edges_from_raw(&contents)
            .expect("derive edges");
        assert!(
            edges.len() >= 5,
            "expected ≥5 follows edges, got {}",
            edges.len()
        );

        // Spot-check at least one substrate-rooted edge if substrate is pinned.
        if pins.contains_key("substrate") {
            let has_substrate_edge = edges.iter().any(|(p, _)| p == "substrate" || p == "root");
            assert!(has_substrate_edge);
        }
    }
}
