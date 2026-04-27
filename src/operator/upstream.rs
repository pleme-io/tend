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
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
            r#ref: if r#ref.is_empty() { "HEAD".into() } else { r#ref.to_string() },
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
#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub upstream_rev: String,
    pub upstream_modified: i64,
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
        matches!(self, RegistryError::RateLimited { .. } | RegistryError::AuthFailed(_))
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
    async fn head(&self, id: &UpstreamId) -> Result<HeadInfo, RegistryError>;
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
            message: r#"{"message":"... request ID DIFFERENT ... timestamp 2026-04-27 04:46:51 UTC"}"#.to_string(),
        };
        assert_eq!(e1.condition_message(), e2.condition_message());
        assert!(!e1.condition_message().contains("request ID"));
        assert!(!e1.condition_message().contains("E2D8"));
    }

    #[test]
    fn condition_reason_is_stable_camel_case() {
        assert_eq!(
            RegistryError::RateLimited { reset_at: None, message: "x".into() }.condition_reason(),
            "RateLimited"
        );
        assert_eq!(RegistryError::AuthFailed("x".into()).condition_reason(), "AuthFailed");
        assert_eq!(RegistryError::NotFound("x".into()).condition_reason(), "NotFound");
        assert_eq!(RegistryError::Transient("x".into()).condition_reason(), "TransientError");
    }
}
