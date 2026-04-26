//! `LockFormat` impl for Nix `flake.lock`.
//!
//! Phase 1 stub. Defers to the existing `crate::flake_lock::FlakeLock`
//! parser for what it covers (GitHub-hosted inputs only) and returns
//! best-effort answers for the rest. The real operator needs a richer
//! parse that captures:
//!   - non-GitHub input types (git+, tarball, path)
//!   - the `nodes[*].inputs[*].follows` graph for edge derivation
//!   - `narHash` and `lastModified` so a write_pin can update them
//!     atomically alongside `rev`
//!
//! Extending `flake_lock.rs` (additive — keeps existing `FlakeLock`
//! shape stable) is a follow-up before Phase 1 actually reconciles.

use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::path::Path;

use super::crds::FlakeRev;
use super::lock_format::LockFormat;
use crate::flake_lock::FlakeLock;

pub struct FlakeLockAdapter;

impl LockFormat for FlakeLockAdapter {
    type Pin = FlakeRev;

    fn lock_path(&self) -> &'static Path {
        Path::new("flake.lock")
    }

    fn parse(&self, contents: &str) -> Result<BTreeMap<String, FlakeRev>> {
        let lock = FlakeLock::parse(contents)?;
        let mut out = BTreeMap::new();
        for (name, locked) in lock.iter() {
            // narHash + lastModified aren't yet exposed by FlakeLock —
            // fill with sentinels until the parser is extended. The
            // operator's verification gate will catch any pin written
            // with sentinel hashes (nix verifies narHash on fetch).
            out.insert(
                name.clone(),
                FlakeRev {
                    url: format!("github:{}/{}", locked.owner, locked.repo),
                    rev: locked.rev.clone(),
                    nar_hash: String::new(),
                    last_modified: 0,
                },
            );
        }
        Ok(out)
    }

    fn edges(&self, _parsed: &BTreeMap<String, FlakeRev>) -> Vec<(String, String)> {
        // FlakeLock doesn't yet expose the inputs graph. Return empty
        // until the parser is extended. With no edges, the DAG planner
        // treats every input as a root — over-eager verification, but
        // safe (no false-greens).
        Vec::new()
    }

    fn write_pin(&self, _contents: &str, _target: &str, _new: &FlakeRev) -> Result<String> {
        // Defer until the extended parser is in place — writing a pin
        // requires updating narHash + lastModified atomically with rev,
        // and the current parser doesn't expose those fields.
        Err(anyhow!(
            "FlakeLockAdapter::write_pin not yet implemented \
             (needs extended flake_lock parser — see operator/mod.rs TODO)"
        ))
    }
}
