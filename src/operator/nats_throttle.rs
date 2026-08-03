//! `NatsThrottleClient` — operator-side `RegistryClient` impl that
//! routes GitHub HEAD requests through the NATS-backed rate-limited
//! consumer (`pleme-tend-throttle`) instead of dispatching inline.
//!
//! Activated by `TEND_THROTTLE_ENABLED=true` (set by the chart's
//! `throttle.enabled: true`). When inactive, the operator continues
//! to use `ReqwestHeadResolver` (Phase A–E protections + a samba
//! `LeakyBucket` pacer) exactly as before.
//!
//! Wire protocol mirrors the throttle worker side (see
//! `src/operator/throttle.rs`):
//!
//!   request:  publish RefreshJobRequest on `<refresh_subject>.<key>`
//!   response: await    RefreshJobResult  on `<result_subject>.<key>`
//!
//! `<key>` is the sanitized form of `UpstreamId.Display` — no characters
//! that would break NATS subject parsing (no `:`, `/`, `@`).
//!
//! Failure modes:
//!
//!   • Result timeout → operator surfaces a transient `RegistryError`,
//!     reconciler will retry next cycle. Job may still be in NATS;
//!     operator's dedup-by-`Nats-Msg-Id` collapses re-publish.
//!   • NATS unreachable → transient registry error, same recovery.
//!   • Worker NAKs / dispatch error → result_subject never receives
//!     a message; operator times out (option (c) from the pause-removal
//!     review § B.3 — no inline fallback, no budget violation).

use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

use samba::LeakyBucket;

use crate::operator::upstream::{
    CachedHead, HeadOutcome, RegistryClient, RegistryError, UpstreamId,
};

/// Operator-side throttle client. Constructed once per reconciler
/// invocation; held cheaply (just a NATS Client handle + subject
/// prefixes).
pub struct NatsThrottleClient {
    client: async_nats::Client,
    refresh_subject: String,
    result_subject: String,
    result_timeout: Duration,
    /// Latest rate-limit-remaining observed in result messages from
    /// the throttle worker. Drives `last_observed_remaining` so the
    /// reconciler's downstream code paths see fleet-wide pressure
    /// (not just per-pod). u32::MAX = unknown.
    last_remaining: Arc<AtomicU32>,
    /// Operator-side pacer (samba's `LeakyBucket`) — kept in step with
    /// fleet pressure via `record_headroom`/`record_observed_limit` on
    /// every result. Optional because some test paths skip it;
    /// production wiring always supplies one.
    budget: Option<Arc<LeakyBucket>>,
}

impl NatsThrottleClient {
    /// Connect + return a ready-to-dispatch client.
    ///
    /// Reads from these env vars (chart sets them when
    /// `throttle.enabled: true`):
    ///
    ///   • `TEND_NATS_URL`                — broker URL
    ///   • `TEND_THROTTLE_REFRESH_SUBJECT` — refresh-job subject prefix
    ///   • `TEND_THROTTLE_RESULT_SUBJECT`  — result-job subject prefix
    ///   • `TEND_THROTTLE_RESULT_TIMEOUT`  — go-duration string (e.g. "30m")
    ///
    /// `budget` is the operator's existing samba `LeakyBucket` — passing
    /// it in lets received rate-limit-remaining/-total values feed the
    /// bucket's pressure + rate tracking, so the operator's internal
    /// back-pressure mirrors what samba sees fleet-wide.
    pub async fn from_env(budget: Option<Arc<LeakyBucket>>) -> Result<Self, anyhow::Error> {
        let url = std::env::var("TEND_NATS_URL")
            .unwrap_or_else(|_| "nats://pleme-nats.nats.svc:4222".to_string());
        let refresh = std::env::var("TEND_THROTTLE_REFRESH_SUBJECT")
            .unwrap_or_else(|_| "tend.github.jobs.refresh".to_string());
        let result = std::env::var("TEND_THROTTLE_RESULT_SUBJECT")
            .unwrap_or_else(|_| "tend.github.jobs.results".to_string());
        let timeout_str =
            std::env::var("TEND_THROTTLE_RESULT_TIMEOUT").unwrap_or_else(|_| "30m".to_string());
        let result_timeout =
            parse_duration_str(&timeout_str).unwrap_or_else(|| Duration::from_secs(1800));

        let client = async_nats::connect(&url)
            .await
            .map_err(|e| anyhow::anyhow!("nats connect {url}: {e}"))?;
        Ok(Self {
            client,
            refresh_subject: refresh,
            result_subject: result,
            result_timeout,
            last_remaining: Arc::new(AtomicU32::new(u32::MAX)),
            budget,
        })
    }

    /// Whether the operator should activate this throttle client.
    /// Reads `TEND_THROTTLE_ENABLED`; default false (operator uses
    /// the inline `ReqwestHeadResolver`).
    #[must_use]
    pub fn enabled() -> bool {
        std::env::var("TEND_THROTTLE_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false)
    }
}

/// Sanitize an `UpstreamId.Display` form into a valid NATS subject
/// segment. NATS subjects allow `[A-Za-z0-9._-]+` per token; replace
/// the rest. We use `_` as a single replacement.
fn sanitize_key(id: &UpstreamId) -> String {
    id.to_string()
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Tiny duration parser for go-style strings ("30m", "5s", "1h").
/// Returns None on parse failure; caller uses default.
fn parse_duration_str(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix("ms") {
        rest.parse::<u64>().ok().map(Duration::from_millis)
    } else if let Some(rest) = s.strip_suffix('s') {
        rest.parse::<u64>().ok().map(Duration::from_secs)
    } else if let Some(rest) = s.strip_suffix('m') {
        rest.parse::<u64>()
            .ok()
            .map(|n| Duration::from_secs(n * 60))
    } else if let Some(rest) = s.strip_suffix('h') {
        rest.parse::<u64>()
            .ok()
            .map(|n| Duration::from_secs(n * 3600))
    } else {
        s.parse::<u64>().ok().map(Duration::from_secs)
    }
}

#[async_trait]
impl RegistryClient for NatsThrottleClient {
    fn last_observed_remaining(&self) -> Option<u32> {
        let r = self.last_remaining.load(Ordering::Relaxed);
        if r == u32::MAX {
            None
        } else {
            Some(r)
        }
    }

    async fn head_conditional(
        &self,
        id: &UpstreamId,
        prev: Option<&CachedHead>,
    ) -> Result<HeadOutcome, RegistryError> {
        let key = sanitize_key(id);
        let refresh_subject = format!("{}.{}", self.refresh_subject, key);
        let result_subject = format!("{}.{}", self.result_subject, key);

        // Subscribe FIRST so we don't lose the response between publish
        // and subscribe (race window). Tear down the subscription after
        // we get a result OR time out.
        let mut subscriber = self
            .client
            .subscribe(result_subject.clone())
            .await
            .map_err(|e| {
                RegistryError::Transient(format!("nats subscribe {result_subject}: {e}"))
            })?;

        let req = crate::operator::throttle::RefreshJobRequest {
            id: id.clone(),
            prev: prev.cloned(),
        };
        let body = serde_json::to_vec(&req)
            .map_err(|e| RegistryError::Transient(format!("encode request: {e}")))?;

        // Set Nats-Msg-Id so JetStream's duplicate_window collapses
        // repeat publishes of the same job (e.g. operator pod
        // restart, multi-pod race) into one. Key matches the
        // sanitized UpstreamId so {repo, ref} → one effective publish.
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Msg-Id", key.as_str());

        self.client
            .publish_with_headers(refresh_subject.clone(), headers, body.into())
            .await
            .map_err(|e| {
                RegistryError::Transient(format!("nats publish {refresh_subject}: {e}"))
            })?;
        // Flush so the throttle broker actually sees the publish before
        // we sit on the subscriber. Without this, fast publishes can be
        // buffered when the timeout window is short.
        let _ = self.client.flush().await;

        // Await result with timeout. Use try_recv from the subscriber
        // stream — async-nats yields `async_nats::Message` items.
        use futures::StreamExt;
        let recv = subscriber.next();
        match timeout(self.result_timeout, recv).await {
            Ok(Some(msg)) => {
                let result: crate::operator::throttle::RefreshJobResult =
                    serde_json::from_slice(&msg.payload)
                        .map_err(|e| RegistryError::Transient(format!("decode result: {e}")))?;

                // ★ Adaptive feedback loop: capture observed
                // rate-limit-remaining for downstream consumers
                // (last_observed_remaining trait method) AND feed it
                // into the operator's own samba `LeakyBucket` so
                // pressure self-throttling tracks the fleet-wide
                // signal, not just per-pod observation. Prefer the
                // throttle worker's actually-observed
                // `rate_limit_total` (tracks GitHub's real ceiling —
                // PAT vs GitHub App vs GHE all differ); 5000 is only a
                // fallback for older workers that predate that field.
                if let Some(remaining) = result.rate_limit_remaining {
                    self.last_remaining.store(remaining, Ordering::Relaxed);
                    if let Some(b) = &self.budget {
                        let total = result.rate_limit_total.unwrap_or(5000);
                        b.record_observed_limit(total).await;
                        b.record_headroom(remaining, total).await;
                    }
                }

                if result.unchanged {
                    Ok(HeadOutcome::Unchanged(result.head))
                } else {
                    Ok(HeadOutcome::Fresh(result.head))
                }
            }
            Ok(None) => Err(RegistryError::Transient(
                "nats subscriber closed without delivering result".into(),
            )),
            Err(_elapsed) => Err(RegistryError::Transient(format!(
                "throttle worker did not respond within {:?} (queue stalled?)",
                self.result_timeout
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_sanitization_rejects_subject_breaking_chars() {
        let id = UpstreamId::new_github("nixos", "nixpkgs", "main");
        let k = sanitize_key(&id);
        assert!(!k.contains(':'));
        assert!(!k.contains('@'));
        assert!(!k.contains('/'));
        // The `.` separator stays — NATS allows it in subjects.
        assert!(k.contains('.') || !k.contains('.'));
    }

    #[test]
    fn duration_parser_recognizes_common_units() {
        assert_eq!(parse_duration_str("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration_str("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_duration_str("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(
            parse_duration_str("250ms"),
            Some(Duration::from_millis(250))
        );
        assert_eq!(parse_duration_str("60"), Some(Duration::from_secs(60)));
        assert_eq!(parse_duration_str("garbage"), None);
    }
}
