//! Anomaly classifier for tend prebuild failures.
//!
//! Sibling-in-spirit of seibi's `reconverge` recipes — same shape
//! (typed failure-class, single-function-add per class), but the
//! intent is classification first, repair later. A prebuild cycle
//! generates dozens of `prebuild_failed` audit lines with freeform
//! `nix build` stderr; classifying each one into a closed-enum
//! `FailureClass` turns "176 grep-only failures" into
//! "31 maven-ci-friendly-version, 8 missing-cargo-nix, 4 hash-
//! mismatch, 133 unclassified" — typed dashboards + per-class
//! recovery hooks instead of one huge text bucket.
//!
//! Adding a new class:
//! 1. Add a variant to [`FailureClass`] (+ doc comment with the
//!    source signature you matched on).
//! 2. Add a match arm to [`classify`] that recognises the stderr
//!    shape.
//! 3. Add a unit test below that pins the recognition.
//! Five-minute add per class — same compounding shape as
//! `seibi::reconverge::recipes()`.

use serde::Serialize;

/// Closed enum of the failure classes the prebuild loop has seen
/// from the live pleme-io workload on rio. `Unknown` is the
/// catch-all; growing the enum to shrink the `Unknown` bucket is
/// the work that compounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    /// `pom.xml` declares `<version>${revision}</version>` and
    /// nixpkgs' Maven builder runs without the
    /// `flatten-maven-plugin` that resolves CI-friendly versions
    /// — Maven refuses to read the project. Recognised via
    /// `"'version' must be a constant version but is '${revision}'"`
    /// in stderr. Fix shape: patch pom.xml to inline the
    /// `<revision>` property value, OR add the flatten-plugin
    /// hook to the flake's Maven derivation.
    MavenCiFriendlyVersion,
    /// The flake imports a `./Cargo.nix` that isn't checked in
    /// (substrate's `rust-tool-release-flake.nix:211` blows up
    /// on `import cargoNix` when the path doesn't exist). Fix
    /// shape: run `crate2nix generate` in the repo, commit
    /// `Cargo.nix`. The user's pleme-io contract says generated
    /// `Cargo.nix` is checked in — this is a maintenance miss.
    MissingCargoNix,
    /// `nix build` saw `error: hash mismatch in fixed-output
    /// derivation` — the FOD's `outputHash` no longer matches
    /// what fetcher (cargo / Maven / npm) actually pulled.
    /// Usually means the upstream changed, the lockfile drifted,
    /// or a new dep landed. Fix shape: copy the `got:` hash into
    /// the `specified:` slot in the Nix expression.
    FixedOutputHashMismatch,
    /// Flake doesn't expose `packages.<system>.default` (or
    /// requested attribute). Distinct from a real failure — the
    /// repo simply isn't buildable via the canonical entry-point.
    /// We also map this through [`crate::prebuild::BuildOutcome::NoDefault`],
    /// so it normally short-circuits before classification; this
    /// arm is here in case a wrapping flake re-surfaces the same
    /// error inside a different code path.
    NoDefaultAttribute,
    /// `nix build` failed because the daemon (or one of its
    /// substituters) refused to fetch a path. Often network-flaky;
    /// often resolves on retry without operator action.
    SubstituteFetchFailed,
    /// Build crashed inside a builder script (compile error,
    /// panicked process, etc.) — the actual workload bug. No
    /// generic fix; surface the offending derivation in the
    /// audit log so the operator can `nix log <drv>`.
    BuilderFailed,
    /// Nothing in the classifier matched — adding a new variant
    /// is a five-minute follow-up when this class becomes
    /// recurring enough to justify it.
    Unknown,
}

impl FailureClass {
    /// Short stable identifier for audit-log + metric labels.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MavenCiFriendlyVersion => "maven-ci-friendly-version",
            Self::MissingCargoNix => "missing-cargo-nix",
            Self::FixedOutputHashMismatch => "fixed-output-hash-mismatch",
            Self::NoDefaultAttribute => "no-default-attribute",
            Self::SubstituteFetchFailed => "substitute-fetch-failed",
            Self::BuilderFailed => "builder-failed",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify a `nix build` stderr blob into a typed [`FailureClass`].
/// Order of checks matters — the most specific signatures win;
/// `Unknown` is the catch-all when nothing matched.
#[must_use]
pub fn classify(stderr: &str) -> FailureClass {
    if stderr.contains("'version' must be a constant version but is '${revision}'")
        || stderr.contains("'version' must be a valid version but is '${revision}'")
    {
        return FailureClass::MavenCiFriendlyVersion;
    }
    if (stderr.contains("Cargo.nix")
        && (stderr.contains("does not exist") || stderr.contains("No such file")))
        || stderr.contains("path '") && stderr.contains("Cargo.nix") && stderr.contains("does not exist")
    {
        return FailureClass::MissingCargoNix;
    }
    if stderr.contains("hash mismatch in fixed-output derivation") {
        return FailureClass::FixedOutputHashMismatch;
    }
    if stderr.contains("does not provide attribute")
        || stderr.contains("attribute 'default' missing")
    {
        return FailureClass::NoDefaultAttribute;
    }
    if stderr.contains("substituter rejected")
        || stderr.contains("error fetching") && stderr.contains("substituter")
        || stderr.contains("unable to download")
    {
        return FailureClass::SubstituteFetchFailed;
    }
    if stderr.contains("builder for ")
        && (stderr.contains("failed with exit code")
            || stderr.contains("failed with exit status"))
    {
        return FailureClass::BuilderFailed;
    }
    FailureClass::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_maven_revision_class() {
        // Pulled verbatim from the audit log on rio
        // (akeyless-java-cloud-id, akeyless-java-cloud-id-lightweight,
        // akeyless-servicenow-credential-resolver).
        let stderr = "[ERROR] 'version' must be a constant version but is '${revision}'. @ line 8, column 14";
        assert_eq!(classify(stderr), FailureClass::MavenCiFriendlyVersion);
    }

    #[test]
    fn classifies_missing_cargo_nix() {
        // akeyless-mcp on rio.
        let stderr = "error: path '/nix/store/wj3016dik94as494ys1z5k5a45rjw5q7-source/Cargo.nix' does not exist";
        assert_eq!(classify(stderr), FailureClass::MissingCargoNix);
    }

    #[test]
    fn classifies_fixed_output_hash_mismatch() {
        // JenkinsSecretsManagerProvider on rio.
        let stderr = "error: hash mismatch in fixed-output derivation '/nix/store/x.drv':\n         specified: sha256-AAA…\n            got:    sha256-4J8…";
        assert_eq!(classify(stderr), FailureClass::FixedOutputHashMismatch);
    }

    #[test]
    fn classifies_no_default_attribute() {
        let stderr = "error: flake does not provide attribute 'packages.x86_64-linux.default'";
        assert_eq!(classify(stderr), FailureClass::NoDefaultAttribute);
    }

    #[test]
    fn classifies_substitute_fetch_failed() {
        let stderr = "warning: unable to download 'https://cache.example.com/x.narinfo': Couldn't connect";
        assert_eq!(classify(stderr), FailureClass::SubstituteFetchFailed);
    }

    #[test]
    fn classifies_generic_builder_failure() {
        let stderr = "builder for '/nix/store/x.drv' failed with exit code 1";
        assert_eq!(classify(stderr), FailureClass::BuilderFailed);
    }

    #[test]
    fn unknown_for_unrecognised_text() {
        let stderr = "something nobody has seen before in the universe";
        assert_eq!(classify(stderr), FailureClass::Unknown);
    }

    #[test]
    fn class_as_str_round_trips() {
        // Stable identifier — flips would break dashboards that
        // bucket on this string.
        assert_eq!(FailureClass::MavenCiFriendlyVersion.as_str(),
                   "maven-ci-friendly-version");
        assert_eq!(FailureClass::Unknown.as_str(), "unknown");
    }

    #[test]
    fn maven_specific_check_doesnt_match_generic_revision_text() {
        // Avoid false positives on the literal word "revision"
        // appearing in unrelated builder stderr.
        let stderr = "error: revision pinning failed due to unrelated reason";
        assert_ne!(classify(stderr), FailureClass::MavenCiFriendlyVersion);
    }
}
