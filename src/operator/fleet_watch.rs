//! Fleet-wide GitHub-org watcher.
//!
//! Replaces the flake.lock follows-edges discovery model with
//! continuous org-scope watching:
//!
//!   1. Periodically discover ALL repos in the configured org via
//!      GitHub's `list_repos` API.
//!   2. Filter to repos that have a `flake.nix` at HEAD (one extra
//!      API call per repo, paced).
//!   3. Run a round-robin publisher that ticks at the throttle's
//!      drain rate (60/rpm seconds) and enqueues one refresh-job
//!      per tick to NATS.
//!   4. Subscribe to results on `<result_subject>.>` and process
//!      every advance as it arrives.
//!
//! The queue stays near depth-zero in steady state because the
//! publisher tick rate equals the throttle's drain rate — but the
//! "stream of work" is continuous, every flake-having org repo gets
//! HEAD-checked every (N_repos × tick_interval) seconds.
//!
//! See pleme-io/theory/RATE-LIMITED-CONSUMERS.md §V (per-consumer
//! ingestion strategy: Mode A steady-trickle).

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::operator::throttle::{RefreshJobRequest, RefreshJobResult};
use crate::operator::upstream::UpstreamId;

/// Subject prefix where the watcher publishes detected advances.
/// One message per advance, on `<prefix>.<sanitized-key>`.
pub const ADVANCE_SUBJECT_PREFIX: &str = "tend.fleet.advances";

/// Wire payload for a detected upstream advance. Published by
/// `fleet_watch` on `tend.fleet.advances.<sanitized-key>`; consumed
/// by `fleet_advance_consumer` to trigger downstream actions
/// (currently: annotate FlakeUpdatePolicy CRs to fire reconcile).
///
/// Carries the full `UpstreamId` so consumers don't need to
/// reverse-engineer the sanitized subject key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetAdvanceEvent {
    pub id: UpstreamId,
    pub from_rev: Option<String>,
    pub to_rev: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// Config for the fleet watcher. Hydrated from env at operator start.
#[derive(Debug, Clone)]
pub struct FleetWatchConfig {
    /// `pleme-io` etc.
    pub org: String,
    /// `nats://...` for both publish + subscribe.
    pub nats_url: String,
    /// Subject prefix for refresh-job publishes (matches throttle worker's filter).
    pub refresh_subject: String,
    /// Subject prefix for result subscriptions.
    pub result_subject: String,
    /// How often to re-enumerate the org.
    pub discovery_interval: Duration,
    /// How often to publish a refresh job — matches throttle drain rate.
    pub tick_interval: Duration,
    /// How long to wait between flake.nix probe calls during discovery
    /// (paced to avoid hammering GitHub during the org scan).
    pub discovery_pace: Duration,
}

impl FleetWatchConfig {
    /// Hydrate from env vars; returns `None` if `TEND_FLEET_WATCH_ENABLED`
    /// is not truthy.
    pub fn from_env() -> Option<Self> {
        let enabled = std::env::var("TEND_FLEET_WATCH_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        let org = std::env::var("TEND_FLEET_WATCH_ORG")
            .unwrap_or_else(|_| "pleme-io".to_string());
        let nats_url = std::env::var("TEND_NATS_URL")
            .unwrap_or_else(|_| "nats://pleme-nats.nats.svc:4222".to_string());
        let refresh_subject = std::env::var("TEND_THROTTLE_REFRESH_SUBJECT")
            .unwrap_or_else(|_| "tend.github.jobs.refresh".to_string());
        let result_subject = std::env::var("TEND_THROTTLE_RESULT_SUBJECT")
            .unwrap_or_else(|_| "tend.github.jobs.results".to_string());
        let discovery_interval_secs = std::env::var("TEND_FLEET_WATCH_DISCOVERY_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 60 && *n <= 86400)
            .unwrap_or(3600);
        let tick_interval_ms = std::env::var("TEND_FLEET_WATCH_TICK_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 1000 && *n <= 600_000)
            .unwrap_or(7500);
        let discovery_pace_ms = std::env::var("TEND_FLEET_WATCH_DISCOVERY_PACE_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 100 && *n <= 60_000)
            .unwrap_or(500);
        Some(Self {
            org,
            nats_url,
            refresh_subject,
            result_subject,
            discovery_interval: Duration::from_secs(discovery_interval_secs),
            tick_interval: Duration::from_millis(tick_interval_ms),
            discovery_pace: Duration::from_millis(discovery_pace_ms),
        })
    }
}

/// Sanitize an `UpstreamId.Display` form into a valid NATS subject
/// segment. Mirrors `nats_throttle::sanitize_key` so publishes from
/// fleet-watch and policy-reconciler hit the same keys.
fn sanitize_key(id: &UpstreamId) -> String {
    id.to_string()
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// Round-robin sequence with stable cursor across mutations.
/// Adding a repo doesn't restart from the front — preserves existing
/// progress through the wheel. Removing a repo it was about to visit
/// just skips ahead.
#[derive(Debug, Default)]
pub struct RoundRobin<T: Clone + Eq + std::hash::Hash> {
    items: Vec<T>,
    index: usize,
}

impl<T: Clone + Eq + std::hash::Hash> RoundRobin<T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            index: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Replace the item set with `next`, preserving the cursor relative
    /// to the previous wheel where possible.
    pub fn replace(&mut self, next: Vec<T>) {
        if next.is_empty() {
            self.items.clear();
            self.index = 0;
            return;
        }
        if self.index >= next.len() {
            self.index %= next.len();
        }
        self.items = next;
    }

    /// Advance + return next item. None if empty.
    pub fn next(&mut self) -> Option<&T> {
        if self.items.is_empty() {
            return None;
        }
        let item = &self.items[self.index];
        self.index = (self.index + 1) % self.items.len();
        Some(item)
    }
}

/// Per-repo state tracked by the watcher.
#[derive(Debug, Clone)]
pub struct RepoState {
    pub id: UpstreamId,
    pub last_seen_rev: Option<String>,
    pub last_check: Option<chrono::DateTime<chrono::Utc>>,
    pub advances_observed: u64,
    pub cached_responses: u64,
}

/// Fleet watcher task.
pub struct FleetWatchTask {
    cfg: FleetWatchConfig,
    nats: async_nats::Client,
    /// Round-robin wheel of currently tracked repos.
    wheel: Arc<RwLock<RoundRobin<UpstreamId>>>,
    /// Per-repo running state. Keyed by sanitized key (matches NATS subject suffix).
    state: Arc<RwLock<HashMap<String, RepoState>>>,
}

impl FleetWatchTask {
    pub async fn from_env() -> Result<Option<Self>> {
        let Some(cfg) = FleetWatchConfig::from_env() else {
            return Ok(None);
        };
        info!(
            org = %cfg.org,
            nats = %cfg.nats_url,
            tick_ms = cfg.tick_interval.as_millis(),
            discovery_secs = cfg.discovery_interval.as_secs(),
            "fleet watcher: connecting"
        );
        let nats = async_nats::connect(&cfg.nats_url)
            .await
            .with_context(|| format!("nats connect {}", cfg.nats_url))?;
        Ok(Some(Self {
            cfg,
            nats,
            wheel: Arc::new(RwLock::new(RoundRobin::new())),
            state: Arc::new(RwLock::new(HashMap::new())),
        }))
    }

    /// Run the watcher's three concurrent loops. Returns when any of
    /// them exits unexpectedly; in practice they run forever.
    pub async fn run(self) {
        let me = Arc::new(self);
        let me1 = me.clone();
        let me2 = me.clone();
        let me3 = me.clone();
        // Run all three loops concurrently. tokio::join blocks on all,
        // so any exit takes the watcher down (then the operator's
        // outer error_policy retries).
        let _ = tokio::join!(
            tokio::spawn(async move { me1.run_discovery_loop().await }),
            tokio::spawn(async move { me2.run_publisher_loop().await }),
            tokio::spawn(async move { me3.run_subscriber_loop().await }),
        );
    }

    /// Periodically re-enumerate the org + filter to flake-having repos.
    /// First iteration runs immediately on startup; subsequent ones at
    /// `discovery_interval` cadence.
    async fn run_discovery_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.cfg.discovery_interval);
        // tokio::time::interval fires immediately on the first tick,
        // which is exactly what we want — discover at startup.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match self.discover().await {
                Ok(repos) => {
                    let count = repos.len();
                    let mut wheel = self.wheel.write().await;
                    wheel.replace(repos.clone());
                    info!(count, "fleet watcher: discovery complete");

                    // Seed state map for any new repos.
                    let mut state = self.state.write().await;
                    for id in &repos {
                        let key = sanitize_key(id);
                        state.entry(key).or_insert_with(|| RepoState {
                            id: id.clone(),
                            last_seen_rev: None,
                            last_check: None,
                            advances_observed: 0,
                            cached_responses: 0,
                        });
                    }
                }
                Err(e) => warn!(error = %format!("{e:#}"), "fleet watcher: discovery failed"),
            }
        }
    }

    /// Tick at `tick_interval`, publish one refresh-job per tick
    /// round-robin. Fire-and-forget — results arrive on the
    /// subscriber loop.
    async fn run_publisher_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.cfg.tick_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // Get next repo from the wheel + clone so we don't hold
            // the lock across publish.
            let next = {
                let mut wheel = self.wheel.write().await;
                wheel.next().cloned()
            };
            let Some(id) = next else {
                debug!("fleet watcher: empty wheel, skipping publish");
                continue;
            };
            // Pull prev cached entry so the throttle worker can do an
            // ETag conditional fetch (304 = free).
            let prev = {
                let state = self.state.read().await;
                let key = sanitize_key(&id);
                state.get(&key).and_then(|s| s.last_seen_rev.as_ref().map(|rev| {
                    crate::operator::upstream::CachedHead {
                        info: crate::operator::upstream::HeadInfo {
                            upstream_rev: rev.clone(),
                            upstream_modified: 0,
                        },
                        // We don't track ETags in the watcher state yet;
                        // the throttle worker's per-pod ETag cache picks
                        // up the slack. This means the FIRST cycle pays
                        // a non-conditional GET; steady-state hits the
                        // worker's cache.
                        etag: None,
                        fetched_at: chrono::Utc::now(),
                    }
                }))
            };

            if let Err(e) = self.publish_refresh(&id, prev.as_ref()).await {
                warn!(id = %id, error = %e, "fleet watcher: publish failed");
            }
        }
    }

    /// Publish one refresh-job to NATS. Fire-and-forget; result comes
    /// back on the subscriber loop. No `Nats-Msg-Id` so the broker
    /// does NOT dedup — coexists with policy-reconciler publishes.
    async fn publish_refresh(
        &self,
        id: &UpstreamId,
        prev: Option<&crate::operator::upstream::CachedHead>,
    ) -> Result<()> {
        let key = sanitize_key(id);
        let subject = format!("{}.{}", self.cfg.refresh_subject, key);
        let req = RefreshJobRequest {
            id: id.clone(),
            prev: prev.cloned(),
        };
        let body = serde_json::to_vec(&req).context("encode RefreshJobRequest")?;
        self.nats
            .publish(subject.clone(), body.into())
            .await
            .with_context(|| format!("nats publish {subject}"))?;
        Ok(())
    }

    /// Subscribe to all results, update state, log advances.
    async fn run_subscriber_loop(self: Arc<Self>) {
        let pattern = format!("{}.>", self.cfg.result_subject);
        let mut sub = match self.nats.subscribe(pattern.clone()).await {
            Ok(s) => s,
            Err(e) => {
                warn!(pattern = %pattern, error = %e, "fleet watcher: subscribe failed");
                return;
            }
        };
        info!(pattern = %pattern, "fleet watcher: subscribed to results");
        use futures::StreamExt;
        while let Some(msg) = sub.next().await {
            if let Err(e) = self.handle_result(msg).await {
                warn!(error = %e, "fleet watcher: result handler failed");
            }
        }
    }

    async fn handle_result(&self, msg: async_nats::Message) -> Result<()> {
        // Subject format: <result_subject>.<sanitized-key>
        let key = msg
            .subject
            .as_str()
            .rsplit('.')
            .next()
            .unwrap_or("unknown")
            .to_string();
        let result: RefreshJobResult =
            serde_json::from_slice(&msg.payload).context("decode RefreshJobResult")?;

        let mut state = self.state.write().await;
        // Prefer the typed id echoed by the throttle worker. Fall
        // back to placeholder if the result is from an old worker
        // (no `id` field) AND we haven't yet seen this key from a
        // discovery cycle.
        let id = result
            .id
            .clone()
            .or_else(|| state.get(&key).map(|s| s.id.clone()))
            .unwrap_or_else(|| UpstreamId::new_github("unknown", "unknown", "HEAD"));
        let entry = state.entry(key.clone()).or_insert_with(|| RepoState {
            id: id.clone(),
            last_seen_rev: None,
            last_check: None,
            advances_observed: 0,
            cached_responses: 0,
        });
        entry.last_check = Some(chrono::Utc::now());
        // Refresh stored id in case the entry was created with a
        // placeholder during a result-before-discovery race.
        if entry.id.key.starts_with("unknown") && !id.key.starts_with("unknown") {
            entry.id = id.clone();
        }

        if result.unchanged {
            entry.cached_responses += 1;
            debug!(key = %key, rev = %result.head.info.upstream_rev, "fleet watcher: 304 (cached)");
        } else {
            let new_rev = result.head.info.upstream_rev.clone();
            let old = entry.last_seen_rev.clone();
            let advanced = old.as_ref().map(|prev| prev != &new_rev).unwrap_or(true);
            if advanced {
                entry.advances_observed += 1;
                info!(
                    key = %key,
                    from = ?old,
                    to = %new_rev,
                    advances = entry.advances_observed,
                    "fleet watcher: ADVANCE detected"
                );

                // Drop the lock before publishing so the publish I/O
                // doesn't block other state updates.
                let evt = FleetAdvanceEvent {
                    id: id.clone(),
                    from_rev: old.clone(),
                    to_rev: new_rev.clone(),
                    observed_at: chrono::Utc::now(),
                };
                drop(state);
                if let Err(e) = self.emit_advance(&key, &evt).await {
                    warn!(key = %key, error = %e, "fleet watcher: emit advance event failed");
                }
                // Re-acquire to write the new rev.
                let mut state = self.state.write().await;
                if let Some(entry) = state.get_mut(&key) {
                    entry.last_seen_rev = Some(new_rev);
                }
            } else {
                entry.last_seen_rev = Some(new_rev);
            }
        }

        Ok(())
    }

    /// Publish a typed `FleetAdvanceEvent` so downstream consumers
    /// (notably `fleet_advance_consumer`) can react. Fire-and-forget
    /// — failures are logged but not retried; next advance recomputes
    /// from the updated state.
    async fn emit_advance(&self, key: &str, evt: &FleetAdvanceEvent) -> Result<()> {
        let subject = format!("{ADVANCE_SUBJECT_PREFIX}.{key}");
        let body = serde_json::to_vec(evt).context("encode FleetAdvanceEvent")?;
        self.nats
            .publish(subject.clone(), body.into())
            .await
            .with_context(|| format!("nats publish {subject}"))?;
        Ok(())
    }

    /// One discovery cycle: list org repos, filter to flake-having.
    async fn discover(&self) -> Result<Vec<UpstreamId>> {
        info!(org = %self.cfg.org, "fleet watcher: discovering org repos");
        let names = crate::provider::discover_github_repos(&self.cfg.org)
            .await
            .with_context(|| format!("listing repos for {}", self.cfg.org))?;
        info!(org = %self.cfg.org, count = names.len(), "fleet watcher: org enumeration");

        // Filter to flake-having. Paced (tick at discovery_pace) so
        // the discovery scan doesn't burst against the GitHub API.
        let token = crate::provider::github_token();
        let client = reqwest::Client::builder()
            .user_agent("tend-fleet-watch")
            .build()
            .context("building reqwest client for discovery")?;

        let mut flake_having = Vec::new();
        let mut tick = tokio::time::interval(self.cfg.discovery_pace);
        for name in &names {
            tick.tick().await;
            match has_flake_nix(&client, &self.cfg.org, name, token.as_deref()).await {
                Ok(true) => {
                    let id = UpstreamId::new_github(&self.cfg.org, name, "HEAD");
                    flake_having.push(id);
                }
                Ok(false) => {
                    debug!(repo = %name, "fleet watcher: no flake.nix");
                }
                Err(e) => {
                    warn!(repo = %name, error = %e, "fleet watcher: flake.nix probe failed");
                }
            }
        }
        info!(
            count = flake_having.len(),
            total = names.len(),
            "fleet watcher: flake-having filter"
        );
        Ok(flake_having)
    }
}

// Helper to give HeadInfo a placeholder UpstreamId for the entry — we
// rely on this only for type compatibility, the actual id is restored
// from the subject key on next publish round.
trait HeadInfoExt {
    fn into_upstream_id_placeholder(self) -> UpstreamId;
}
impl HeadInfoExt for crate::operator::upstream::HeadInfo {
    fn into_upstream_id_placeholder(self) -> UpstreamId {
        UpstreamId::new_github("unknown", "unknown", "HEAD")
    }
}

/// Probe whether `<owner>/<repo>` has a `flake.nix` at HEAD.
/// Uses a HEAD HTTP request against the contents endpoint — minimal
/// payload, valid 200/404 distinction.
async fn has_flake_nix(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
    token: Option<&str>,
) -> Result<bool> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/contents/flake.nix");
    let mut req = client
        .head(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "tend-fleet-watch");
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.with_context(|| format!("HEAD {url}"))?;
    let status = resp.status().as_u16();
    match status {
        200 => Ok(true),
        404 => Ok(false),
        _ => Err(anyhow::anyhow!(
            "unexpected HEAD response status {status} for {owner}/{repo}/flake.nix"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_robin_cycles() {
        let mut rr = RoundRobin::<&'static str>::new();
        rr.replace(vec!["a", "b", "c"]);
        assert_eq!(rr.next(), Some(&"a"));
        assert_eq!(rr.next(), Some(&"b"));
        assert_eq!(rr.next(), Some(&"c"));
        assert_eq!(rr.next(), Some(&"a")); // wraps
    }

    #[test]
    fn round_robin_empty_returns_none() {
        let mut rr = RoundRobin::<&'static str>::new();
        assert_eq!(rr.next(), None);
    }

    #[test]
    fn round_robin_replace_preserves_cursor_when_safe() {
        let mut rr = RoundRobin::<&'static str>::new();
        rr.replace(vec!["a", "b", "c"]);
        rr.next(); // index → 1
        rr.replace(vec!["a", "b", "c", "d"]);
        // Cursor was at 1; new wheel size 4; should still return "b" next.
        assert_eq!(rr.next(), Some(&"b"));
    }

    #[test]
    fn round_robin_replace_with_smaller_wheel_wraps() {
        let mut rr = RoundRobin::<&'static str>::new();
        rr.replace(vec!["a", "b", "c", "d"]);
        rr.next();
        rr.next();
        rr.next(); // index = 3
        rr.replace(vec!["x", "y"]);
        // Cursor 3 % 2 = 1; next returns "y".
        assert_eq!(rr.next(), Some(&"y"));
    }

    #[test]
    fn sanitize_key_strips_unsafe_chars() {
        let id = UpstreamId::new_github("pleme-io", "blackmatter", "HEAD");
        let k = sanitize_key(&id);
        assert!(!k.contains(':'));
        assert!(!k.contains('@'));
        assert!(!k.contains('/'));
    }

    #[test]
    fn from_env_disabled_returns_none() {
        std::env::remove_var("TEND_FLEET_WATCH_ENABLED");
        assert!(FleetWatchConfig::from_env().is_none());
    }
}
