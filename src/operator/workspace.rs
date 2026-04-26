//! Resolve a `RepoRef` into an on-disk path using tend's workspace config.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

use super::crds::RepoRef;
use crate::config::Config;

/// Resolve a `RepoRef` to its working-tree directory on this host.
///
/// Reuses tend's existing config — every workspace declares
/// `base_dir`, repos clone under `<base_dir>/<repo>`. The operator
/// pod mounts `~/.config/tend/config.yaml` from the host so this
/// resolution matches what the host's tend daemon would do.
pub fn resolve_repo_dir(cfg: &Config, r: &RepoRef) -> Result<PathBuf> {
    let ws = cfg
        .workspaces
        .iter()
        .find(|w| w.name == r.workspace)
        .ok_or_else(|| anyhow!("workspace `{}` not in tend config", r.workspace))?;
    let base = shellexpand::tilde(&ws.base_dir).into_owned();
    Ok(PathBuf::from(base).join(&r.repo))
}
