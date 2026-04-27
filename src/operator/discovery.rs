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

use super::crds::FlakeRev;
use super::upstream::{HeadInfo, RegistryClient, RegistryError, SourceKind, UpstreamId};
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

pub struct ReqwestHeadResolver {
    pub client: reqwest::Client,
    pub token: Option<String>,
}

#[async_trait::async_trait]
impl RegistryClient for ReqwestHeadResolver {
    async fn head(&self, id: &UpstreamId) -> Result<HeadInfo, RegistryError> {
        if !matches!(id.source, SourceKind::Github) {
            return Err(RegistryError::Transient(format!(
                "ReqwestHeadResolver doesn't service {:?} sources",
                id.source
            )));
        }
        let url = format!("https://api.github.com/repos/{}/commits/{}", id.key, id.r#ref);
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tend-operator");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| RegistryError::Transient(format!("send: {e}")))?;
        let status = resp.status();
        let reset_at = resp
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok());

        if status.as_u16() == 403 {
            let remaining_zero = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .map(|v| v == "0")
                .unwrap_or(false);
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
        let body: GitHubCommit = resp
            .json()
            .await
            .map_err(|e| RegistryError::Transient(format!("json: {e}")))?;
        let upstream_modified = chrono::DateTime::parse_from_rfc3339(&body.commit.author.date)
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        Ok(HeadInfo {
            upstream_rev: body.sha,
            upstream_modified,
        })
    }
}

pub async fn discover_advances<R: RegistryClient + ?Sized>(
    lock: &ExtendedLockFile,
    client: &R,
) -> Result<DiscoveryOutcome> {
    let mut head_cache: HashMap<UpstreamId, Option<HeadInfo>> = HashMap::new();
    let mut out = Vec::new();
    for (name, node) in &lock.nodes {
        if name == &lock.root {
            continue;
        }
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
        let r#ref = locked.r#ref.as_deref().unwrap_or("HEAD");
        let id = UpstreamId::new_github(owner, repo, r#ref);

        let head = if let Some(cached) = head_cache.get(&id) {
            cached.clone()
        } else {
            match client.head(&id).await {
                Ok(info) => {
                    head_cache.insert(id.clone(), Some(info.clone()));
                    Some(info)
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
                        input = %name,
                        error = %e,
                        "head lookup failed for input; skipping",
                    );
                    head_cache.insert(id.clone(), None);
                    None
                }
            }
        };
        let Some(head) = head else { continue };
        if head.upstream_rev == rev {
            continue;
        }

        out.push(CandidateAdvance {
            input: name.clone(),
            from: FlakeRev {
                url: format!("github:{owner}/{repo}"),
                rev: rev.to_string(),
                nar_hash: locked.nar_hash.clone().unwrap_or_default(),
                last_modified: locked.last_modified.unwrap_or(0),
            },
            to: FlakeRev {
                url: format!("github:{owner}/{repo}"),
                rev: head.upstream_rev,
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
        async fn head(&self, id: &UpstreamId) -> Result<HeadInfo, RegistryError> {
            self.calls.lock().unwrap().push(id.clone());
            match self.responses.get(id) {
                Some(slot) => {
                    let guard = slot.lock().unwrap();
                    match &*guard {
                        Ok(info) => Ok(info.clone()),
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

    const SAMPLE: &str = r#"{
      "nodes": {
        "root": { "inputs": { "substrate": "substrate", "nixpkgs": "nixpkgs" } },
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
        "nixpkgs_3": {
          "locked": {
            "lastModified": 100, "narHash": "sha256-old=",
            "owner": "nixos", "repo": "nixpkgs",
            "rev": "old-nixpkgs-sha", "type": "github"
          }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    #[tokio::test]
    async fn dedup_collapses_case_variants_to_one_call() {
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
            "expected 2 unique upstreams, got {}", mock.call_count());
        assert_eq!(advances.len(), 4);
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
