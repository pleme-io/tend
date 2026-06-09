//! `tend nixpkgs-align` — fleet nixpkgs alignment as a standing reconcile check.
//!
//! Every repo's nixpkgs should follow substrate's single pinned rev (the one
//! true version) so identical software builds once and is a cache hit
//! fleet-wide. This converts a repo's flake.nix to
//! `nixpkgs.follows = "substrate/nixpkgs"`, drops substrate's own
//! `inputs.nixpkgs.follows` (else a follow-cycle), pulls the pinned substrate,
//! and commits + pushes. Idempotent; never touches a dirty working tree (WIP).
//!
//! Sibling of the `flake_refresh` config concern — but where flake-refresh
//! bumps every input, this enforces the ONE invariant that nixpkgs == substrate.

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use regex::Regex;

use crate::flake_lock::FlakeLock;
use crate::git::GitOps;

const MSG: &str = "flake: follow substrate/nixpkgs (fleet nixpkgs alignment for cache reuse)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlignOutcome {
    Aligned,
    Skipped(String),
    Failed(String),
}

/// Convert a flake.nix to follow substrate/nixpkgs. Returns `Some(new)` when a
/// transformation applies, `None` when the repo isn't eligible (no substrate
/// input, not branch-pinned, or already following).
pub(crate) fn convert_flake(text: &str) -> Option<String> {
    let already = Regex::new(r#"nixpkgs\.follows\s*=\s*"substrate/nixpkgs""#).unwrap();
    if already.is_match(text) {
        return None;
    }
    let has_sub = Regex::new(r"substrate(\s*=\s*\{|\.url\s*=)").unwrap();
    if !has_sub.is_match(text) {
        return None;
    }
    let branch = Regex::new(
        r#"(?P<i>[ \t]*)nixpkgs\.url\s*=\s*"github:NixOS/nixpkgs/(nixos|release|unstable|master)[^"]*"\s*;"#,
    )
    .unwrap();
    if !branch.is_match(text) {
        return None;
    }
    // Edit 1: nixpkgs.url branch -> follows substrate/nixpkgs.
    let new = branch
        .replace(text, "${i}nixpkgs.follows = \"substrate/nixpkgs\";")
        .into_owned();

    // Edit 2: drop substrate's OWN `inputs.nixpkgs.follows = "nixpkgs";`.
    // The trailing newline is OPTIONAL so an inline single-line substrate block
    // (`substrate = { url = "..."; inputs.nixpkgs.follows = "nixpkgs"; };`) is
    // also rewritten — missing this left a `substrate/nixpkgs -> nixpkgs ->
    // substrate/nixpkgs` follow-cycle that fails `nix flake lock`.
    let np_follows =
        Regex::new(r#"[ \t]*inputs\.nixpkgs\.follows\s*=\s*"nixpkgs"\s*;[ \t]*\n?"#).unwrap();
    let sub_block = Regex::new(r"(?s)substrate\s*=\s*\{(?P<b>[^}]*)\}").unwrap();
    let new = if let Some(caps) = sub_block.captures(&new) {
        let m = caps.name("b").unwrap();
        let body2 = np_follows.replace_all(m.as_str(), "");
        let mut s = String::with_capacity(new.len());
        s.push_str(&new[..m.start()]);
        s.push_str(&body2);
        s.push_str(&new[m.end()..]);
        s
    } else {
        new
    };
    // The `substrate.inputs.nixpkgs.follows = "nixpkgs";` standalone form too.
    let standalone =
        Regex::new(r#"[ \t]*substrate\.inputs\.nixpkgs\.follows\s*=\s*"nixpkgs"\s*;[ \t]*\n?"#)
            .unwrap();
    let new = standalone.replace_all(&new, "").into_owned();

    if new == text { None } else { Some(new) }
}

/// Pull the pinned substrate (so nixpkgs == substrate's rev). `nix` has no
/// typed in-process equivalent; tend already invokes it via `Command`
/// (e.g. clone in sync.rs), so this is consistent — and it is `Command`, not a
/// shell, so there is no word-splitting/quoting fragility.
fn nix_update_substrate(repo: &Path) -> bool {
    let ran = |args: &[&str]| {
        Command::new("nix")
            .current_dir(repo)
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    ran(&["flake", "update", "substrate"]) || ran(&["flake", "lock"])
}

/// The repo's locked nixpkgs rev, via the typed `FlakeLock` (not raw json).
fn locked_nixpkgs_rev(repo: &Path) -> Option<String> {
    FlakeLock::read(&repo.join("flake.lock"))
        .ok()?
        .locked_input("nixpkgs")
        .map(|i| i.rev.clone())
}

/// substrate's pinned nixpkgs rev — the canonical "one true version" every
/// repo must converge to. Read once by the orchestrator from the substrate repo.
pub(crate) fn substrate_canonical_rev(substrate_dir: &Path) -> Option<String> {
    locked_nixpkgs_rev(substrate_dir)
}

/// Align one repo to follow substrate's pinned nixpkgs. Idempotent; never
/// touches a dirty tree (WIP is sacred). `canonical_rev` is substrate's nixpkgs
/// rev (read once by the orchestrator) — the post-align lock must match it.
/// Git is driven through the typed `GitOps` trait (mockable, no shell).
pub(crate) fn align_one_repo(
    repo: &Path,
    canonical_rev: &str,
    git: &dyn GitOps,
) -> Result<AlignOutcome> {
    let flake = repo.join("flake.nix");
    if !flake.exists() {
        return Ok(AlignOutcome::Skipped("no flake.nix".into()));
    }
    if !git.is_clean(repo)? {
        return Ok(AlignOutcome::Skipped("dirty".into()));
    }
    let Some(converted) = convert_flake(&std::fs::read_to_string(&flake)?) else {
        return Ok(AlignOutcome::Skipped("not eligible".into()));
    };
    let flake_rel = Path::new("flake.nix");
    let lock_rel = Path::new("flake.lock");
    let restore = || git.restore(repo, &[flake_rel, lock_rel]);

    std::fs::write(&flake, &converted)?;
    if !nix_update_substrate(repo) {
        let _ = restore();
        return Ok(AlignOutcome::Failed("lock-failed".into()));
    }
    match locked_nixpkgs_rev(repo) {
        Some(rev) if rev.starts_with(canonical_rev) => {}
        other => {
            let _ = restore();
            return Ok(AlignOutcome::Failed(format!(
                "wrong-rev:{}",
                other.unwrap_or_default()
            )));
        }
    }
    git.add(repo, flake_rel)?;
    git.add(repo, lock_rel)?;
    if !git.has_staged_changes(repo)? {
        return Ok(AlignOutcome::Skipped("no-op".into()));
    }
    git.commit(repo, MSG)?;
    if git.push(repo).is_ok() {
        return Ok(AlignOutcome::Aligned);
    }
    // Drifted from origin. tend's reconcile syncs before aligning, so this is
    // rare; ff-pull then re-push. If the branch genuinely diverged the pull is
    // a no-op and the push stays failed — the alignment commit remains locally
    // (clean tree, one pending commit), and the next reconcile picks it up.
    let branch = git.current_branch(repo)?;
    let _ = git.pull(repo, &branch);
    if git.push(repo).is_ok() {
        Ok(AlignOutcome::Aligned)
    } else {
        Ok(AlignOutcome::Failed("push".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_block_converts_and_breaks_cycle() {
        let t = "  inputs = {\n    nixpkgs.url = \"github:NixOS/nixpkgs/nixos-25.11\";\n    substrate = {\n      url = \"github:pleme-io/substrate\";\n      inputs.nixpkgs.follows = \"nixpkgs\";\n    };\n  };\n";
        let out = convert_flake(t).unwrap();
        assert!(out.contains("nixpkgs.follows = \"substrate/nixpkgs\""));
        assert!(!out.contains("inputs.nixpkgs.follows = \"nixpkgs\""));
    }

    #[test]
    fn inline_single_line_block_converts_and_breaks_cycle() {
        // The exact shape that caused the follow-cycle in the first sweep.
        let t = "  inputs = {\n    nixpkgs.url = \"github:NixOS/nixpkgs/nixos-25.11\";\n    substrate = { url = \"github:pleme-io/substrate\"; inputs.nixpkgs.follows = \"nixpkgs\"; };\n  };\n";
        let out = convert_flake(t).unwrap();
        assert!(out.contains("nixpkgs.follows = \"substrate/nixpkgs\""));
        assert!(
            !out.contains("inputs.nixpkgs.follows = \"nixpkgs\""),
            "inline substrate follow must be removed (else a cycle):\n{out}"
        );
    }

    #[test]
    fn already_following_is_skipped() {
        let t = "    nixpkgs.follows = \"substrate/nixpkgs\";\n    substrate.url = \"x\";";
        assert!(convert_flake(t).is_none());
    }

    #[test]
    fn no_substrate_input_is_skipped() {
        let t = "    nixpkgs.url = \"github:NixOS/nixpkgs/nixos-25.11\";";
        assert!(convert_flake(t).is_none());
    }
}
