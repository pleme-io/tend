//! `tend nixpkgs-align` — fleet nixpkgs alignment as a standing reconcile check.
//!
//! Every repo's nixpkgs should follow substrate's single pinned rev (the one
//! true version) so identical software builds once and is a cache hit
//! fleet-wide. This converts a repo's flake.nix to
//! `nixpkgs.follows = "substrate/nixpkgs"`, drops substrate's own
//! `inputs.nixpkgs.follows` (else a follow-cycle), pulls the pinned substrate,
//! and commits + pushes. Idempotent; never touches a dirty working tree (WIP).
//! It PLANS by default and writes only with `--apply`: one run can push to
//! hundreds of repos' main.
//!
//! Sibling of the `flake_refresh` config concern — but where flake-refresh
//! bumps every input, this enforces the ONE invariant that nixpkgs == substrate.
//!
//! ★ Eligibility is read from the LOCK, never guessed from the text. Nix has
//! already parsed every spelling of the input URL (`github:NixOS/nixpkgs/x`,
//! `github:nixos/nixpkgs?ref=x`, no ref at all) into typed `original` fields.
//! The first version matched one spelling with a regex and classed every other
//! one "not eligible" — measured 2026-09-25, 49 fleet flakes (41 with a
//! substrate input, blue among them) were skipped that way every run, silently,
//! because a skip was one uncounted number. So every repo now gets a typed
//! reason, and a repo the aligner cannot rewrite is reported by name.

use std::fmt;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use regex::Regex;

use crate::flake_lock::{ExtendedInputRef, ExtendedLockFile, ExtendedOriginal, FlakeLock};
use crate::git::GitOps;

const MSG: &str = "flake: follow substrate/nixpkgs (fleet nixpkgs alignment for cache reuse)";

/// Why a repo is, or is not, convertible, read from its flake.lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Eligibility {
    /// The root's nixpkgs is GitHub nixos/nixpkgs on a movable ref (a branch,
    /// or none) and the flake has a substrate input to follow.
    Movable,
    /// Converged: nixpkgs already follows substrate/nixpkgs.
    FollowsSubstrate,
    /// Follows another input's nixpkgs (`blue/nixpkgs`); that input's owner
    /// aligns, and this repo inherits it.
    FollowsOther(String),
    /// Pinned to a commit on purpose (substrate's own tuple is one).
    PinnedCommit,
    NoSubstrateInput,
    NoNixpkgsInput,
    /// A nixpkgs input nix locked as something other than GitHub nixos/nixpkgs.
    NotGithubNixpkgs(String),
}

impl fmt::Display for Eligibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Movable => f.write_str("movable"),
            Self::FollowsSubstrate => f.write_str("follows substrate/nixpkgs"),
            Self::FollowsOther(path) => write!(f, "follows {path}"),
            Self::PinnedCommit => f.write_str("pinned to a commit"),
            Self::NoSubstrateInput => f.write_str("no substrate input"),
            Self::NoNixpkgsInput => f.write_str("no nixpkgs input"),
            Self::NotGithubNixpkgs(what) => write!(f, "nixpkgs is {what}"),
        }
    }
}

/// Classify a repo from its lock: how the root declares `nixpkgs`, and what
/// nix recorded the declaration as.
pub(crate) fn classify(lock: &ExtendedLockFile) -> Eligibility {
    let Some(declared) = lock.root_input_ref("nixpkgs") else {
        return Eligibility::NoNixpkgsInput;
    };
    if let ExtendedInputRef::Follows(path) = declared {
        return if path.iter().map(String::as_str).eq(["substrate", "nixpkgs"]) {
            Eligibility::FollowsSubstrate
        } else {
            Eligibility::FollowsOther(path.join("/"))
        };
    }
    if lock.root_input_ref("substrate").is_none() {
        return Eligibility::NoSubstrateInput;
    }
    let Some(original) = lock.root_input("nixpkgs").and_then(|n| n.original.as_ref()) else {
        return Eligibility::NotGithubNixpkgs("locked with no declaration".into());
    };
    if !is_github_nixpkgs(original) {
        return Eligibility::NotGithubNixpkgs(describe(original));
    }
    let names_commit = original.r#ref.as_deref().is_some_and(is_commit);
    if original.rev.is_some() || names_commit {
        return Eligibility::PinnedCommit;
    }
    Eligibility::Movable
}

/// GitHub owners and repos are case-insensitive: `nixos` is `NixOS`.
fn is_github_nixpkgs(o: &ExtendedOriginal) -> bool {
    let is = |field: &Option<String>, want: &str| {
        field.as_deref().is_some_and(|v| v.eq_ignore_ascii_case(want))
    };
    o.kind == "github" && is(&o.owner, "nixos") && is(&o.repo, "nixpkgs")
}

fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn describe(o: &ExtendedOriginal) -> String {
    match (&o.owner, &o.repo, &o.url) {
        (Some(owner), Some(repo), _) => {
            [o.kind.as_str(), ":", owner.as_str(), "/", repo.as_str()].concat()
        }
        (_, _, Some(url)) => [o.kind.as_str(), ":", url.as_str()].concat(),
        _ => o.kind.clone(),
    }
}

/// Why a movable repo's flake.nix could not be rewritten. Reported by name:
/// it is a blind spot of the rewriter, never a clean skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RewriteRefusal {
    /// No `nixpkgs.url = "…";` line: the block form `nixpkgs = { url = …; }`
    /// or an inline `inputs = { … };`.
    NoDeclarationLine,
    SeveralDeclarationLines(usize),
}

impl fmt::Display for RewriteRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDeclarationLine => {
                f.write_str("no `nixpkgs.url = \"…\";` line (block or inline form)")
            }
            Self::SeveralDeclarationLines(n) => write!(f, "{n} `nixpkgs.url` lines"),
        }
    }
}

/// Rewrite a movable repo's flake.nix to follow substrate/nixpkgs. Whatever
/// the URL's spelling — the lock already said it is a movable nixpkgs — the
/// one declaration line becomes a follows.
pub(crate) fn rewrite_flake(text: &str) -> std::result::Result<String, RewriteRefusal> {
    let declaration =
        Regex::new(r#"(?m)^(?P<lead>[ \t]*(?:inputs\.)?)nixpkgs\.url\s*=\s*"[^"]*"\s*;"#).unwrap();
    match declaration.find_iter(text).count() {
        1 => {}
        0 => return Err(RewriteRefusal::NoDeclarationLine),
        n => return Err(RewriteRefusal::SeveralDeclarationLines(n)),
    }
    // Edit 1: the nixpkgs declaration -> follows substrate/nixpkgs.
    let new = declaration
        .replace(text, "${lead}nixpkgs.follows = \"substrate/nixpkgs\";")
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
    // The standalone form, anchored to the line so an `inputs.` prefix goes
    // with it rather than being left dangling.
    let standalone = Regex::new(
        r#"(?m)^[ \t]*(?:inputs\.)?substrate\.inputs\.nixpkgs\.follows\s*=\s*"nixpkgs"\s*;[ \t]*\n?"#,
    )
    .unwrap();
    Ok(standalone.replace_all(&new, "").into_owned())
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

/// The rev the repo's root `nixpkgs` input locks to, `follows` resolved.
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

/// What aligning one repo did, or in plan mode would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlignOutcome {
    Aligned,
    /// Plan mode: convertible, nothing written.
    WouldAlign,
    Skipped(Skip),
    /// Movable, but its flake.nix has a shape the rewriter cannot edit.
    Unrewritable(RewriteRefusal),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skip {
    NoFlake,
    /// A flake with no readable lock cannot be classified.
    NoLock,
    Dirty,
    Ineligible(Eligibility),
    NoOp,
}

/// Align one repo to follow substrate's pinned nixpkgs. Idempotent; never
/// touches a dirty tree (WIP is sacred). `canonical_rev` is substrate's nixpkgs
/// rev (read once by the orchestrator) — the post-align lock must match it.
/// With `apply` false nothing is written. Git is driven through the typed
/// `GitOps` trait (mockable, no shell).
pub(crate) fn align_one_repo(
    repo: &Path,
    canonical_rev: &str,
    git: &dyn GitOps,
    apply: bool,
) -> Result<AlignOutcome> {
    let flake = repo.join("flake.nix");
    if !flake.exists() {
        return Ok(AlignOutcome::Skipped(Skip::NoFlake));
    }
    if !git.is_clean(repo)? {
        return Ok(AlignOutcome::Skipped(Skip::Dirty));
    }
    let Ok(lock) = ExtendedLockFile::read(&repo.join("flake.lock")) else {
        return Ok(AlignOutcome::Skipped(Skip::NoLock));
    };
    match classify(&lock) {
        Eligibility::Movable => {}
        other => return Ok(AlignOutcome::Skipped(Skip::Ineligible(other))),
    }
    let converted = match rewrite_flake(&std::fs::read_to_string(&flake)?) {
        Ok(text) => text,
        Err(refusal) => return Ok(AlignOutcome::Unrewritable(refusal)),
    };
    if !apply {
        return Ok(AlignOutcome::WouldAlign);
    }
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
            return Ok(AlignOutcome::Failed(
                ["wrong-rev:", other.unwrap_or_default().as_str()].concat(),
            ));
        }
    }
    // ── ★ EVALUATE BEFORE PUSHING A flake.nix REWRITE ────────────────────
    // This path REGEX-REWRITES flake.nix and then pushed straight to the
    // checked-out branch. `nix flake update`/`lock` resolves inputs without
    // evaluating `outputs`, so a rewrite that produces a flake nix cannot
    // evaluate passed every check here and reached main — the same shape
    // that stopped every reconciler in the fleet on 2026-08-03.
    //
    // Same gate the flake.lock path uses (`flake::verify_flake_evaluates`),
    // deliberately shared rather than re-implemented: this path was missing
    // it precisely because the rule lived inline in the other one.
    if let Some(reason) = crate::flake::verify_flake_evaluates(repo) {
        let _ = restore();
        return Ok(AlignOutcome::Failed(["does-not-evaluate:", reason.as_str()].concat()));
    }
    git.add(repo, flake_rel)?;
    git.add(repo, lock_rel)?;
    if !git.has_staged_changes(repo)? {
        return Ok(AlignOutcome::Skipped(Skip::NoOp));
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

/// The run's tally. Every outcome lands in exactly one row, and the rows that
/// need a person carry names, so "skipped" is never one number hiding a blind
/// spot.
#[derive(Debug, Default)]
pub(crate) struct AlignReport {
    pub aligned: Vec<String>,
    pub would_align: Vec<String>,
    pub converged: usize,
    /// Ineligible by design (pinned, follows another input, no substrate, …).
    pub by_design: usize,
    pub dirty: Vec<String>,
    /// Could not be classified or rewritten: the aligner's blind spots.
    pub blind: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub not_flakes: usize,
}

impl AlignReport {
    pub(crate) fn record(&mut self, repo: &str, outcome: Result<AlignOutcome>) {
        let name = repo.to_string();
        match outcome {
            Ok(AlignOutcome::Aligned) => self.aligned.push(name),
            Ok(AlignOutcome::WouldAlign) => self.would_align.push(name),
            Ok(AlignOutcome::Skipped(Skip::Ineligible(Eligibility::FollowsSubstrate)))
            | Ok(AlignOutcome::Skipped(Skip::NoOp)) => self.converged += 1,
            Ok(AlignOutcome::Skipped(Skip::Ineligible(_))) => self.by_design += 1,
            Ok(AlignOutcome::Skipped(Skip::Dirty)) => self.dirty.push(name),
            Ok(AlignOutcome::Skipped(Skip::NoFlake)) => self.not_flakes += 1,
            Ok(AlignOutcome::Skipped(Skip::NoLock)) => {
                self.blind.push((name, "flake.nix with no readable flake.lock".into()));
            }
            Ok(AlignOutcome::Unrewritable(refusal)) => self.blind.push((name, refusal.to_string())),
            Ok(AlignOutcome::Failed(reason)) => self.failed.push((name, reason)),
            Err(e) => self.failed.push((name, e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(nixpkgs_decl: &str, nixpkgs_original: &str, with_substrate: bool) -> ExtendedLockFile {
        let substrate = if with_substrate { r#", "substrate": "substrate""# } else { "" };
        let json = [
            r#"{ "nodes": {
              "nixpkgs": { "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "t" },
                           "original": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-unstable" } },
              "nixpkgs_2": { "locked": { "type": "github", "owner": "nixos", "repo": "nixpkgs", "rev": "r" },
                             "original": "#,
            nixpkgs_original,
            r#" },
              "substrate": { "inputs": { "nixpkgs": "nixpkgs" },
                             "locked": { "type": "github", "owner": "pleme-io", "repo": "substrate", "rev": "s" } },
              "root": { "inputs": { "nixpkgs": "#,
            nixpkgs_decl,
            substrate,
            r#" } } },
              "root": "root", "version": 7 }"#,
        ]
        .concat();
        ExtendedLockFile::parse(&json).unwrap()
    }

    const BRANCH: &str = r#"{ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "ref": "nixos-25.11" }"#;

    #[test]
    fn every_spelling_nix_parses_as_a_branch_is_movable() {
        // `github:nixos/nixpkgs?ref=nixos-25.11` — blue's spelling, missed by
        // the first version's one-spelling regex.
        let query = r#"{ "type": "github", "owner": "nixos", "repo": "nixpkgs", "ref": "nixos-25.11" }"#;
        let unstable = r#"{ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "ref": "nixpkgs-unstable" }"#;
        let default_branch = r#"{ "type": "github", "owner": "NixOS", "repo": "nixpkgs" }"#;
        for original in [BRANCH, query, unstable, default_branch] {
            assert_eq!(classify(&lock(r#""nixpkgs_2""#, original, true)), Eligibility::Movable, "{original}");
        }
    }

    #[test]
    fn each_reason_not_to_convert_is_named() {
        let pinned = r#"{ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "6b316287bae2ee04c9b93c8c858d930fd07d7338" }"#;
        let fork = r#"{ "type": "github", "owner": "someone", "repo": "nixpkgs", "ref": "main" }"#;
        let tarball = r#"{ "type": "tarball", "url": "https://example.com/n.tar.gz" }"#;
        assert_eq!(classify(&lock(r#""nixpkgs_2""#, pinned, true)), Eligibility::PinnedCommit);
        assert_eq!(classify(&lock(r#""nixpkgs_2""#, BRANCH, false)), Eligibility::NoSubstrateInput);
        assert_eq!(
            classify(&lock(r#"["substrate", "nixpkgs"]"#, BRANCH, true)),
            Eligibility::FollowsSubstrate
        );
        assert_eq!(
            classify(&lock(r#"["blue", "nixpkgs"]"#, BRANCH, true)),
            Eligibility::FollowsOther("blue/nixpkgs".into())
        );
        assert_eq!(
            classify(&lock(r#""nixpkgs_2""#, fork, true)),
            Eligibility::NotGithubNixpkgs("github:someone/nixpkgs".into())
        );
        assert_eq!(
            classify(&lock(r#""nixpkgs_2""#, tarball, true)),
            Eligibility::NotGithubNixpkgs("tarball:https://example.com/n.tar.gz".into())
        );
    }

    #[test]
    fn classification_reads_the_roots_input_not_the_node_named_nixpkgs() {
        // Node `nixpkgs` is substrate's transitive copy on nixos-unstable; the
        // root's own input is `nixpkgs_2`, pinned. Reading by node key would
        // call it movable.
        let pinned = r#"{ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "6b316287bae2ee04c9b93c8c858d930fd07d7338" }"#;
        assert_eq!(classify(&lock(r#""nixpkgs_2""#, pinned, true)), Eligibility::PinnedCommit);
    }

    #[test]
    fn multiline_block_converts_and_breaks_cycle() {
        let t = "  inputs = {\n    nixpkgs.url = \"github:NixOS/nixpkgs/nixos-25.11\";\n    substrate = {\n      url = \"github:pleme-io/substrate\";\n      inputs.nixpkgs.follows = \"nixpkgs\";\n    };\n  };\n";
        let out = rewrite_flake(t).unwrap();
        assert!(out.contains("nixpkgs.follows = \"substrate/nixpkgs\""));
        assert!(!out.contains("inputs.nixpkgs.follows = \"nixpkgs\""));
    }

    #[test]
    fn inline_single_line_block_converts_and_breaks_cycle() {
        // The exact shape that caused the follow-cycle in the first sweep.
        let t = "  inputs = {\n    nixpkgs.url = \"github:NixOS/nixpkgs/nixos-25.11\";\n    substrate = { url = \"github:pleme-io/substrate\"; inputs.nixpkgs.follows = \"nixpkgs\"; };\n  };\n";
        let out = rewrite_flake(t).unwrap();
        assert!(out.contains("nixpkgs.follows = \"substrate/nixpkgs\""));
        assert!(
            !out.contains("inputs.nixpkgs.follows = \"nixpkgs\""),
            "inline substrate follow must be removed (else a cycle):\n{out}"
        );
    }

    #[test]
    fn blues_spelling_is_rewritten_exactly() {
        let t = "  inputs = {\n    nixpkgs.url = \"github:nixos/nixpkgs?ref=nixos-25.11\";\n    crate2nix.url = \"github:nix-community/crate2nix\";\n    substrate = {\n      url = \"github:pleme-io/substrate\";\n      inputs.nixpkgs.follows = \"nixpkgs\";\n    };\n  };\n";
        let want = "  inputs = {\n    nixpkgs.follows = \"substrate/nixpkgs\";\n    crate2nix.url = \"github:nix-community/crate2nix\";\n    substrate = {\n      url = \"github:pleme-io/substrate\";\n    };\n  };\n";
        assert_eq!(rewrite_flake(t).unwrap(), want);
    }

    #[test]
    fn top_level_inputs_keep_their_prefix_and_lose_the_whole_follows_line() {
        let t = "  inputs.nixpkgs.url = \"github:NixOS/nixpkgs\";\n  inputs.substrate.url = \"github:pleme-io/substrate\";\n  inputs.substrate.inputs.nixpkgs.follows = \"nixpkgs\";\n  outputs = x: x;\n";
        let want = "  inputs.nixpkgs.follows = \"substrate/nixpkgs\";\n  inputs.substrate.url = \"github:pleme-io/substrate\";\n  outputs = x: x;\n";
        assert_eq!(rewrite_flake(t).unwrap(), want);
    }

    #[test]
    fn a_shape_the_rewriter_cannot_edit_is_refused_by_name() {
        let block = "  inputs = {\n    nixpkgs = {\n      url = \"github:NixOS/nixpkgs/nixos-25.11\";\n    };\n  };\n";
        assert_eq!(rewrite_flake(block), Err(RewriteRefusal::NoDeclarationLine));
        let inline = "  inputs = { nixpkgs.url = \"github:NixOS/nixpkgs\"; substrate.url = \"x\"; };\n";
        assert_eq!(rewrite_flake(inline), Err(RewriteRefusal::NoDeclarationLine));
        let two = "    nixpkgs.url = \"a\";\n    nixpkgs.url = \"b\";\n";
        assert_eq!(rewrite_flake(two), Err(RewriteRefusal::SeveralDeclarationLines(2)));
    }

    #[test]
    fn a_blind_spot_is_reported_by_name_and_never_counted_as_a_skip() {
        let mut report = AlignReport::default();
        report.record("a", Ok(AlignOutcome::Unrewritable(RewriteRefusal::NoDeclarationLine)));
        report.record("b", Ok(AlignOutcome::Skipped(Skip::NoLock)));
        report.record("c", Ok(AlignOutcome::Skipped(Skip::Ineligible(Eligibility::PinnedCommit))));
        report.record("d", Ok(AlignOutcome::Skipped(Skip::Ineligible(Eligibility::FollowsSubstrate))));
        assert_eq!(
            report.blind.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!((report.by_design, report.converged), (1, 1));
    }
}
