//! Differential gating support — extract failure signatures from gate
//! output and compare them as sets.
//!
//! The fleet-update use case: a verification gate (`nix flake check`,
//! `nix build`, `cargo test`, …) is rarely on a clean baseline. Tests
//! flap, modules in flight have eval errors, downstream repos publish
//! configs that don't pass yet. If the gate is "absolute" — fail on
//! any error — the operator can't ever land a bump on a non-pristine
//! repo, even one that doesn't touch the failing code.
//!
//! The right semantics: a gate passes for a proposal iff
//!
//!     failures(proposed pin) ⊆ failures(baseline)
//!
//! Pre-existing failures are out-of-scope. Only failures *introduced*
//! by the proposed pin block the bump. This module provides the
//! `FailureSet` primitive that makes that comparison cheap and
//! domain-agnostic — every controller (Helm Phase 2, Cargo Phase 3,
//! Image Phase 4) can extract its own failure signatures and reuse
//! the same subset semantics.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureSet {
    /// Normalized, deduplicated failure signatures. Each entry is one
    /// "thing that went wrong" — a named test, a single `error:` line,
    /// a build target. Sortable for stable diff output.
    inner: BTreeSet<String>,
}

impl FailureSet {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when every signature in `self` also appears in `baseline`.
    /// "self ⊆ baseline" — the load-bearing predicate for differential
    /// gating. When this is true, the proposed change introduced no
    /// *new* failures and the gate passes.
    #[must_use]
    pub fn is_subset_of(&self, baseline: &Self) -> bool {
        self.inner.is_subset(&baseline.inner)
    }

    /// Signatures present in `self` but absent from `baseline` — the
    /// "new failures" surfaced for operator-readable diagnostic output.
    #[must_use]
    pub fn new_vs(&self, baseline: &Self) -> Vec<String> {
        self.inner.difference(&baseline.inner).cloned().collect()
    }

    /// Extract failure signatures from a captured gate log. Combines
    /// two passes:
    ///   - Compound "NixOS eval tests failed (NN/MM passed): a; b; c"
    ///     splits each named-test entry into its own signature.
    ///   - Bare `error:` lines (one per failure target).
    ///
    /// Both passes normalize ANSI escapes and store-path-style hashes
    /// so the same failure produces the same signature across runs.
    #[must_use]
    pub fn extract(log: &str) -> Self {
        let mut inner = BTreeSet::new();
        let cleaned = strip_ansi(log);
        for failure in extract_eval_test_failures(&cleaned) {
            inner.insert(normalize(&failure));
        }
        // Single-error lines (`error: foo`) are signatures themselves —
        // but skip:
        //   - the compound-summary `error: <thing> tests failed
        //     (N/M passed): a: x; b: y` (volatile count, individual
        //     test names already captured above)
        //   - eval-only artifacts like `error: path '/nix/store/...'
        //     is not valid` (means "would need to build the
        //     derivation"; not a real flake bug — `nix run .#rebuild`
        //     would substitute or build it). These churn whenever a
        //     `--override-input` changes nixpkgs-derived paths.
        for line in cleaned.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("error:")
                && !is_compound_failure_summary(trimmed)
                && !is_eval_only_artifact(trimmed)
            {
                inner.insert(normalize(trimmed));
            }
        }
        Self { inner }
    }
}

/// Strip ANSI CSI escape sequences (`\x1b[...m`) — nix colors errors
/// and the codes appear inside otherwise-stable strings.
fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // skip until terminator (alpha char in 0x40..=0x7e)
            i += 2;
            while i < bytes.len() && !(bytes[i] >= 0x40 && bytes[i] <= 0x7e) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Stable normalization for a single failure line: strip nix store
/// path hashes (the `xxxxxxxx-` prefix in `/nix/store/abcdefgh-name`),
/// trim whitespace, collapse repeated whitespace.
fn normalize(s: &str) -> String {
    let trimmed = s.trim();
    // Replace each `/nix/store/<32 hash>-` with `/nix/store/<hash>-`
    // so different builds with the same logical content match.
    let mut out = String::with_capacity(trimmed.len());
    let bytes = trimmed.as_bytes();
    let needle = b"/nix/store/";
    let mut i = 0;
    while i < bytes.len() {
        if i + needle.len() < bytes.len() && &bytes[i..i + needle.len()] == needle {
            out.push_str("/nix/store/<hash>");
            i += needle.len();
            // skip 32 hex/base32 chars (nix uses base32) up to the dash
            while i < bytes.len() && bytes[i] != b'-' && bytes[i] != b'/' {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// True for the volatile "tests failed: a; b; c" summary line. The
/// individual test entries are stable signatures, but the wrapper has
/// a count ("(80/84 passed)") that changes per run.
fn is_compound_failure_summary(line: &str) -> bool {
    line.contains("tests failed")
}

/// True for eval-time artifacts like `error: path '/nix/store/...'
/// is not valid`. Surfaced by `nix flake check --no-build` when a
/// derivation reference exists but the corresponding store path
/// hasn't been realized; the user's actual `nix run .#rebuild` would
/// substitute or build it. Including these in the failure set causes
/// false-positive differential rejections on any pin bump that
/// changes a transitive nixpkgs-derived path (every gemdir, every
/// `vendorHash`-driven derivation, etc.).
fn is_eval_only_artifact(line: &str) -> bool {
    line.contains("is not valid") && line.contains("/nix/store/")
}

/// Pull individual test names out of a "NixOS eval tests failed
/// (NN/MM passed): a: msg1; b: msg2; c: msg3" line.
fn extract_eval_test_failures(log: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needles = ["eval tests failed", "tests failed"];
    for line in log.lines() {
        for needle in &needles {
            if let Some(idx) = line.find(needle) {
                if let Some(colon_after) = line[idx..].find(':') {
                    let body = &line[idx + colon_after + 1..];
                    for part in body.split(';') {
                        let s = part.trim();
                        if !s.is_empty() && s.contains(':') {
                            out.push(s.to_string());
                        }
                    }
                    break;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_nix_eval_test_failures_splits_compound_line() {
        let log = "error: NixOS eval tests failed (80/84 passed): orion-variant-server: orion blizzard variant should be server; cid-anvil-jules: cid anvil should have Jules MCP server\n";
        let fs = FailureSet::extract(log);
        // The compound split + the leading `error:` line both count.
        assert!(fs.len() >= 2);
        let body = format!("{:?}", fs.inner);
        assert!(body.contains("orion-variant-server"));
        assert!(body.contains("cid-anvil-jules"));
    }

    #[test]
    fn subset_check_passes_when_no_new_failures() {
        let baseline = FailureSet::extract("error: NixOS eval tests failed: a: x; b: y; c: z");
        let proposed = FailureSet::extract("error: NixOS eval tests failed: a: x; b: y");
        // proposed has fewer failures than baseline → strict subset → pass
        assert!(proposed.is_subset_of(&baseline));
        assert_eq!(proposed.new_vs(&baseline), Vec::<String>::new());
    }

    #[test]
    fn subset_check_fails_when_new_failure_introduced() {
        let baseline = FailureSet::extract("error: NixOS eval tests failed: a: x; b: y");
        let proposed = FailureSet::extract("error: NixOS eval tests failed: a: x; b: y; c: NEW");
        assert!(!proposed.is_subset_of(&baseline));
        let new = proposed.new_vs(&baseline);
        assert!(new.iter().any(|s| s.contains("c: NEW")), "got: {new:?}");
    }

    #[test]
    fn ansi_codes_dont_break_signature_matching() {
        let baseline = FailureSet::extract("error: \x1b[31;1minfinite recursion\x1b[0m");
        let proposed = FailureSet::extract("error: infinite recursion");
        assert!(
            proposed.is_subset_of(&baseline),
            "ANSI normalization should let plain match colored: baseline={:?} proposed={:?}",
            baseline.inner,
            proposed.inner
        );
    }

    #[test]
    fn store_path_hashes_normalize() {
        let baseline =
            FailureSet::extract("error: cannot import /nix/store/abc123def456ghi789-foo-1.0/bin/x");
        let proposed =
            FailureSet::extract("error: cannot import /nix/store/zzzyyyxxxwww111222-foo-1.0/bin/x");
        assert!(
            proposed.is_subset_of(&baseline),
            "store hashes should normalize: baseline={:?} proposed={:?}",
            baseline.inner,
            proposed.inner
        );
    }

    #[test]
    fn empty_proposed_against_failing_baseline_passes() {
        // Proposed pin somehow fixes the failing tests. Subset of any
        // non-empty set, so the gate passes.
        let baseline = FailureSet::extract("error: a: x; b: y");
        let proposed = FailureSet::default();
        assert!(proposed.is_subset_of(&baseline));
    }

    #[test]
    fn empty_proposed_passes_against_empty_baseline() {
        // Both clean — gate trivially passes.
        let baseline = FailureSet::default();
        let proposed = FailureSet::default();
        assert!(proposed.is_subset_of(&baseline));
    }

    #[test]
    fn eval_only_path_not_valid_is_filtered_out() {
        // `nix flake check --no-build` surfaces "path is not valid"
        // when a derivation reference's store path isn't realized.
        // This isn't a flake bug — the user's real rebuild builds or
        // substitutes the derivation. Keeping it as a signature would
        // cause false-positive differential rejections on every pin
        // bump that changes a transitive nixpkgs-derived path.
        let log = "error: path '/nix/store/dy3bnagxvay5gmbhg7dfwk0g54mcwi5g-pangea-compiler-gemdir.drv' is not valid";
        let fs = FailureSet::extract(log);
        assert!(
            fs.is_empty(),
            "eval-only `path is not valid` should be filtered, got: {:?}",
            fs.inner
        );
    }

    #[test]
    fn real_eval_errors_still_captured_around_eval_only_filter() {
        // Mix the eval-only artifact with a real eval error; only the
        // real one should remain in the failure set.
        let log = "\
error: path '/nix/store/abc-foo.drv' is not valid
error: infinite recursion encountered while evaluating module foo
";
        let fs = FailureSet::extract(log);
        let body = format!("{:?}", fs.inner);
        assert!(
            body.contains("infinite recursion"),
            "real error should be captured: {body}"
        );
        assert!(
            !body.contains("is not valid"),
            "eval-only artifact should be filtered: {body}"
        );
    }
}
