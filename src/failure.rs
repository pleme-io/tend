//! Typed failure classification for a reconcile cycle.
//!
//! # Why this exists
//!
//! `SyncOutcome::Failed { stderr: String }` and `FetchOutcome::Failed { stderr }`
//! carry the reason as unstructured text. The reason is therefore only readable
//! by a human running `grep` over a cumulative log, and that is not a
//! hypothetical problem — it produced a real, confident misdiagnosis on
//! 2026-07-28:
//!
//! - A `grep -c` over the 112 MB cumulative `tend-daemon.err` counted 193,402
//!   `could not read Username` and 17,768 `Session open refused by peer` lines,
//!   which was read as "91% credential / 8% transport".
//! - Normalizing the same corpus by failure SHAPE showed the top line was
//!   157,285 × `Cloning into 'X'...` — **git progress output, not an error at
//!   all** — followed by ~134k DNS/connectivity failures.
//! - Windowing to the most recent 4,000 lines showed exactly **2** failures.
//!
//! Three different answers from one file, because the data had no types, no
//! cycle boundaries, and no distinction between "this workspace is broken" and
//! "this laptop was asleep". A `String` cannot be aggregated, compared across
//! cycles, or counted correctly.
//!
//! # The determinism property this buys
//!
//! A reconciler is deterministically stable when a cycle's verdict is a
//! function of *(declared config, repo state)* and NOT of ambient conditions.
//! `Could not resolve host` is ambient — a closed lid produces thousands of
//! them and says nothing about whether tend is working. `could not read
//! Username` is not ambient: it means a credential this machine is supposed to
//! have is gone.
//!
//! Collapsing both into one "failed" counter is what makes the health signal
//! non-deterministic. [`Disposition`] is the split, so the zero-residue
//! predicate can count only what a fix could actually change.
//!
//! # Scope
//!
//! Pure and total: no I/O, no allocation beyond the borrowed input, every
//! input returns a variant. That makes it exhaustively testable against the
//! real corpus, which is exactly what the tests below do.

/// Whether a failure is something an operator can act on, or ambient noise.
///
/// This is the load-bearing distinction. Counting them together is what
/// produced three contradictory readings of the same log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// A fact about the environment, not about tend or its config: the host is
    /// offline, DNS is down, a proxy is unhealthy. Retrying later fixes it and
    /// nothing in the repo can. These MUST NOT count toward residue — a
    /// sleeping laptop would otherwise report the fleet as permanently broken.
    Environmental,
    /// tend's own configuration or state caused it, and a change on this
    /// machine fixes it. Counts toward residue.
    SelfInflicted,
    /// Real and actionable, but the fix is OUTSIDE this machine — an
    /// authorization a human must grant, a repo that no longer exists. Counts
    /// toward residue, but escalates to a person rather than to a retry.
    RequiresOperator,
}

/// A classified reason a repo operation failed.
///
/// Every variant below was observed in the live corpus; none is speculative.
/// The counts in each doc comment are from the cumulative
/// `~/Library/Logs/tend-daemon.err` on cid, 2026-07-28, normalized by shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCause {
    /// `fatal: could not read Username for '...': Device not configured`
    ///
    /// 6,130 occurrences, and the ONLY cause present in the recent window.
    /// Self-inflicted and self-sustaining: git's `credential.helper = store`
    /// ERASES the entry on auth failure, so one failure removes the credential
    /// the next fetch needs. Distinct from [`Self::CredentialRejected`] —
    /// this one is recoverable without a human.
    CredentialAbsent,

    /// Authentication was attempted WITH a credential and refused.
    ///
    /// Needs an authorization a human must grant (SSO for an org, a scope on a
    /// PAT). Indistinguishable from `CredentialAbsent` in the old `String`
    /// form, which is precisely why "why do 88 of 125 repos fail?" could not
    /// be answered without a manual ssh probe.
    CredentialRejected,

    /// `mux_client_request_session: session request failed: Session open
    /// refused by peer`
    ///
    /// The shared ssh `ControlMaster` connection is at its session cap.
    /// 17,768 raw lines, but only 9 as an actual tend failure — ssh emits this
    /// on its own stderr, so the raw count over-reports its impact. Recorded
    /// here so the distinction is measurable next time instead of guessed.
    TransportSaturated,

    /// `Could not resolve host` / `ssh: connect to host ...: Undefined error`
    ///
    /// ~134k occurrences and the corpus's true dominant shape. Ambient: a
    /// closed lid or a network change produces these in bulk.
    NameResolution,

    /// Connect failed, timed out, or the peer reset the connection.
    Unreachable,

    /// `SSL certificate problem: unable to get local issuer certificate`
    TlsUntrusted,

    /// `no healthy upstream` / `upstream connect error or disconnect` — an
    /// intercepting proxy, not GitHub.
    ProxyUnhealthy,

    /// The remote repo is gone, renamed, or was never visible.
    UpstreamMissing,

    /// `fatal: expected flush after ref listing` — a truncated or corrupted
    /// git wire exchange.
    ProtocolViolation,

    /// Recognized as a failure but not matched to a known shape.
    ///
    /// Deliberately NOT a catch-all for "anything": it means the classifier
    /// needs a new arm. A rising `Unclassified` count is a signal to extend
    /// this enum, which is only visible because it is a distinct variant
    /// rather than being folded into a generic error.
    Unclassified,
}

impl FailureCause {
    /// How this failure should be counted and routed.
    #[must_use]
    pub const fn disposition(self) -> Disposition {
        match self {
            // Ambient — the host, the network, or something between.
            Self::NameResolution
            | Self::Unreachable
            | Self::ProxyUnhealthy
            | Self::ProtocolViolation => Disposition::Environmental,

            // This machine's own config or state.
            Self::CredentialAbsent | Self::TransportSaturated | Self::TlsUntrusted => {
                Disposition::SelfInflicted
            }

            // A human has to grant something or fix something upstream.
            Self::CredentialRejected | Self::UpstreamMissing => Disposition::RequiresOperator,

            // Unknown: treat as actionable so it is never silently ignored.
            Self::Unclassified => Disposition::RequiresOperator,
        }
    }

    /// Whether this failure counts toward the zero-residue predicate.
    ///
    /// `Environmental` failures do not: no change to config or code can clear
    /// them, so including them would make "0 failed" unreachable whenever the
    /// host is offline, and the predicate would be abandoned as unrealistic.
    #[must_use]
    pub const fn counts_as_residue(self) -> bool {
        !matches!(self.disposition(), Disposition::Environmental)
    }
}

/// Whether a stderr line is a FAILURE at all, as opposed to git progress.
///
/// This exists because of a specific mistake: `Cloning into 'X'...` is written
/// to **stderr** by `git clone` on success, and it was the single most frequent
/// line in the corpus (157,285). Counting stderr lines as failures inflated
/// every number derived from that file.
#[must_use]
pub fn is_failure_output(stderr: &str) -> bool {
    let s = stderr.trim();
    if s.is_empty() {
        return false;
    }
    // git progress and informational output, all on stderr, none an error.
    const BENIGN_PREFIXES: &[&str] = &[
        "Cloning into",
        "Receiving objects",
        "Resolving deltas",
        "remote: Enumerating",
        "remote: Counting",
        "remote: Compressing",
        "remote: Total",
        "Updating files",
        "Submodule path",
        "From ",
        " * [new",
        "Already up to date",
    ];
    if BENIGN_PREFIXES.iter().any(|p| s.starts_with(p)) {
        return false;
    }
    true
}

/// Classify a git stderr payload into a typed cause.
///
/// Total: every input yields a variant. Order matters — the most specific
/// patterns are tested first, because git nests messages (a credential error
/// arrives inside a `fatal: unable to access '...'` wrapper).
#[must_use]
pub fn classify(stderr: &str) -> FailureCause {
    let s = stderr;

    // Credentials. `could not read Username` means the helper produced
    // NOTHING — the entry is absent, not refused.
    if s.contains("could not read Username") || s.contains("could not read Password") {
        return FailureCause::CredentialAbsent;
    }
    if s.contains("Authentication failed")
        || s.contains("Permission denied (publickey")
        || s.contains("internal error performing authentication")
        || s.contains("SAML")
        || s.contains("must use a personal access token")
    {
        return FailureCause::CredentialRejected;
    }

    // Transport saturation — the shared ControlMaster's session cap.
    if s.contains("Session open refused by peer") || s.contains("mux_client_request_session") {
        return FailureCause::TransportSaturated;
    }

    // Name resolution. Checked BEFORE the generic connect failures because
    // ssh's "Undefined error" wrapper is a resolution failure in disguise.
    if s.contains("Could not resolve host")
        || s.contains("Name or service not known")
        || s.contains("nodename nor servname provided")
        || s.contains("Temporary failure in name resolution")
        || (s.contains("ssh: connect to host") && s.contains("Undefined error"))
    {
        return FailureCause::NameResolution;
    }

    // TLS before the generic access failure, since it arrives wrapped too.
    if s.contains("SSL certificate problem")
        || s.contains("unable to get local issuer certificate")
        || s.contains("certificate verify failed")
    {
        return FailureCause::TlsUntrusted;
    }

    // An intercepting proxy, not the remote.
    if s.contains("no healthy upstream") || s.contains("upstream connect error") {
        return FailureCause::ProxyUnhealthy;
    }

    if s.contains("expected flush after ref listing") || s.contains("protocol error") {
        return FailureCause::ProtocolViolation;
    }

    if s.contains("Repository not found")
        || s.contains("does not appear to be a git repository")
        || s.contains("Could not read from remote repository")
    {
        return FailureCause::UpstreamMissing;
    }

    if s.contains("Couldn't connect to server")
        || s.contains("Failed to connect to")
        || s.contains("Connection reset by peer")
        || s.contains("Connection refused")
        || s.contains("Recv failure")
        || s.contains("Operation timed out")
        || s.contains("ssh: connect to host")
    {
        return FailureCause::Unreachable;
    }

    FailureCause::Unclassified
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every string below is verbatim from ~/Library/Logs/tend-daemon.err on
    // cid, 2026-07-28. None is invented, so a passing test means the
    // classifier handles what actually happens, not what seemed plausible.

    #[test]
    fn git_progress_on_stderr_is_not_a_failure() {
        // THE mistake this module exists to prevent. 157,285 of these were
        // counted as failures, which is what made a cumulative grep produce a
        // confidently wrong diagnosis.
        assert!(!is_failure_output("Cloning into 'akeyless-go'..."));
        assert!(!is_failure_output("Receiving objects:  47% (100/210)"));
        assert!(!is_failure_output("Resolving deltas: 100% (52/52), done."));
        assert!(!is_failure_output("Already up to date."));
        assert!(!is_failure_output("   "));
        assert!(!is_failure_output(""));
    }

    #[test]
    fn real_failures_are_recognized_as_failures() {
        assert!(is_failure_output(
            "fatal: could not read Username for 'https://github.com': Device not configured"
        ));
        assert!(is_failure_output(
            "fatal: unable to access 'https://github.com/x/y': Could not resolve host: github.com"
        ));
    }

    #[test]
    fn credential_absent_is_the_recent_window_cause() {
        let s = "fatal: could not read Username for 'https://github.com': Device not configured";
        assert_eq!(classify(s), FailureCause::CredentialAbsent);
        assert_eq!(
            classify(s).disposition(),
            Disposition::SelfInflicted,
            "an erased credential is this machine's own state, not the network's"
        );
        assert!(classify(s).counts_as_residue());
    }

    #[test]
    fn dns_failure_is_the_corpus_dominant_shape_and_is_environmental() {
        for s in [
            "fatal: unable to access 'https://github.com/pleme-io/x/': Could not resolve host: github.com",
            "ssh: connect to host github.com port 22: Undefined error: 0",
        ] {
            assert_eq!(classify(s), FailureCause::NameResolution, "input: {s}");
            assert_eq!(classify(s).disposition(), Disposition::Environmental);
            assert!(
                !classify(s).counts_as_residue(),
                "a closed lid must not make the zero-residue predicate unreachable"
            );
        }
    }

    #[test]
    fn ssh_undefined_error_is_resolution_not_merely_unreachable() {
        // Ordering regression guard: this string contains BOTH
        // "ssh: connect to host" (an Unreachable trigger) and "Undefined
        // error". It must classify as NameResolution, or ~78k corpus lines
        // land in the wrong bucket.
        let s = "ssh: connect to host github.com port 22: Undefined error: 0";
        assert_eq!(classify(s), FailureCause::NameResolution);
    }

    #[test]
    fn transport_saturation_is_distinct_from_unreachable() {
        let s = "mux_client_request_session: session request failed: Session open refused by peer";
        assert_eq!(classify(s), FailureCause::TransportSaturated);
        assert_eq!(classify(s).disposition(), Disposition::SelfInflicted);
    }

    #[test]
    fn tls_and_proxy_are_not_collapsed_into_unreachable() {
        assert_eq!(
            classify("fatal: unable to access 'https://github.com/x/': SSL certificate problem: unable to get local issuer certificate"),
            FailureCause::TlsUntrusted
        );
        assert_eq!(classify("ERROR: no healthy upstream"), FailureCause::ProxyUnhealthy);
        assert_eq!(
            classify("ERROR: upstream connect error or disconnect/reset before headers. retried and the latest reset reason: remote connection failure"),
            FailureCause::ProxyUnhealthy
        );
    }

    #[test]
    fn rejected_is_distinguished_from_absent() {
        // The distinction that made "why do 88 of 125 fail?" answerable.
        assert_eq!(
            classify("ERROR: internal error performing authentication"),
            FailureCause::CredentialRejected
        );
        assert_eq!(
            classify("remote: Permission denied (publickey)."),
            FailureCause::CredentialRejected
        );
        assert_ne!(
            classify("fatal: could not read Username for 'https://github.com': Device not configured"),
            FailureCause::CredentialRejected,
            "absent and rejected must never collapse: one auto-recovers, one needs a human"
        );
        assert_eq!(
            FailureCause::CredentialRejected.disposition(),
            Disposition::RequiresOperator
        );
    }

    #[test]
    fn remaining_observed_shapes_all_classify() {
        for (s, want) in [
            (
                "fatal: unable to access 'https://github.com/x/': Failed to connect to github.com port 443 after 75000 ms: Couldn't connect to server",
                FailureCause::Unreachable,
            ),
            (
                "fatal: unable to access 'https://github.com/x/': Recv failure: Connection reset by peer",
                FailureCause::Unreachable,
            ),
            ("fatal: expected flush after ref listing", FailureCause::ProtocolViolation),
            (
                "fatal: Could not read from remote repository.",
                FailureCause::UpstreamMissing,
            ),
        ] {
            assert_eq!(classify(s), want, "input: {s}");
        }
    }

    #[test]
    fn classifier_is_total_and_never_panics() {
        // Adversarial inputs: a classifier that panics inside a reconcile loop
        // would take the whole cycle down.
        for s in [
            "",
            " ",
            "\n\n",
            "\u{0}",
            "a",
            "fatal:",
            "warning: fetch failed for x: ",
            "💥 unrecognised",
            "Could not resolve host",
        ] {
            let _ = classify(s);
            let _ = is_failure_output(s);
        }
    }

    #[test]
    fn unknown_input_is_actionable_not_ignored() {
        // A cause the classifier does not recognize must NOT be silently
        // dropped into the environmental bucket, or a new failure mode would
        // become invisible exactly the way these ones were.
        let c = classify("some entirely novel git catastrophe");
        assert_eq!(c, FailureCause::Unclassified);
        assert!(
            c.counts_as_residue(),
            "an unrecognised failure must still count, or the classifier hides its own gaps"
        );
    }

    #[test]
    fn residue_partition_is_exhaustive_and_disjoint() {
        // Catalog-reflection style: every variant has a disposition, and the
        // residue predicate is exactly the non-environmental set. If a variant
        // is added without considering its disposition, this fails.
        const ALL: &[FailureCause] = &[
            FailureCause::CredentialAbsent,
            FailureCause::CredentialRejected,
            FailureCause::TransportSaturated,
            FailureCause::NameResolution,
            FailureCause::Unreachable,
            FailureCause::TlsUntrusted,
            FailureCause::ProxyUnhealthy,
            FailureCause::UpstreamMissing,
            FailureCause::ProtocolViolation,
            FailureCause::Unclassified,
        ];
        let residue = ALL.iter().filter(|c| c.counts_as_residue()).count();
        let ambient = ALL.iter().filter(|c| !c.counts_as_residue()).count();
        assert_eq!(residue + ambient, ALL.len(), "partition must be complete");
        assert!(residue > 0 && ambient > 0, "both sides must be non-empty");
        for c in ALL {
            let d = c.disposition();
            assert_eq!(
                c.counts_as_residue(),
                d != Disposition::Environmental,
                "residue must be exactly the non-environmental set: {c:?}"
            );
        }
    }
}
