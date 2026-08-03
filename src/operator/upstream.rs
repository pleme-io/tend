//! Upstream identity primitives shared across every per-domain watcher.
//!
//! Why this exists: discovery layers need to deduplicate work (don't poll
//! the same upstream twice in a cycle) and back off cleanly when a registry
//! rate-limits. Both behaviors require a typed identity for "the upstream
//! we're polling" that's normalized at construction.
//!
//! The flake-only domain learned this the hard way: `NixOS/nixpkgs` and
//! `nixos/nixpkgs` are the same GitHub repo but a string-keyed cache treats
//! them as distinct, leading to 5000+/hr API calls on a single reconcile
//! and immediate rate-limit exhaustion. `UpstreamId::new_github` normalizes
//! case at the boundary so the cache key is canonical.
//!
//! Phase 2-4 watchers (Helm OCI, crates.io, image registries) get the same
//! protection by constructing their own `UpstreamId` variants — `SourceKind`
//! is open to additional cases as those domains land.

use std::fmt;

/// A normalized identity for an upstream pin source. Construct via the
/// `new_*` constructors so case + scheme normalization happens once at
/// the boundary; downstream code treats `UpstreamId` as opaque-by-key.
///
/// Serialize/Deserialize derived so the persistent HEAD cache can
/// round-trip these as JSON map keys (encoded via `to_string()` Display).
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct UpstreamId {
    pub source: SourceKind,
    /// Canonical identifier within the source. For GitHub: `owner/repo`
    /// lowercased. For OCI: `<host>/<path>` lowercased. For crates.io:
    /// crate name (already lowercase by registry convention).
    pub key: String,
    /// The branch/tag/version axis the consumer follows. `"HEAD"` for
    /// "whatever the default branch points at". Free-form per source.
    pub r#ref: String,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SourceKind {
    Github,
    OciRegistry,
    CratesIo,
    ImageRegistry,
}

impl UpstreamId {
    /// GitHub repo identity. Normalizes owner+repo to lowercase because
    /// GitHub's API is case-insensitive but accepts any casing in the
    /// path; without normalization, `NixOS/nixpkgs` and `nixos/nixpkgs`
    /// hit cache as distinct keys and produce duplicate API calls.
    #[must_use]
    pub fn new_github(owner: &str, repo: &str, r#ref: &str) -> Self {
        Self {
            source: SourceKind::Github,
            key: format!("{}/{}", owner.to_lowercase(), repo.to_lowercase()),
            r#ref: if r#ref.is_empty() {
                "HEAD".into()
            } else {
                r#ref.to_string()
            },
        }
    }

    /// OCI registry chart/image identity. Phase 2 / 4. Hostname +
    /// path lowercased per OCI registry distribution spec; tag is
    /// case-sensitive per registry policy so left as-is.
    #[must_use]
    pub fn new_oci(host: &str, path: &str, tag: &str) -> Self {
        Self {
            source: SourceKind::OciRegistry,
            key: format!("{}/{}", host.to_lowercase(), path.to_lowercase()),
            r#ref: tag.to_string(),
        }
    }

    /// crates.io crate identity. Phase 3. Crate names are already
    /// canonicalized lowercase by the registry.
    #[must_use]
    pub fn new_crate(name: &str, channel: &str) -> Self {
        Self {
            source: SourceKind::CratesIo,
            key: name.to_lowercase(),
            r#ref: channel.to_string(),
        }
    }

    /// Container image registry identity. Phase 4.
    #[must_use]
    pub fn new_image(registry: &str, image: &str, tag: &str) -> Self {
        Self {
            source: SourceKind::ImageRegistry,
            key: format!("{}/{}", registry.to_lowercase(), image.to_lowercase()),
            r#ref: tag.to_string(),
        }
    }
}

impl fmt::Display for UpstreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.source {
            SourceKind::Github => "github",
            SourceKind::OciRegistry => "oci",
            SourceKind::CratesIo => "crate",
            SourceKind::ImageRegistry => "image",
        };
        write!(f, "{prefix}:{}@{}", self.key, self.r#ref)
    }
}

/// Outcome of a HEAD lookup. `Advance` means upstream moved past the
/// caller's pin; `Stable` means caller is current.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HeadInfo {
    pub upstream_rev: String,
    pub upstream_modified: i64,
}

/// Cached HEAD lookup, keyed by `UpstreamId`. The `etag` is the
/// load-bearing field for under-the-radar polling: when present, the
/// next `head_conditional` call sends `If-None-Match: <etag>`. GitHub
/// returns 304 Not Modified for matching content, **and 304 responses
/// don't count against the rate limit**. Most polls are unchanged →
/// most polls are free.
///
/// `fetched_at` lets a future TTL-aware planner decide whether to
/// refresh at all (Phase B of the under-the-radar redesign).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedHead {
    pub info: HeadInfo,
    pub etag: Option<String>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of a conditional HEAD lookup.
#[derive(Debug, Clone)]
pub enum HeadOutcome {
    /// 200 OK with fresh data. Caller persists `CachedHead` to its cache.
    Fresh(CachedHead),
    /// 304 Not Modified — prior cached data is still authoritative.
    /// The request did NOT count against the registry's rate limit.
    /// Caller may bump `fetched_at` on the cached entry but doesn't
    /// need to invalidate.
    Unchanged(CachedHead),
}

impl HeadOutcome {
    /// The `CachedHead` regardless of fresh/unchanged — both are
    /// equivalently authoritative for "the upstream is at this rev now".
    #[must_use]
    pub fn cached(&self) -> &CachedHead {
        match self {
            HeadOutcome::Fresh(c) | HeadOutcome::Unchanged(c) => c,
        }
    }

    /// True when the request was free (304). Useful for telemetry.
    #[must_use]
    pub fn was_unchanged(&self) -> bool {
        matches!(self, HeadOutcome::Unchanged(_))
    }
}

/// Errors a `RegistryClient` can surface in a way the discovery loop
/// must distinguish — rate limits halt the cycle (don't make it worse),
/// auth failures need operator attention, transient errors are skipped
/// per-input.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// Registry is rate-limiting. Discovery should abort the rest of
    /// this cycle and wait for the next requeue. `reset_at` (unix
    /// epoch seconds) is when the limit resets if known.
    #[error("rate limited (reset_at={reset_at:?}): {message}")]
    RateLimited {
        reset_at: Option<i64>,
        message: String,
    },
    /// Auth credentials were rejected. Surfaces as a status condition;
    /// no point retrying without operator intervention.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// Upstream resource doesn't exist (or PAT can't see it). Skip
    /// this one; other inputs still progress.
    #[error("not found: {0}")]
    NotFound(String),
    /// Anything else — network blip, malformed response, transient
    /// upstream issue. Skip + retry next cycle.
    #[error("transient error: {0}")]
    Transient(String),
}

impl RegistryError {
    /// True when the error means "stop this cycle entirely, retry later".
    /// False when it means "skip this input, continue with others".
    #[must_use]
    pub fn is_fatal_to_cycle(&self) -> bool {
        matches!(
            self,
            RegistryError::RateLimited { .. } | RegistryError::AuthFailed(_)
        )
    }

    /// K8s-style condition `reason` (CamelCase, stable across cycles).
    #[must_use]
    pub fn condition_reason(&self) -> &'static str {
        match self {
            RegistryError::RateLimited { .. } => "RateLimited",
            RegistryError::AuthFailed(_) => "AuthFailed",
            RegistryError::NotFound(_) => "NotFound",
            RegistryError::Transient(_) => "TransientError",
        }
    }

    /// Stable, operator-readable condition `message`. Crucially does NOT
    /// embed raw HTTP response bodies — those carry per-request IDs and
    /// timestamps that would churn every reconcile and self-trigger the
    /// Controller into a tight loop. `reset_at` is stable for the
    /// duration of a rate-limit window so it's safe to include.
    #[must_use]
    pub fn condition_message(&self) -> String {
        match self {
            RegistryError::RateLimited { reset_at, .. } => match reset_at {
                Some(ts) => format!("rate limited (resets at unix={ts})"),
                None => "rate limited".to_string(),
            },
            RegistryError::AuthFailed(_) => "auth credentials rejected".to_string(),
            RegistryError::NotFound(s) => format!("upstream not found: {s}"),
            RegistryError::Transient(_) => "transient registry error".to_string(),
        }
    }
}

/// A registry-agnostic HEAD lookup primitive. Phase 1 has one impl
/// (GitHub via `discovery::ReqwestHeadResolver`). Phase 2-4 add
/// implementations for OCI / crates.io / image registries.
///
/// Implementations should map their HTTP-level errors to
/// `RegistryError` variants so the discovery layer's cycle-management
/// works uniformly across domains.
#[async_trait::async_trait]
pub trait RegistryClient: Send + Sync {
    /// Last observed rate-limit-remaining from the underlying registry.
    ///
    /// Used by samba's adaptive backoff to drive the `LeakyBucket`
    /// pressure level. Returning `None` (the default) is correct for
    /// registries that don't expose remaining; clients that DO see
    /// the header should override and return the latest reading.
    ///
    /// Implementations must be cheap (no I/O) — typically reads an
    /// atomic / mutex updated by `head_conditional`.
    fn last_observed_remaining(&self) -> Option<u32> {
        None
    }

    /// Last observed rate-limit-TOTAL from the underlying registry
    /// (e.g. `X-RateLimit-Limit` for GitHub). Lets samba's
    /// `LeakyBucket` derive its actual rpm dynamically as
    /// `quota_pct × observed_total / 60`, so the configured "1% of
    /// GitHub's quota" really means 1% of whatever GitHub reports
    /// (which differs across token types and over time).
    ///
    /// Default `None` for registries that don't expose a total.
    /// Implementations must be cheap (no I/O).
    fn last_observed_total(&self) -> Option<u32> {
        None
    }

    /// Conditional HEAD lookup.
    ///
    /// When `prev` is `Some(c)` and `c.etag` is `Some`, the
    /// implementation should send `If-None-Match: <etag>` (or the
    /// equivalent for non-HTTP registries) so the upstream can return
    /// "unchanged" without spending rate-limit budget on a full
    /// response. When `prev` is `None`, this is a fresh lookup.
    ///
    /// Implementations are responsible for mapping HTTP-level errors
    /// to `RegistryError` variants and (where the protocol exposes
    /// them) calling the pacer's `record_headroom`/`record_observed_limit`
    /// (samba's `LeakyBucket`) with the remaining/limit headers so the
    /// operator self-throttles before hitting the cliff.
    async fn head_conditional(
        &self,
        id: &UpstreamId,
        prev: Option<&CachedHead>,
    ) -> Result<HeadOutcome, RegistryError>;

    /// Convenience for callers that don't have a cached entry —
    /// equivalent to `head_conditional(id, None).await` returning the
    /// fresh `HeadInfo` directly. Default impl in terms of the
    /// conditional method; overriders should rarely need this.
    async fn head(&self, id: &UpstreamId) -> Result<HeadInfo, RegistryError> {
        match self.head_conditional(id, None).await? {
            HeadOutcome::Fresh(c) | HeadOutcome::Unchanged(c) => Ok(c.info),
        }
    }

    /// Optional batch lookup. Default impl serializes via repeated
    /// `head_conditional` calls, so overriders are NOT required —
    /// they just unlock the per-request quota multiplier.
    ///
    /// For GitHub: a GraphQL query batching N repos in one request
    /// (one alias per repo) returns each ref's commit SHA in a single
    /// API call. 50:1 quota reduction. Overriding implementations
    /// MUST preserve the per-id ETag semantics (when supported by the
    /// underlying protocol; GraphQL doesn't have ETags so batched
    /// lookups are always "fresh" — the per-input cache TTL still
    /// short-circuits before the batch fires).
    ///
    /// Result vector parallels the input `targets` slice. Errors per
    /// target are returned in-place so partial failure doesn't poison
    /// the whole batch.
    async fn head_conditional_batch(
        &self,
        targets: &[(UpstreamId, Option<CachedHead>)],
    ) -> Vec<Result<HeadOutcome, RegistryError>> {
        let mut out = Vec::with_capacity(targets.len());
        for (id, prev) in targets {
            out.push(self.head_conditional(id, prev.as_ref()).await);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_normalization_makes_keys_match_across_case() {
        let a = UpstreamId::new_github("NixOS", "nixpkgs", "HEAD");
        let b = UpstreamId::new_github("nixos", "nixpkgs", "HEAD");
        let c = UpstreamId::new_github("NIXOS", "NIXPKGS", "HEAD");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.key, "nixos/nixpkgs");
    }

    #[test]
    fn ref_defaults_to_HEAD_when_empty() {
        let a = UpstreamId::new_github("nixos", "nixpkgs", "");
        assert_eq!(a.r#ref, "HEAD");
    }

    #[test]
    fn oci_lowercases_host_and_path_but_keeps_tag_case() {
        let id = UpstreamId::new_oci("Ghcr.IO", "Pleme-IO/Charts/Foo", "amd64-2216c1d");
        assert_eq!(id.key, "ghcr.io/pleme-io/charts/foo");
        assert_eq!(id.r#ref, "amd64-2216c1d");
    }

    #[test]
    fn rate_limited_is_fatal_to_cycle() {
        let e = RegistryError::RateLimited {
            reset_at: Some(123),
            message: "x".into(),
        };
        assert!(e.is_fatal_to_cycle());
    }

    #[test]
    fn not_found_is_skip_not_fatal() {
        let e = RegistryError::NotFound("x".into());
        assert!(!e.is_fatal_to_cycle());
    }

    #[test]
    fn display_includes_source_prefix() {
        let id = UpstreamId::new_github("nixos", "nixpkgs", "HEAD");
        assert_eq!(format!("{id}"), "github:nixos/nixpkgs@HEAD");
    }

    #[test]
    fn condition_message_excludes_volatile_fields() {
        // The whole point: the raw HTTP body has request IDs and
        // timestamps that change every cycle. condition_message must
        // produce a string stable across the rate-limit window so the
        // status-write loop-breaker can dedup.
        let body = r#"{"message":"API rate limit exceeded ... request ID E2D8:3ACA70 ... timestamp 2026-04-27 04:46:48 UTC"}"#;
        let e1 = RegistryError::RateLimited {
            reset_at: Some(1777265408),
            message: body.to_string(),
        };
        let e2 = RegistryError::RateLimited {
            reset_at: Some(1777265408),
            message:
                r#"{"message":"... request ID DIFFERENT ... timestamp 2026-04-27 04:46:51 UTC"}"#
                    .to_string(),
        };
        assert_eq!(e1.condition_message(), e2.condition_message());
        assert!(!e1.condition_message().contains("request ID"));
        assert!(!e1.condition_message().contains("E2D8"));
    }

    #[test]
    fn condition_reason_is_stable_camel_case() {
        assert_eq!(
            RegistryError::RateLimited {
                reset_at: None,
                message: "x".into()
            }
            .condition_reason(),
            "RateLimited"
        );
        assert_eq!(
            RegistryError::AuthFailed("x".into()).condition_reason(),
            "AuthFailed"
        );
        assert_eq!(
            RegistryError::NotFound("x".into()).condition_reason(),
            "NotFound"
        );
        assert_eq!(
            RegistryError::Transient("x".into()).condition_reason(),
            "TransientError"
        );
    }
}
