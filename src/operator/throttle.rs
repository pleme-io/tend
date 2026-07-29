//! `tend throttle` subcommand — the rate-limited GitHub API consumer.
//!
//! Composes:
//!
//!   • samba's `JetStreamPullWorker` for queue + bucket pacing
//!   • tend's `RegistryClient` (the existing GitHub HEAD-resolver) for
//!     the actual upstream dispatch
//!   • the upstream module's typed `UpstreamId` + `HeadOutcome` types
//!
//! The flow:
//!
//!   producers (operator) ──► JetStream stream TEND_GITHUB_JOBS
//!                            (subjects: tend.github.jobs.refresh.<key>)
//!                                          │
//!                                          ▼
//!                            this worker (samba::JetStreamPullWorker)
//!                                          │
//!                                          ▼
//!                            samba::LeakyBucket (≤8 req/min default)
//!                                          │
//!                                          ▼
//!                            this module's `TendGithubApi` impl
//!                                          │
//!                                          ▼
//!                            tend's existing `discovery::head_conditional`
//!                                          │
//!                                          ▼
//!                            api.github.com (with ETag conditional)
//!                                          │
//!                                          ▼
//!                            result published on tend.github.jobs.results.<key>
//!
//! See pleme-io/theory/RATE-LIMITED-CONSUMERS.md for the canonical
//! design + the eight invariants this composition enforces.

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

/// Throttle dispatch error — wraps the upstream RegistryError plus
/// any orchestration failure into a `std::error::Error` impl as
/// required by samba's `UpstreamApi::Error` bound.
#[derive(Debug, Error)]
pub enum ThrottleError {
    /// Upstream registry refused / errored.
    #[error("registry: {0}")]
    Registry(#[from] crate::operator::upstream::RegistryError),
    /// Anything else (auth, config, etc.) — flattened from anyhow.
    #[error("dispatch: {0}")]
    Other(String),
}

impl From<anyhow::Error> for ThrottleError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(format!("{e:#}"))
    }
}

use crate::operator::discovery::ReqwestHeadResolver;
use crate::operator::upstream::{CachedHead, RegistryClient, UpstreamId};

/// Wire payload published by the operator on
/// `tend.github.jobs.refresh.<key>`. Round-trips JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshJobRequest {
    /// Canonical upstream identity (lowercased owner/repo + ref).
    pub id: UpstreamId,
    /// Operator's last-known cached entry for ETag conditional fetch.
    /// `None` ⇒ first lookup, no `If-None-Match` header.
    pub prev: Option<CachedHead>,
}

/// Wire payload published by this worker on
/// `tend.github.jobs.results.<key>`. The operator's subscriber matches
/// on the trailing key segment to wake the awaiting reconcile.
///
/// On dispatch error, the worker NAKs the message instead (JetStream
/// redelivers up to `maxDeliver`); a `RefreshJobResult` is only
/// published on success, including the "unchanged" case (304).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshJobResult {
    /// Whether the upstream responded 304 (no quota cost).
    pub unchanged: bool,
    /// Authoritative head for the operator to write into its cache.
    pub head: CachedHead,
    /// Last-seen `X-RateLimit-Remaining` from the response, if known.
    /// The operator may emit this as a metric of its own; samba's
    /// LeakyBucket already uses it for adaptive shrinkage.
    pub rate_limit_remaining: Option<u32>,
    /// Last-seen `X-RateLimit-Limit` from the response — the
    /// upstream's CURRENT total budget. samba's LeakyBucket uses
    /// this as the base for dynamic rpm derivation
    /// (`quota_pct × this_value / 60`). Lets the configured "1% of
    /// GitHub's quota" track GitHub's actual ceiling rather than
    /// a hardcoded constant.
    #[serde(default)]
    pub rate_limit_total: Option<u32>,
    /// Echo of the original `RefreshJobRequest.id`. Lets fleet
    /// watchers and other result subscribers recover full repo
    /// identity from a result message without parsing the sanitized
    /// subject suffix. Optional for backward compat with old workers.
    #[serde(default)]
    pub id: Option<UpstreamId>,
}

/// `samba::UpstreamApi` impl backed by tend's existing GitHub client.
///
/// Holds a shared `RegistryClient` so the same ETag-aware HEAD path
/// the inline operator uses also flows through here when running as a
/// rate-limited worker.
pub struct TendGithubApi {
    client: Arc<dyn RegistryClient>,
}

impl TendGithubApi {
    /// Construct from an environment variable for the GitHub token.
    /// Returns an error if the token is unset (we won't run the worker
    /// without auth — unauthenticated GitHub gives 60 req/hr per IP,
    /// which defeats the throttle).
    ///
    /// The internal `ReqwestHeadResolver` pacer (also a samba
    /// `LeakyBucket`, via `budget::pacer_from_env`) runs at the
    /// operator's tunable cap (`TEND_BUDGET_MAX_PER_HOUR`, default
    /// 100/hr/pod) — but with the throttle worker as the *single*
    /// drain in the cluster, this inner bucket is mostly redundant
    /// with the outer `JetStreamPullWorker`'s own `LeakyBucket`
    /// (constructed from `samba::Config` in `run()` below). Both are
    /// kept in place — same primitive, two independent scopes — so
    /// the worker is robust when run outside a samba context too
    /// (e.g. CLI smoke-test).
    pub fn from_env() -> Result<Self> {
        let token = load_github_token().context(
            "GITHUB_TOKEN env var unset — refusing to run unauthenticated \
             (60 req/hr per IP defeats the throttle)",
        )?;
        let http = reqwest::Client::builder()
            .user_agent("tend-throttle")
            .build()
            .context("building reqwest client")?;
        let resolver = ReqwestHeadResolver::new(
            http,
            Some(token.expose().to_string()),
            Arc::new(super::budget::pacer_from_env()),
        );
        Ok(Self {
            client: Arc::new(resolver),
        })
    }
}

#[async_trait]
impl samba::UpstreamApi for TendGithubApi {
    const NAME: &'static str = "github";
    /// 5000/hr is the authenticated REST API default for classic +
    /// fine-grained PATs. Used ONLY as a cold-start fallback before
    /// the first response with `X-RateLimit-Limit` arrives — at
    /// which point samba's `LeakyBucket` re-derives its rate from
    /// the observed value (could be 5000, 15000 for GitHub Apps,
    /// 1000 for GHE customs, etc.).
    const INITIAL_BUDGET_PER_HOUR: u32 = 5_000;

    type Request = RefreshJobRequest;
    type Response = RefreshJobResult;
    type Error = ThrottleError;

    async fn dispatch(
        &self,
        req: RefreshJobRequest,
    ) -> std::result::Result<RefreshJobResult, Self::Error> {
        let outcome = self
            .client
            .head_conditional(&req.id, req.prev.as_ref())
            .await?;

        let (unchanged, head) = match outcome {
            crate::operator::upstream::HeadOutcome::Fresh(c) => (false, c),
            crate::operator::upstream::HeadOutcome::Unchanged(c) => (true, c),
        };

        Ok(RefreshJobResult {
            unchanged,
            head,
            rate_limit_remaining: self.client.last_observed_remaining(),
            // ★ The dynamic-limit signal. ReqwestHeadResolver stores
            // the latest X-RateLimit-Limit on every response; samba's
            // worker reads this and re-rates LeakyBucket so the
            // configured quota_pct truly tracks GitHub's reported
            // ceiling — not a hardcoded constant.
            rate_limit_total: self.client.last_observed_total(),
            id: Some(req.id),
        })
    }

    fn rate_limit_remaining(&self, response: &RefreshJobResult) -> Option<u32> {
        response.rate_limit_remaining
    }

    fn rate_limit_total(&self, response: &RefreshJobResult) -> Option<u32> {
        response.rate_limit_total
    }

    fn was_cached_response(&self, response: &RefreshJobResult) -> bool {
        response.unchanged
    }
}

/// Entry point for the `tend throttle` subcommand.
///
/// Loads samba config from the path (or env var), constructs a
/// `TendGithubApi` backed by the existing reqwest client, and runs
/// the JetStream pull worker until SIGTERM.
pub async fn run(config_path: Option<&Path>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tend=debug,samba=debug".into()),
        )
        .init();

    let cfg = match config_path {
        Some(p) => samba::Config::load(p)
            .with_context(|| format!("loading samba config from {}", p.display()))?,
        None => samba::Config::from_env().context("loading samba config from env")?,
    };

    tracing::info!(
        upstream = "github",
        stream = %cfg.nats.stream,
        consumer = %cfg.nats.consumer,
        quota_pct = cfg.rate_limit.quota_pct,
        target_rpm = cfg.target_rpm(),
        "tend throttle worker starting"
    );

    let upstream = TendGithubApi::from_env()?;
    let worker = samba::JetStreamPullWorker::new(upstream, cfg)
        .map_err(|e| anyhow::anyhow!("samba worker init: {e}"))?;
    worker
        .run()
        .await
        .map_err(|e| anyhow::anyhow!("samba worker run: {e}"))?;
    Ok(())
}

/// Delegates to `provider::github_token`. This used to be its own
/// env-only lookup, so a deployment relying on the
/// `~/.config/github/token` fallback authenticated everywhere except
/// here.
fn load_github_token() -> Option<crate::secret::Secret> {
    crate::provider::github_token()
}
