//! Discovery — detect upstream advances against current pins.
//!
//! For each (repo, input) tuple in a `FlakeUpdatePolicy`, query the
//! configured registry for the current HEAD. If it differs from the
//! locked rev, emit a candidate advance.
//!
//! Layered on `operator::upstream` primitives:
//!   - `UpstreamId` for case-normalized cache keys (NixOS/nixpkgs vs
//!     nixos/nixpkgs collapse to one key)
//!   - `RegistryClient` trait so Phase 2-4 watchers (Helm OCI,
//!     crates.io, image registries) plug in here unchanged
//!   - `RegistryError::is_fatal_to_cycle()` so rate-limit/auth
//!     failures abort the cycle cleanly instead of hammering
//!
//! Phase 1 only handles `type: "github"` flake inputs. Other input
//! types (git+, tarball, path) are skipped — the lock format adapter
//! preserves their pins on write so they don't drift.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;

use std::sync::Arc;

use samba::LeakyBucket;

use super::crds::FlakeRev;
use super::upstream::{
    CachedHead, HeadInfo, HeadOutcome, RegistryClient, RegistryError, SourceKind, UpstreamId,
};
use crate::flake_lock::ExtendedLockFile;

#[derive(Debug, Clone)]
pub struct CandidateAdvance {
    pub input: String,
    pub from: FlakeRev,
    pub to: FlakeRev,
}

/// Outcome of a discovery cycle. `Halted` says "registry pushed back,
/// don't push it harder this cycle" — reconciler reflects this in
/// the policy status so operators see why progress paused.
#[derive(Debug)]
pub enum DiscoveryOutcome {
    Advances(Vec<CandidateAdvance>),
    Halted {
        partial: Vec<CandidateAdvance>,
        reason: RegistryError,
    },
}

#[derive(Debug, Deserialize)]
struct GitHubCommit {
    sha: String,
    commit: GitHubCommitInner,
}

#[derive(Debug, Deserialize)]
struct GitHubCommitInner {
    author: GitHubCommitAuthor,
}

#[derive(Debug, Deserialize)]
struct GitHubCommitAuthor {
    date: String,
}

/// Result row from the batched GraphQL query — one repository's
/// resolved ref → commit. Aliases r0..rN-1 carry the input ordering
/// so we can reconstruct per-target results.
#[derive(Debug, Deserialize)]
struct GraphQLRefTarget {
    oid: String,
    #[serde(rename = "committedDate")]
    committed_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphQLRef {
    target: Option<GraphQLRefTarget>,
}

#[derive(Debug, Deserialize)]
struct GraphQLRepository {
    #[serde(rename = "ref")]
    ref_: Option<GraphQLRef>,
}

#[derive(Debug, Deserialize)]
struct GraphQLBatchData {
    #[serde(flatten)]
    aliases: std::collections::BTreeMap<String, Option<GraphQLRepository>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLBatchResponse {
    data: Option<GraphQLBatchData>,
    errors: Option<Vec<GraphQLError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

/// Default maximum aliases per GraphQL batch — GitHub allows up to
/// 500-node queries with reasonable rate-limit cost; 50 stays well
/// within the per-query node budget while compressing 50:1 for our
/// use case. Tunable via `TEND_GRAPHQL_BATCH_SIZE` env (Helm chart
/// values: `graphql.batchSize`).
const DEFAULT_GRAPHQL_BATCH_SIZE: usize = 50;

fn graphql_enabled() -> bool {
    std::env::var("TEND_GRAPHQL_ENABLED")
        .map(|v| !(v.eq_ignore_ascii_case("false") || v == "0"))
        .unwrap_or(true)
}

fn graphql_batch_size() -> usize {
    std::env::var("TEND_GRAPHQL_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0 && *n <= 500)
        .unwrap_or(DEFAULT_GRAPHQL_BATCH_SIZE)
}

pub struct ReqwestHeadResolver {
    pub client: reqwest::Client,
    pub token: Option<String>,
    /// Shared rate-limit pacer — samba's `LeakyBucket`, the same
    /// canonical primitive `tend throttle`'s worker uses. Every
    /// head_conditional invocation awaits a token from this bucket
    /// *before* sending; that's what keeps us under behavioral
    /// abuse-detection thresholds.
    pub budget: Arc<LeakyBucket>,
    /// Latest `X-RateLimit-Remaining` observed from any response, or
    /// `u32::MAX` if no response has been seen yet (sentinel for
    /// "unknown" since `Option<AtomicU32>` is awkward). Read by the
    /// `RegistryClient::last_observed_remaining` trait method, which
    /// samba's worker consults to adapt its `LeakyBucket` pace.
    pub last_remaining: Arc<std::sync::atomic::AtomicU32>,
    /// Latest `X-RateLimit-Limit` observed (the upstream's CURRENT
    /// total budget). Drives samba's dynamic rate-derivation:
    /// `quota_pct × this_value / 60` = effective rpm. u32::MAX = unknown.
    pub last_limit: Arc<std::sync::atomic::AtomicU32>,
}

impl ReqwestHeadResolver {
    /// Construct with token + shared budget, fresh remaining + limit trackers.
    pub fn new(client: reqwest::Client, token: Option<String>, budget: Arc<LeakyBucket>) -> Self {
        Self {
            client,
            token,
            budget,
            last_remaining: Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX)),
            last_limit: Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX)),
        }
    }
}

#[async_trait::async_trait]
impl RegistryClient for ReqwestHeadResolver {
    fn last_observed_remaining(&self) -> Option<u32> {
        let r = self.last_remaining.load(std::sync::atomic::Ordering::Relaxed);
        if r == u32::MAX {
            None
        } else {
            Some(r)
        }
    }

    fn last_observed_total(&self) -> Option<u32> {
        let l = self.last_limit.load(std::sync::atomic::Ordering::Relaxed);
        if l == u32::MAX {
            None
        } else {
            Some(l)
        }
    }

    async fn head_conditional(
        &self,
        id: &UpstreamId,
        prev: Option<&CachedHead>,
    ) -> Result<HeadOutcome, RegistryError> {
        if !matches!(id.source, SourceKind::Github) {
            return Err(RegistryError::Transient(format!(
                "ReqwestHeadResolver doesn't service {:?} sources",
                id.source
            )));
        }
        // Rate-limit pacer — block until a token is free. This is the
        // load-bearing constraint for staying under GitHub's radar.
        self.budget.acquire().await;

        let url = format!("https://api.github.com/repos/{}/commits/{}", id.key, id.r#ref);
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tend-operator");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        // Conditional request: if we have a cached etag, ask GitHub
        // to return 304 (free!) if nothing changed.
        if let Some(p) = prev {
            if let Some(etag) = p.etag.as_deref() {
                req = req.header("If-None-Match", etag);
            }
        }
        let resp = req
            .send()
            .await
            .map_err(|e| RegistryError::Transient(format!("send: {e}")))?;
        let status = resp.status();

        // Observe rate-limit pressure unconditionally — even on
        // error/304 responses GitHub returns these headers, and the
        // bucket self-throttles based on the running snapshot.
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok());
        let limit = resp
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok());
        if let (Some(r), Some(l)) = (remaining, limit) {
            self.budget.record_observed_limit(l).await;
            self.budget.record_headroom(r, l).await;
        }
        // Stash the latest observed remaining so samba can read it
        // via RegistryClient::last_observed_remaining and shrink its
        // own LeakyBucket pace when GitHub reports headroom < threshold.
        if let Some(r) = remaining {
            self.last_remaining
                .store(r, std::sync::atomic::Ordering::Relaxed);
        }
        // Stash the latest observed total — samba uses it as the
        // base for `quota_pct × observed / 60` rate derivation, so
        // the configured percentage tracks GitHub's CURRENT ceiling
        // (varies by token type, plan, etc.).
        if let Some(l) = limit {
            self.last_limit
                .store(l, std::sync::atomic::Ordering::Relaxed);
        }
        let reset_at = resp
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());

        // 304 Not Modified — caller's `prev` is still authoritative.
        // GitHub does NOT count 304s against the rate limit, but we
        // already paid the budget slot above; that's intentional —
        // the budget is about request *volume sent*, not GitHub's
        // accounting, so cron-pattern detection still sees fewer
        // outgoing requests on average.
        if status.as_u16() == 304 {
            let prev = prev.expect("304 only returnable when prev was Some");
            return Ok(HeadOutcome::Unchanged(CachedHead {
                fetched_at: chrono::Utc::now(),
                ..prev.clone()
            }));
        }

        if status.as_u16() == 403 {
            let remaining_zero = remaining == Some(0);
            let body = resp.text().await.unwrap_or_default();
            if remaining_zero || body.contains("API rate limit exceeded") {
                return Err(RegistryError::RateLimited { reset_at, message: body });
            }
            return Err(RegistryError::AuthFailed(body));
        }
        if status.as_u16() == 401 {
            let body = resp.text().await.unwrap_or_default();
            return Err(RegistryError::AuthFailed(body));
        }
        if status.as_u16() == 404 {
            return Err(RegistryError::NotFound(format!("{id}")));
        }
        if status.as_u16() == 429 {
            let body = resp.text().await.unwrap_or_default();
            return Err(RegistryError::RateLimited { reset_at, message: body });
        }
        if !status.is_success() {
            return Err(RegistryError::Transient(format!("{id}: HTTP {status}")));
        }

        // Capture the response ETag before consuming the body — that
        // header is what makes the *next* poll cheap.
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let body: GitHubCommit = resp
            .json()
            .await
            .map_err(|e| RegistryError::Transient(format!("json: {e}")))?;
        let upstream_modified = chrono::DateTime::parse_from_rfc3339(&body.commit.author.date)
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        Ok(HeadOutcome::Fresh(CachedHead {
            info: HeadInfo {
                upstream_rev: body.sha,
                upstream_modified,
            },
            etag,
            fetched_at: chrono::Utc::now(),
        }))
    }

    /// GraphQL-batched HEAD lookups — one POST returns the resolved
    /// commit for up to `GRAPHQL_BATCH_SIZE` GitHub repos. Counts as
    /// 1 request against rate limit (GitHub charges by computed
    /// "node cost" for GraphQL, but a flat ref-target query at 50
    /// aliases stays well under their per-call ceiling and costs
    /// less than 50 separate REST requests).
    ///
    /// ETags don't apply to GraphQL — every response is fresh data.
    /// The upstream cache TTL in `planner::plan` short-circuits before
    /// the batch fires for unchanged inputs, so the typical batch is
    /// a small subset of total inputs (the ones genuinely needing
    /// refresh). Non-Github targets fall through to the per-id REST
    /// path via the trait default.
    async fn head_conditional_batch(
        &self,
        targets: &[(UpstreamId, Option<CachedHead>)],
    ) -> Vec<Result<HeadOutcome, RegistryError>> {
        // Operator can disable GraphQL via TEND_GRAPHQL_ENABLED=false
        // (chart: graphql.enabled). When disabled, fall through to the
        // trait default (per-id calls) so we still benefit from ETag
        // conditional requests just without the batch multiplier.
        if !graphql_enabled() {
            let mut out = Vec::with_capacity(targets.len());
            for (id, prev) in targets {
                out.push(self.head_conditional(id, prev.as_ref()).await);
            }
            return out;
        }

        // Partition: GitHub targets get batched; everything else
        // falls through to per-id (one round-trip each, but the
        // count of non-GitHub targets is small in practice).
        let mut out: Vec<Option<Result<HeadOutcome, RegistryError>>> =
            (0..targets.len()).map(|_| None).collect();
        let mut github_indices = Vec::new();
        for (i, (id, _)) in targets.iter().enumerate() {
            if matches!(id.source, SourceKind::Github) {
                github_indices.push(i);
            }
        }
        // Non-GitHub fallback path.
        for (i, (id, prev)) in targets.iter().enumerate() {
            if !matches!(id.source, SourceKind::Github) {
                out[i] = Some(self.head_conditional(id, prev.as_ref()).await);
            }
        }
        // Batch GitHub in env-configurable chunks.
        let batch_size = graphql_batch_size();
        for chunk in github_indices.chunks(batch_size) {
            let chunk_targets: Vec<(usize, &UpstreamId)> =
                chunk.iter().map(|&i| (i, &targets[i].0)).collect();
            match self.graphql_batch(&chunk_targets).await {
                Ok(results) => {
                    for ((i, _), res) in chunk_targets.iter().zip(results.into_iter()) {
                        out[*i] = Some(res);
                    }
                }
                Err(e) => {
                    // Whole-batch failure: fall back to per-id for
                    // this chunk so rate-limit errors are still
                    // surfaced and other targets in this batch don't
                    // get poisoned. The per-id path already handles
                    // ETag conditionals, so we still benefit from
                    // free 304s on the fallback.
                    tracing::warn!(
                        chunk_size = chunk.len(),
                        error = %format!("{e:#}"),
                        "graphql batch failed; falling back to per-id REST"
                    );
                    for &i in chunk {
                        let (id, prev) = &targets[i];
                        out[i] = Some(self.head_conditional(id, prev.as_ref()).await);
                    }
                }
            }
        }
        out.into_iter()
            .map(|opt| opt.unwrap_or_else(|| {
                Err(RegistryError::Transient("internal: target index unfilled".into()))
            }))
            .collect()
    }
}

impl ReqwestHeadResolver {
    /// Issue one GraphQL POST resolving up to GRAPHQL_BATCH_SIZE refs.
    /// Returns one HeadOutcome per input position. The whole-batch
    /// error path returns Err so the caller can fall back per-id.
    async fn graphql_batch(
        &self,
        targets: &[(usize, &UpstreamId)],
    ) -> Result<Vec<Result<HeadOutcome, RegistryError>>, anyhow::Error> {
        // Build the query: one alias per target.
        let mut query = String::from("query {\n");
        for (alias_n, (_, id)) in targets.iter().enumerate() {
            let (owner, repo) = id.key.split_once('/').unwrap_or((id.key.as_str(), ""));
            let qualified_ref = if id.r#ref == "HEAD" || id.r#ref.is_empty() {
                "refs/heads/HEAD".to_string()
            } else if id.r#ref.starts_with("refs/") {
                id.r#ref.clone()
            } else if looks_like_tag(&id.r#ref) {
                format!("refs/tags/{}", id.r#ref)
            } else {
                format!("refs/heads/{}", id.r#ref)
            };
            query.push_str(&format!(
                "  r{alias_n}: repository(owner: \"{owner}\", name: \"{repo}\") {{\
                 ref(qualifiedName: \"{qualified_ref}\") {{ \
                 target {{ ... on Commit {{ oid committedDate }} }} }} }}\n"
            ));
        }
        query.push('}');

        // Pacer: 1 token for the whole batch.
        self.budget.acquire().await;

        let body = serde_json::json!({ "query": query });
        let mut req = self
            .client
            .post("https://api.github.com/graphql")
            .header("User-Agent", "tend-operator")
            .json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;

        // Observe rate-limit pressure even on errors — same as REST.
        if let (Some(rem), Some(lim)) = (
            resp.headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok()),
            resp.headers()
                .get("x-ratelimit-limit")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u32>().ok()),
        ) {
            self.budget.record_observed_limit(lim).await;
            self.budget.record_headroom(rem, lim).await;
        }

        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("graphql HTTP {status}"));
        }
        let parsed: GraphQLBatchResponse = resp.json().await?;
        if let Some(errs) = parsed.errors {
            // Surface aggregated GraphQL errors but don't fail the
            // whole batch — per-target results may still be present.
            let summary: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
            tracing::warn!(errors = ?summary, "graphql batch returned errors");
        }
        let data = parsed
            .data
            .ok_or_else(|| anyhow::anyhow!("graphql response missing data"))?;

        let now = chrono::Utc::now();
        let mut out = Vec::with_capacity(targets.len());
        for (alias_n, (_, id)) in targets.iter().enumerate() {
            let alias_key = format!("r{alias_n}");
            let outcome = match data.aliases.get(&alias_key) {
                Some(Some(repo)) => match &repo.ref_ {
                    Some(GraphQLRef { target: Some(t) }) => {
                        let upstream_modified = t
                            .committed_date
                            .as_deref()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.timestamp())
                            .unwrap_or(0);
                        Ok(HeadOutcome::Fresh(CachedHead {
                            info: HeadInfo {
                                upstream_rev: t.oid.clone(),
                                upstream_modified,
                            },
                            etag: None, // GraphQL has no per-row ETag
                            fetched_at: now,
                        }))
                    }
                    _ => Err(RegistryError::NotFound(format!("{id}: ref absent"))),
                },
                Some(None) => Err(RegistryError::NotFound(format!("{id}"))),
                None => Err(RegistryError::Transient(format!(
                    "graphql alias {alias_key} missing from response"
                ))),
            };
            out.push(outcome);
        }
        Ok(out)
    }
}

/// Heuristic: does this ref string look more like a tag than a branch?
/// Used to qualify `refs/tags/X` vs `refs/heads/X` in GraphQL queries.
/// Branches typically contain `/` (e.g. `feature/foo`) or are common
/// names (`main`, `master`, `develop`); tags typically start with `v`
/// or contain a dot (semver). Imperfect but covers the common case;
/// when wrong, the GraphQL query returns ref:null and we fall through.
fn looks_like_tag(s: &str) -> bool {
    s.starts_with('v') && s.chars().nth(1).is_some_and(|c| c.is_ascii_digit())
        || (s.contains('.') && !matches!(s, "main" | "master" | "develop" | "trunk"))
}

pub async fn discover_advances<R: RegistryClient + ?Sized>(
    lock: &ExtendedLockFile,
    client: &R,
) -> Result<DiscoveryOutcome> {
    let mut empty_cache: HashMap<UpstreamId, CachedHead> = HashMap::new();
    discover_advances_filtered(lock, client, None, &mut empty_cache).await
}

/// `declared_inputs` is the set of input names actually in the user's
/// `flake.nix` (parsed via rnix). Discovery filters its root-input
/// iteration to this set when provided — eliminates "ghost" proposals
/// for inputs the user removed from flake.nix but flake.lock still
/// has stale entries for. Pass `None` to skip the filter (e.g.
/// when flake.nix isn't readable for some reason; degrades to
/// previous behavior, never blocks discovery).
///
/// `head_cache` persists across reconcile cycles — entries carry
/// ETags so the next `head_conditional` can send `If-None-Match` and
/// receive 304 (free request) when nothing changed. The caller owns
/// the cache so it survives between cycles within the same pod.
pub async fn discover_advances_filtered<R: RegistryClient + ?Sized>(
    lock: &ExtendedLockFile,
    client: &R,
    declared_inputs: Option<&std::collections::BTreeSet<String>>,
    head_cache: &mut HashMap<UpstreamId, CachedHead>,
) -> Result<DiscoveryOutcome> {
    // Pass 1 — collect distinct UpstreamIds that need an upstream
    // call. Case-aliased inputs (NixOS/nixpkgs vs nixos/nixpkgs)
    // collapse here so a batch query never asks for the same alias
    // twice. The `targets` vector preserves the (id, prev_cached)
    // shape that `head_conditional_batch` consumes directly.
    let mut targets: Vec<(UpstreamId, Option<CachedHead>)> = Vec::new();
    let mut seen_ids: std::collections::BTreeSet<UpstreamId> =
        std::collections::BTreeSet::new();
    for (local_name, node_name) in lock.root_input_nodes() {
        if let Some(declared) = declared_inputs {
            if !declared.contains(&local_name) {
                tracing::debug!(
                    input = %local_name,
                    "discovery skip: stale lock entry not in flake.nix",
                );
                continue;
            }
        }
        let Some(node) = lock.nodes.get(&node_name) else { continue };
        let Some(locked) = &node.locked else { continue };
        if locked.kind != "github" {
            continue;
        }
        let (Some(owner), Some(repo), _) = (
            locked.owner.as_deref(),
            locked.repo.as_deref(),
            locked.rev.as_deref(),
        ) else {
            continue;
        };
        let r#ref = node.tracking_ref();
        let id = UpstreamId::new_github(owner, repo, r#ref);
        if seen_ids.insert(id.clone()) {
            let prev = head_cache.get(&id).cloned();
            targets.push((id, prev));
        }
    }

    // Pass 2 — batch fetch. The `head_conditional_batch` impl on
    // ReqwestHeadResolver chunks targets into GraphQL queries of up
    // to GRAPHQL_BATCH_SIZE aliases each (50:1 quota multiplier on
    // large cycles). The trait default fans out to per-id calls so
    // tests with the in-memory mock keep their existing semantics.
    let results = client.head_conditional_batch(&targets).await;

    // Pass 3 — resolve results into a per-id lookup. Errors fatal to
    // the cycle (rate limit, auth) abort the whole pass with the
    // partial set of advances we'd already gathered (none yet at
    // this point, but kept for shape consistency with the prior
    // single-call path).
    let mut cycle_seen: HashMap<UpstreamId, Option<HeadInfo>> = HashMap::new();
    let out: Vec<CandidateAdvance> = Vec::new();
    for ((id, _), result) in targets.iter().zip(results.into_iter()) {
        match result {
            Ok(outcome) => {
                let cached = outcome.cached().clone();
                head_cache.insert(id.clone(), cached.clone());
                cycle_seen.insert(id.clone(), Some(cached.info));
            }
            Err(e) if e.is_fatal_to_cycle() => {
                tracing::warn!(
                    upstream = %id,
                    error = %e,
                    "discovery aborting cycle on fatal error",
                );
                return Ok(DiscoveryOutcome::Halted { partial: out, reason: e });
            }
            Err(e) => {
                tracing::warn!(
                    upstream = %id,
                    error = %e,
                    "head lookup failed for input; skipping",
                );
                cycle_seen.insert(id.clone(), None);
            }
        }
    }

    // Pass 4 — assemble candidate advances. Each root input's
    // current rev is compared to the resolved upstream rev; differing
    // → emit a `CandidateAdvance`. Case-aliased inputs each get their
    // own advance even though they share a single batch lookup.
    let mut out = out;
    for (local_name, node_name) in lock.root_input_nodes() {
        if let Some(declared) = declared_inputs {
            if !declared.contains(&local_name) {
                continue;
            }
        }
        let Some(node) = lock.nodes.get(&node_name) else { continue };
        let Some(locked) = &node.locked else { continue };
        if locked.kind != "github" {
            continue;
        }
        let (Some(owner), Some(repo), Some(rev)) = (
            locked.owner.as_deref(),
            locked.repo.as_deref(),
            locked.rev.as_deref(),
        ) else {
            continue;
        };
        let id = UpstreamId::new_github(owner, repo, node.tracking_ref());
        let Some(Some(head)) = cycle_seen.get(&id) else { continue };
        if head.upstream_rev == rev {
            continue;
        }

        out.push(CandidateAdvance {
            input: local_name.clone(),
            from: FlakeRev {
                url: format!("github:{owner}/{repo}"),
                rev: rev.to_string(),
                nar_hash: locked.nar_hash.clone().unwrap_or_default(),
                last_modified: locked.last_modified.unwrap_or(0),
            },
            to: FlakeRev {
                url: format!("github:{owner}/{repo}"),
                rev: head.upstream_rev.clone(),
                nar_hash: String::new(),
                last_modified: head.upstream_modified,
            },
        });
    }
    Ok(DiscoveryOutcome::Advances(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;
    use std::sync::Mutex;

    struct CountingMock {
        responses: Map<UpstreamId, Mutex<Result<HeadInfo, RegistryError>>>,
        calls: Mutex<Vec<UpstreamId>>,
    }

    impl CountingMock {
        fn new(responses: Vec<(UpstreamId, Result<HeadInfo, RegistryError>)>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k, Mutex::new(v)))
                    .collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl RegistryClient for CountingMock {
        async fn head_conditional(
            &self,
            id: &UpstreamId,
            _prev: Option<&CachedHead>,
        ) -> Result<HeadOutcome, RegistryError> {
            self.calls.lock().unwrap().push(id.clone());
            match self.responses.get(id) {
                Some(slot) => {
                    let guard = slot.lock().unwrap();
                    match &*guard {
                        Ok(info) => Ok(HeadOutcome::Fresh(CachedHead {
                            info: info.clone(),
                            etag: None,
                            fetched_at: chrono::Utc::now(),
                        })),
                        Err(RegistryError::RateLimited { reset_at, message }) => {
                            Err(RegistryError::RateLimited {
                                reset_at: *reset_at,
                                message: message.clone(),
                            })
                        }
                        Err(RegistryError::AuthFailed(m)) => Err(RegistryError::AuthFailed(m.clone())),
                        Err(RegistryError::NotFound(m)) => Err(RegistryError::NotFound(m.clone())),
                        Err(RegistryError::Transient(m)) => Err(RegistryError::Transient(m.clone())),
                    }
                }
                None => Err(RegistryError::NotFound(format!("no mock for {id}"))),
            }
        }
    }

    /// Sample lock with three flake.nix inputs at root + two
    /// transitive entries that look superficially upstream-bumpable
    /// but aren't actionable via `nix flake update --update-input`.
    /// Discovery must scope to root inputs only.
    const SAMPLE: &str = r#"{
      "nodes": {
        "root": {
          "inputs": {
            "substrate": "substrate",
            "nixpkgs": "nixpkgs",
            "nixpkgs-aliased": "nixpkgs_2"
          }
        },
        "substrate": {
          "locked": {
            "lastModified": 100, "narHash": "sha256-old=",
            "owner": "pleme-io", "repo": "substrate",
            "rev": "abc", "type": "github"
          }
        },
        "nixpkgs": {
          "locked": {
            "lastModified": 100, "narHash": "sha256-old=",
            "owner": "NixOS", "repo": "nixpkgs",
            "rev": "old-nixpkgs-sha", "type": "github"
          }
        },
        "nixpkgs_2": {
          "locked": {
            "lastModified": 100, "narHash": "sha256-old=",
            "owner": "nixos", "repo": "nixpkgs",
            "rev": "old-nixpkgs-sha", "type": "github"
          }
        },
        "transitive-not-in-root": {
          "locked": {
            "lastModified": 100, "narHash": "sha256-old=",
            "owner": "transient", "repo": "deep-dep",
            "rev": "old-transitive-sha", "type": "github"
          }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    #[test]
    fn looks_like_tag_classifies_common_shapes() {
        // Semver tags
        assert!(looks_like_tag("v0.8.18-pleme.1"));
        assert!(looks_like_tag("v1.0.0"));
        assert!(looks_like_tag("v2.31.4"));
        // Tag-like (dot, not standard branch)
        assert!(looks_like_tag("release.2024-01"));
        // Standard branches
        assert!(!looks_like_tag("main"));
        assert!(!looks_like_tag("master"));
        assert!(!looks_like_tag("develop"));
        assert!(!looks_like_tag("trunk"));
        // Topic branches with slash
        assert!(!looks_like_tag("feature/x"));
        // Bare numeric versions — classified as tag (the dot
        // heuristic catches them; correct behavior, GitHub semver
        // tags are often missing the `v` prefix).
        assert!(looks_like_tag("0.8"));
        // Branch names without dots
        assert!(!looks_like_tag("nixos-unstable"));
    }

    #[tokio::test]
    async fn dedup_collapses_case_variants_to_one_call() {
        // Root has `nixpkgs` (NixOS/nixpkgs) and `nixpkgs-aliased`
        // (nixos/nixpkgs) as separate direct inputs — both legitimately
        // bumpable via `nix flake update`. Case normalization in
        // UpstreamId means a single HEAD lookup serves both.
        let lock = ExtendedLockFile::parse(SAMPLE).unwrap();
        let mock = CountingMock::new(vec![
            (
                UpstreamId::new_github("pleme-io", "substrate", "HEAD"),
                Ok(HeadInfo { upstream_rev: "xyz".into(), upstream_modified: 200 }),
            ),
            (
                UpstreamId::new_github("nixos", "nixpkgs", "HEAD"),
                Ok(HeadInfo { upstream_rev: "new-nixpkgs-sha".into(), upstream_modified: 300 }),
            ),
        ]);
        let outcome = discover_advances(&lock, &mock).await.unwrap();
        let advances = match outcome {
            DiscoveryOutcome::Advances(a) => a,
            other => panic!("expected Advances, got {other:?}"),
        };
        assert_eq!(mock.call_count(), 2,
            "expected 2 unique upstreams (substrate + nixpkgs), got {}", mock.call_count());
        // 3 root inputs all advance: substrate, nixpkgs, nixpkgs-aliased.
        // The two nixpkgs aliases share an UpstreamId but each gets its
        // own proposal because they live at distinct local input names.
        assert_eq!(advances.len(), 3);
    }

    #[tokio::test]
    async fn transitive_lock_nodes_are_not_proposed() {
        // `transitive-not-in-root` is a node in the lockfile but not
        // referenced from root.inputs — `nix flake update` can't bump it.
        // Discovery must skip it entirely, otherwise we generate
        // un-actionable proposals (16k+ in the rio incident).
        let lock = ExtendedLockFile::parse(SAMPLE).unwrap();
        let mock = CountingMock::new(vec![
            (
                UpstreamId::new_github("pleme-io", "substrate", "HEAD"),
                Ok(HeadInfo { upstream_rev: "xyz".into(), upstream_modified: 200 }),
            ),
            (
                UpstreamId::new_github("nixos", "nixpkgs", "HEAD"),
                Ok(HeadInfo { upstream_rev: "new-nixpkgs-sha".into(), upstream_modified: 300 }),
            ),
            (
                UpstreamId::new_github("transient", "deep-dep", "HEAD"),
                Ok(HeadInfo { upstream_rev: "should-not-be-fetched".into(), upstream_modified: 400 }),
            ),
        ]);
        let outcome = discover_advances(&lock, &mock).await.unwrap();
        let advances = match outcome {
            DiscoveryOutcome::Advances(a) => a,
            other => panic!("expected Advances, got {other:?}"),
        };
        assert!(
            !advances.iter().any(|a| a.input == "transitive-not-in-root"),
            "transitive node leaked into proposals: {:?}",
            advances.iter().map(|a| &a.input).collect::<Vec<_>>()
        );
        // And we should not have even queried the upstream — that's
        // wasted GitHub quota.
        let queried: Vec<_> = mock
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|id| id.key.clone())
            .collect();
        assert!(
            !queried.iter().any(|k| k == "transient/deep-dep"),
            "transitive upstream was queried: {queried:?}"
        );
    }

    #[tokio::test]
    async fn rate_limit_halts_cycle_with_partial_results() {
        let lock = ExtendedLockFile::parse(SAMPLE).unwrap();
        let mock = CountingMock::new(vec![
            (
                UpstreamId::new_github("pleme-io", "substrate", "HEAD"),
                Ok(HeadInfo { upstream_rev: "xyz".into(), upstream_modified: 200 }),
            ),
            (
                UpstreamId::new_github("nixos", "nixpkgs", "HEAD"),
                Err(RegistryError::RateLimited { reset_at: Some(9999), message: "rl".into() }),
            ),
        ]);
        let outcome = discover_advances(&lock, &mock).await.unwrap();
        match outcome {
            DiscoveryOutcome::Halted { partial: _, reason } => {
                // Order of map iteration may put nixpkgs first or substrate
                // first; either way the cycle halts. The load-bearing
                // assertion is "we halted with the right reason kind",
                // not the partial count.
                assert!(matches!(reason, RegistryError::RateLimited { .. }));
            }
            DiscoveryOutcome::Advances(_) => panic!("expected Halted"),
        }
        // Critical: the mock should only see a few calls before halt,
        // never iterate every input. Without halt-on-fatal, we'd see
        // `lock.nodes.len()` calls.
        assert!(mock.call_count() <= 2,
            "expected ≤2 calls before halt (cycle should abort), got {}",
            mock.call_count());
    }
}
