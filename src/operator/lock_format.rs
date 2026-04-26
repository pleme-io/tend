//! Generic lock-file abstraction.
//!
//! Implemented once per pinned-version surface in the fleet:
//! `flake.lock`, `Cargo.lock`, `HelmRelease`, container image tags.
//! The DAG planner consumes only this trait — no domain-specific
//! logic leaks into the orchestration layer.

use anyhow::Result;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub trait LockFormat: Send + Sync {
    type Pin: Clone + Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug;

    fn lock_path(&self) -> &'static Path;

    fn parse(&self, contents: &str) -> Result<BTreeMap<String, Self::Pin>>;

    /// Dependency edges within the lock file. For flake.lock this is
    /// the `nodes[*].inputs[*].follows` graph; for Cargo.lock it's the
    /// resolved deps; for HelmRelease the chart sourceRef chain.
    /// Returns `(from_target, to_target)` pairs.
    fn edges(&self, parsed: &BTreeMap<String, Self::Pin>) -> Vec<(String, String)>;

    /// Return new file contents with `target`'s pin replaced.
    /// Implementations must:
    ///   - preserve unrelated content byte-for-byte where possible
    ///   - update any computed integrity hashes (narHash, checksum) to
    ///     match the new pin
    /// Caller is responsible for the atomic file write (tmpfile + rename).
    fn write_pin(&self, contents: &str, target: &str, new: &Self::Pin) -> Result<String>;
}
