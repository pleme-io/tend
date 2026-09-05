//! Discovery reachability — what happened when tend asked a forge about
//! an org, expressed so that "we could not ask" can never be mistaken for
//! "there is nothing there".
//!
//! ── ★ THE BUG THIS EXISTS TO CLOSE ──────────────────────────────────────
//! `sync::resolve_repos` ended in a bare `?`, and every CLI call site
//! (`main.rs` status/sync/pull/…) inherited it. One org that the fleet
//! credential cannot read therefore aborted the whole run: on 2026-09-05 a
//! `403` on `akeylesslabs` — an org the pleme-io fine-grained PAT
//! structurally *cannot* list, by design, not by accident — took down
//! discovery for the four orgs that were perfectly reachable.
//!
//! The naive fix is to swallow the error and carry on with a short list.
//! **That fix is worse than the bug**, and the reason is written down in
//! izumi: `izumi-sources/src/tend_repos.rs` states an honesty contract —
//! *"a failed/absent `tend` run is `Unavailable(Error)` … so a tooling blip
//! never reads as 'every repo clean'"*. Today tend's abort is what upholds
//! that. Exit 0 with a silently-short array would flip a downstream board
//! from honestly-unavailable to **confidently wrong**, which is the one
//! direction this fleet's docs call unrecoverable: a degraded answer that
//! reads as a modest-but-valid one is never re-examined.
//!
//! So blindness is not swallowed and not thrown. It is **represented**.
//!
//! ── ★ FOUR ARMS, BORROWED FROM `kotae` ──────────────────────────────────
//! The fleet already owns this vocabulary: `kotae::Answer` —
//! `found · empty · refused · blind`, "no two rendering the same bytes",
//! and `empty` is a finding rather than an error. This module is the
//! **in-process, typed** sibling of that boundary type, for three reasons:
//!
//!   1. `kotae::Answer` is not generic — its payload is `serde_json::Value`.
//!      It is a *boundary* type for JSON/MCP surfaces. Discovery returns
//!      `Vec<String>` deep inside the call graph, long before a boundary.
//!   2. `tend status --json` emits a **flat array** of `StatusJsonRow`, a
//!      contract izumi parses. Wrapping that array in a kotae envelope
//!      would break a live consumer for no gain.
//!   3. There is fleet precedent for exactly this: `mukae-spec/src/env.rs`
//!      defines a local generic clone of the same four arms rather than
//!      taking the dependency. We follow it.
//!
//! tend's genuine kotae surface is `src/mcp.rs` (agent-facing JSON). That
//! is where the real crate belongs, and adopting it there is a separate
//! change — deliberately not smuggled in here.
//!
//! ── ★ WHY STALENESS IS NOT A FIFTH ARM ──────────────────────────────────
//! It is tempting to add `Stale`. `kotae`'s closure is deliberate — its
//! docs state "a fifth kind of answer has no value" — and the closure is
//! right: serving a cached list *is* an answer about the world, so it is
//! `Found`. What differs is its **freshness**, which is a property of the
//! payload, not a different kind of answer. Hence [`Freshness`] rides
//! inside [`DiscoveryAnswer::Found`] and cannot be forgotten: you cannot
//! construct a `Found` without saying how fresh it is.
//!
//! That is the same witness discipline `RepoStatus::Clean(RemoteWitness)`
//! already uses in `sync.rs` — a verdict that cannot be stated without its
//! evidence.
//!
//! ── ★ TWO TRAPS MEASURED IN todoku, NOT ASSUMED ─────────────────────────
//! **(1) Do not add a retry loop here.** `todoku`'s client already retries
//! — `RetryPolicy::default()` is 3 retries on `429/500/502/503/504`, and it
//! honours `Retry-After` (`todoku/src/client.rs:498`). `GitHubClient::new`
//! does not override it, and `list_repos` paginates, so each page already
//! carries its own budget. A retry wrapper here would silently become 4×
//! per page against an org we are already being rate-limited by — turning
//! a throttle into an outage. Classification only; no re-issuing.
//!
//! **(2) A timeout does not arrive as `TodokuError::Timeout`.** That
//! variant is constructible, but the client's retry loop returns the raw
//! transport error (`todoku/src/client.rs:543`), so a real GET timeout
//! surfaces as `Request(_)` whose `is_timeout()` is true. Matching only on
//! `Timeout` would classify a timeout as a generic transport fault. Both
//! paths are handled in [`classify`] and both are covered by tests.
//!
//! ── ★ WHAT IS DELIBERATELY NOT MODELLED ─────────────────────────────────
//! Rate-limit *headroom*. `TodokuError::Http` carries only `status` and
//! `body`; `x-ratelimit-remaining`/`-reset` are never read by todoku, and
//! `Retry-After` is parsed but discarded on the error path. So a `429`
//! here can say "we were throttled" but **not** "for another N seconds".
//! Claiming a reset time would be fabrication. tend *does* read those
//! headers elsewhere (`src/operator/discovery.rs`, via raw reqwest, which
//! is precisely why it can see what todoku drops) — unifying those planes
//! is a real follow-up, named here rather than half-done.
//!
//! TIER (do not round up): **only-mitigated**. This is a typed runtime
//! classification with an exhaustive match and a wildcard for
//! `#[non_exhaustive]` upstream growth. It is not unrepresentability: a
//! caller can still ignore a `Blind` if it tries. What it *does* buy is
//! that ignoring one now requires writing code that says so.

use std::fmt;

/// How fresh a `Found` payload is.
///
/// Carried inside [`DiscoveryAnswer::Found`] rather than beside it, so a
/// live answer and a recovered-from-cache answer cannot be confused at a
/// call site that forgot to look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// Observed from the forge during this run.
    Live,
    /// Served from tend's discovery cache because the live call failed.
    ///
    /// `age_secs` is real, not nominal: every cache entry on disk already
    /// carries a unix-epoch `timestamp` (`cache.rs`), so the age is
    /// measured, never estimated. `because` records the failure that made
    /// us fall back — without it, a stale answer would look like a policy
    /// choice rather than a recovery.
    Stale { age_secs: u64, because: String },
}

impl Freshness {
    /// True when this answer came off disk after a live failure.
    pub(crate) const fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }
}

impl fmt::Display for Freshness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Live => f.write_str("live"),
            Self::Stale { age_secs, .. } => {
                write!(f, "stale by {}", human_age(*age_secs))
            }
        }
    }
}

/// Render an age in the coarsest unit that stays honest.
///
/// Deliberately never rounds *down* across a unit boundary in a way that
/// understates staleness: 119 minutes reads "1h", not "2h".
fn human_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs >= DAY {
        format!("{}d", secs / DAY)
    } else if secs >= HOUR {
        format!("{}h", secs / HOUR)
    } else if secs >= MIN {
        format!("{}m", secs / MIN)
    } else {
        format!("{secs}s")
    }
}

/// What happened when we asked a forge to list an org.
///
/// The four arms mirror `kotae::Answer` one-for-one. The distinction that
/// earns the type is `Empty` vs `Blind`: an org with no repositories and an
/// org we could not reach are both "zero repos" to a `Vec::len()`, and
/// collapsing them is how a credential failure becomes a silent cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryAnswer<T> {
    /// We asked and got an answer about the world.
    Found { value: T, freshness: Freshness },
    /// We asked, and the org genuinely holds nothing matching.
    ///
    /// A finding, not an error — an org that really has zero non-archived
    /// repos is a fact worth reporting plainly.
    Empty { of: String },
    /// The question was understood and declined. The caller must change
    /// something (a credential, a scope, a config field) — retrying
    /// unchanged will fail identically forever.
    ///
    /// `legal` names what *would* work, so a refusal is actionable rather
    /// than merely negative. An empty `legal` is allowed but discouraged.
    Refused { because: String, legal: Vec<String> },
    /// The question could not be put at all — transport down, timeout,
    /// upstream 5xx. Unlike `Refused`, retrying later may well succeed.
    Blind { because: String },
}

impl<T> DiscoveryAnswer<T> {
    /// The outcome word. Stable; used in JSON and in the human table, so
    /// the two can never disagree about what happened.
    pub(crate) const fn outcome(&self) -> &'static str {
        match self {
            Self::Found { .. } => "found",
            Self::Empty { .. } => "empty",
            Self::Refused { .. } => "refused",
            Self::Blind { .. } => "blind",
        }
    }

    /// True when this answer is evidence *about the world* rather than
    /// about our ability to look at it.
    ///
    /// Mirrors `kotae::Answer::is_about_the_world`. This is the aggregation
    /// rule: a `refused`/`blind` workspace must never be folded into a
    /// "N clean, N dirty" summary, because those counts are claims about
    /// repositories and we did not observe any.
    pub(crate) const fn is_about_the_world(&self) -> bool {
        matches!(self, Self::Found { .. } | Self::Empty { .. })
    }

    /// True when the operator needs to act before this can ever succeed.
    pub(crate) const fn needs_operator(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }

    /// The payload, if we actually observed one.
    pub(crate) fn found(&self) -> Option<&T> {
        match self {
            Self::Found { value, .. } => Some(value),
            _ => None,
        }
    }

    /// The payload if we observed one, else the failure as an error.
    ///
    /// ── ★ THIS IS THE BACKWARD-COMPATIBILITY SEAM ───────────────────────
    /// Every existing caller of `resolve_repos` keeps its old behaviour
    /// through this: a `Refused`/`Blind` still becomes an `Err` and still
    /// aborts. That is deliberate and it is the *safe* default — a caller
    /// that has not been taught to distinguish blindness must keep failing
    /// loudly rather than silently receiving a short list.
    ///
    /// Opting in to degradation is therefore an explicit act at each call
    /// site (match on the answer), never something that happens to a caller
    /// because a library changed underneath it. `Empty` maps to an empty
    /// vec, which is what it has always meant.
    pub(crate) fn into_result(self) -> anyhow::Result<T>
    where
        T: Default,
    {
        match self {
            Self::Found { value, .. } => Ok(value),
            Self::Empty { .. } => Ok(T::default()),
            Self::Refused { because, legal } => {
                if legal.is_empty() {
                    Err(anyhow::anyhow!("{because}"))
                } else {
                    Err(anyhow::anyhow!(
                        "{because}\n  try: {}",
                        legal.join("\n  try: ")
                    ))
                }
            }
            Self::Blind { because } => Err(anyhow::anyhow!("{because}")),
        }
    }

    /// Why we have no live observation, for display. `None` when we do.
    pub(crate) fn because(&self) -> Option<&str> {
        match self {
            Self::Found { .. } => None,
            Self::Empty { .. } => None,
            Self::Refused { because, .. } | Self::Blind { because } => Some(because),
        }
    }
}

/// A classification of a failed discovery call: the three non-`Found` arms.
///
/// Separated from [`DiscoveryAnswer`] so [`classify`] has nothing to say
/// about payloads — it maps *an error* to *a kind of non-answer*, and the
/// caller decides whether a cache fallback can upgrade it to `Found`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Denial {
    Empty { of: String },
    Refused { because: String, legal: Vec<String> },
    Blind { because: String },
}

impl Denial {
    /// Lift a denial into an answer of any payload type.
    pub(crate) fn into_answer<T>(self) -> DiscoveryAnswer<T> {
        match self {
            Self::Empty { of } => DiscoveryAnswer::Empty { of },
            Self::Refused { because, legal } => DiscoveryAnswer::Refused { because, legal },
            Self::Blind { because } => DiscoveryAnswer::Blind { because },
        }
    }

    /// True when retrying the identical request could succeed later.
    ///
    /// Drives the stale-cache fallback: it is honest to serve a cached list
    /// through a transient outage, and dishonest to serve one when the
    /// credential has been revoked — the first is a blip, the second is a
    /// state change we would be hiding.
    pub(crate) const fn is_transient(&self) -> bool {
        matches!(self, Self::Blind { .. })
    }
}

/// Map a `todoku` failure onto the four-arm vocabulary.
///
/// Written once per surface and tested per status, which is what
/// `docs/kotae.md` prescribes — never a blanket `From<Error>`, because
/// only the call site knows which arm an error means.
///
/// `owner` is the org/user we were asking about; it is woven into the
/// message so a multi-workspace run says *which* org went dark.
pub(crate) fn classify(owner: &str, err: &todoku::TodokuError) -> Denial {
    use todoku::TodokuError as E;

    // Transport-shaped failures first, because a timeout can arrive as
    // `Request(_)` rather than `Timeout(_)` (todoku client.rs:543) and
    // matching on the variant alone would misfile it.
    if let E::Request(re) = err {
        let because = if re.is_timeout() {
            format!("timed out contacting the forge about `{owner}`")
        } else if re.is_connect() {
            format!("could not connect to the forge for `{owner}`")
        } else {
            format!("transport failure contacting the forge about `{owner}`: {re}")
        };
        return Denial::Blind { because };
    }

    match err {
        E::Http { status, body } => classify_status(owner, *status, body),

        // A timeout that *did* surface as its own variant.
        E::Timeout(d) => Denial::Blind {
            because: format!("timed out after {d:?} asking about `{owner}`"),
        },

        // The retry budget is todoku's, and it is already generous. Reaching
        // this means repeated 429/5xx — the forge is unwell, not us.
        E::MaxRetries { max, .. } => Denial::Blind {
            because: format!(
                "gave up after {max} retries asking about `{owner}` — the forge kept failing"
            ),
        },

        // Credentials that never got as far as a response.
        E::Auth(m) => Denial::Refused {
            because: format!("authentication failed for `{owner}`: {m}"),
            legal: vec![
                "a credential this forge accepts".to_owned(),
                "`discover: false` for this workspace".to_owned(),
            ],
        },

        // A malformed header value is ours, not theirs, and no retry fixes
        // it — but it is a bug rather than an operator action, so it is
        // Blind (we could not ask) rather than Refused (they said no).
        E::InvalidHeaderValue(e) => Denial::Blind {
            because: format!("tend built an invalid request header for `{owner}`: {e}"),
        },

        // The forge answered something we could not read. Not evidence of
        // absence.
        E::Deserialize(e) => Denial::Blind {
            because: format!("could not parse the forge's answer about `{owner}`: {e}"),
        },

        // `#[non_exhaustive]` upstream: an unknown failure is Blind, never
        // Empty. Erring toward "we could not see" is the safe direction —
        // the unsafe one is inventing an observation.
        other => Denial::Blind {
            because: format!("could not list `{owner}`: {other}"),
        },
    }
}

/// The HTTP half of [`classify`], split out so every status is directly
/// testable without constructing a transport error.
fn classify_status(owner: &str, status: u16, body: &str) -> Denial {
    match status {
        // The owner does not exist under this endpoint. Callers retry the
        // user endpoint on 404 before ever reaching us (`provider.rs`), so
        // by the time a 404 is classified it means "neither an org nor a
        // user by this name" — a fact about the world, hence Empty.
        404 => Denial::Empty {
            of: format!("`{owner}` — no such org or user on this forge"),
        },

        // Unauthenticated / expired. Actionable, and permanently fatal
        // until acted on.
        401 => Denial::Refused {
            because: format!("the credential was rejected listing `{owner}` (401)"),
            legal: vec![
                "a non-expired token".to_owned(),
                "`gh auth status` to check the active credential".to_owned(),
            ],
        },

        // The distinction that motivated this module. GitHub overloads 403
        // for *both* "you may not" and "you have been rate-limited", and
        // they need opposite handling — one is an operator action, the
        // other resolves itself. todoku drops the rate-limit headers, so
        // the body is the only signal available; matching it is a
        // heuristic and is labelled as one rather than dressed up.
        403 if mentions_rate_limit(body) => Denial::Blind {
            because: format!("rate-limited while listing `{owner}` (403)"),
        },
        403 => Denial::Refused {
            because: format!(
                "this credential may not list `{owner}` (403) — \
                 a fine-grained token cannot read org metadata outside its resource owner"
            ),
            legal: vec![
                format!("a credential whose resource owner is `{owner}`"),
                "`discover: false` on this workspace, with `extra_repos` for what you want"
                    .to_owned(),
            ],
        },

        // Explicit throttling. Transient by definition.
        429 => Denial::Blind {
            because: format!("rate-limited while listing `{owner}` (429)"),
        },

        // Upstream is unwell. todoku already exhausted its retries to get
        // here on the retryable ones.
        500..=599 => Denial::Blind {
            because: format!("the forge returned {status} listing `{owner}`"),
        },

        // Anything else the forge chose to say. Not an observation.
        _ => Denial::Blind {
            because: format!("unexpected {status} listing `{owner}`"),
        },
    }
}

/// Heuristic: does a 403 body indicate throttling rather than permission?
///
/// GitHub's throttling bodies say "API rate limit exceeded" or "secondary
/// rate limit". Kept narrow on purpose — a false positive here downgrades a
/// real permission failure into a transient one, which would let a revoked
/// credential quietly serve stale cache forever. Narrow beats clever.
fn mentions_rate_limit(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("rate limit") || b.contains("abuse detection")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, body: &str) -> todoku::TodokuError {
        todoku::TodokuError::http(status, body.to_owned())
    }

    // ── The distinction the module exists for ───────────────────────────

    #[test]
    fn a_permission_403_is_refused_and_names_a_way_forward() {
        let d = classify("akeylesslabs", &http(403, "Resource not accessible"));
        match &d {
            Denial::Refused { because, legal } => {
                assert!(because.contains("akeylesslabs"), "names the org: {because}");
                assert!(!legal.is_empty(), "a refusal must say what would work");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert!(!d.is_transient(), "a permission failure is not transient");
    }

    #[test]
    fn a_throttled_403_is_blind_not_refused() {
        let d = classify("pleme-io", &http(403, "API rate limit exceeded for user"));
        assert!(
            matches!(d, Denial::Blind { .. }),
            "a rate-limit 403 must not read as a permission failure: {d:?}"
        );
        assert!(d.is_transient(), "throttling resolves on its own");
    }

    #[test]
    fn the_two_403s_do_not_render_the_same_bytes() {
        let perm = classify("o", &http(403, "Resource not accessible"));
        let rate = classify("o", &http(403, "API rate limit exceeded"));
        assert_ne!(perm, rate, "same status, opposite handling — must differ");
    }

    #[test]
    fn a_404_is_empty_a_finding_not_a_failure() {
        let d = classify("ghost-org", &http(404, "Not Found"));
        assert!(matches!(d, Denial::Empty { .. }), "got {d:?}");
        let a: DiscoveryAnswer<Vec<String>> = d.into_answer();
        assert!(
            a.is_about_the_world(),
            "an absent org is evidence about the world"
        );
    }

    #[test]
    fn a_401_is_refused() {
        assert!(matches!(
            classify("o", &http(401, "Bad credentials")),
            Denial::Refused { .. }
        ));
    }

    #[test]
    fn a_429_is_blind_and_transient() {
        let d = classify("o", &http(429, "Too Many Requests"));
        assert!(matches!(d, Denial::Blind { .. }));
        assert!(d.is_transient());
    }

    #[test]
    fn server_errors_are_blind_and_transient() {
        for s in [500u16, 502, 503, 504] {
            let d = classify("o", &http(s, ""));
            assert!(matches!(d, Denial::Blind { .. }), "status {s} -> {d:?}");
            assert!(d.is_transient(), "status {s} should be retryable");
        }
    }

    #[test]
    fn an_unknown_status_is_blind_never_empty() {
        // The dangerous direction: an unrecognised status must never be
        // read as "the org has no repos".
        let d = classify("o", &http(418, "teapot"));
        assert!(matches!(d, Denial::Blind { .. }), "got {d:?}");
        let a: DiscoveryAnswer<Vec<String>> = d.into_answer();
        assert!(!a.is_about_the_world());
    }

    #[test]
    fn max_retries_is_blind_and_says_we_gave_up() {
        let e = todoku::TodokuError::MaxRetries {
            url: "https://api.github.com/orgs/o/repos".to_owned(),
            max: 3,
        };
        match classify("o", &e) {
            Denial::Blind { because } => assert!(because.contains('3'), "{because}"),
            other => panic!("expected Blind, got {other:?}"),
        }
    }

    // ── Aggregation rule ────────────────────────────────────────────────

    #[test]
    fn refused_and_blind_are_not_evidence_about_the_world() {
        let refused: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Refused {
            because: "x".into(),
            legal: vec![],
        };
        let blind: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Blind {
            because: "x".into(),
        };
        assert!(!refused.is_about_the_world());
        assert!(!blind.is_about_the_world());
        assert!(refused.needs_operator());
        assert!(!blind.needs_operator());
    }

    #[test]
    fn an_empty_org_and_an_unreachable_org_are_different_answers() {
        // Both are "zero repos" to a Vec::len(). That collapse is the
        // silent-cleanup bug this whole module exists to prevent.
        let empty: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Empty { of: "o".into() };
        let blind: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Blind {
            because: "net down".into(),
        };
        assert_ne!(empty.outcome(), blind.outcome());
        assert!(empty.is_about_the_world());
        assert!(!blind.is_about_the_world());
    }

    // ── Freshness ───────────────────────────────────────────────────────

    #[test]
    fn a_found_answer_cannot_omit_its_freshness() {
        // Compile-time property, asserted by construction: there is no
        // `Found(value)` constructor without a `freshness` field.
        let live = DiscoveryAnswer::Found {
            value: vec!["a".to_owned()],
            freshness: Freshness::Live,
        };
        assert!(!matches!(
            live,
            DiscoveryAnswer::Found {
                freshness: Freshness::Stale { .. },
                ..
            }
        ));
        assert!(live.is_about_the_world());
    }

    #[test]
    fn stale_reports_a_measured_age_not_a_nominal_one() {
        let f = Freshness::Stale {
            age_secs: 3 * 3600 + 61,
            because: "403".into(),
        };
        assert!(f.is_stale());
        assert_eq!(f.to_string(), "stale by 3h");
    }

    #[test]
    fn age_never_rounds_up_across_a_unit_boundary() {
        assert_eq!(human_age(59), "59s");
        assert_eq!(human_age(60), "1m");
        assert_eq!(human_age(119 * 60), "1h", "119m must not read as 2h");
        assert_eq!(human_age(47 * 3600), "1d", "47h must not read as 2d");
    }

    #[test]
    fn only_transient_denials_may_be_served_from_stale_cache() {
        // The recovery policy, asserted rather than left to a call site:
        // a revoked credential must not quietly serve yesterday's list.
        let revoked = classify("o", &http(403, "Resource not accessible"));
        let outage = classify("o", &http(503, ""));
        assert!(!revoked.is_transient(), "revoked cred must not serve stale");
        assert!(outage.is_transient(), "an outage may serve stale");
    }

    // ── The backward-compatibility contract ─────────────────────────────

    #[test]
    fn into_result_keeps_untaught_callers_failing_loudly() {
        // The safety property of the whole change: a call site that has
        // NOT opted in to degradation must keep aborting. If this ever
        // returns Ok, every legacy caller silently starts receiving short
        // lists — the exact regression that would flip izumi from
        // honestly-unavailable to confidently-wrong.
        let refused: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Refused {
            because: "403".into(),
            legal: vec!["a scoped token".into()],
        };
        assert!(refused.into_result().is_err(), "refused must still abort");

        let blind: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Blind {
            because: "dns".into(),
        };
        assert!(blind.into_result().is_err(), "blind must still abort");
    }

    #[test]
    fn a_refusal_carries_its_remedy_into_the_error_text() {
        let refused: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Refused {
            because: "this credential may not list `akeylesslabs` (403)".into(),
            legal: vec!["`discover: false` on this workspace".into()],
        };
        let msg = refused.into_result().unwrap_err().to_string();
        assert!(msg.contains("akeylesslabs"), "{msg}");
        assert!(
            msg.contains("discover: false"),
            "an operator reading the failure must see the fix: {msg}"
        );
    }

    #[test]
    fn empty_is_the_only_non_found_arm_that_yields_ok() {
        let empty: DiscoveryAnswer<Vec<String>> = DiscoveryAnswer::Empty { of: "o".into() };
        assert_eq!(
            empty
                .into_result()
                .expect("empty is a finding, not an error"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn every_outcome_word_is_distinct() {
        let words = [
            DiscoveryAnswer::<()>::Found {
                value: (),
                freshness: Freshness::Live,
            }
            .outcome(),
            DiscoveryAnswer::<()>::Empty { of: String::new() }.outcome(),
            DiscoveryAnswer::<()>::Refused {
                because: String::new(),
                legal: vec![],
            }
            .outcome(),
            DiscoveryAnswer::<()>::Blind {
                because: String::new(),
            }
            .outcome(),
        ];
        let mut sorted = words.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), words.len(), "no two arms may share a word");
    }
}
