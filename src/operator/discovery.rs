//! Discovery — detect upstream advances against current pins.
//!
//! For each (repo, input) tuple in a `FlakeUpdatePolicy`, query
//! GitHub for the current HEAD of the input's `original` ref. If the
//! HEAD SHA differs from the locked rev, emit a candidate advance.
//!
//! Phase 1 only handles `type: "github"` inputs. Other input types
//! (git+, tarball, path) are skipped — the lock format adapter
//! preserves their pins on write so they don't drift.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

use super::crds::FlakeRev;
use crate::flake_lock::ExtendedLockFile;

/// One detected advance: pin `from` is currently in flake.lock,
/// upstream HEAD has moved to `to`.
#[derive(Debug, Clone)]
pub struct CandidateAdvance {
    /// Flake input name (matches `flake.lock` node name).
    pub input: String,
    pub from: FlakeRev,
    pub to: FlakeRev,
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
    /// ISO-8601 commit date — convert to unix epoch for `lastModified`.
    date: String,
}

/// Trait so we can mock GitHub in tests without spinning up reqwest.
#[async_trait::async_trait]
pub trait HeadResolver: Send + Sync {
    async fn head_sha(&self, owner: &str, repo: &str, r#ref: &str) -> Result<(String, i64)>;
}

pub struct ReqwestHeadResolver {
    pub client: reqwest::Client,
    pub token: Option<String>,
}

#[async_trait::async_trait]
impl HeadResolver for ReqwestHeadResolver {
    async fn head_sha(&self, owner: &str, repo: &str, r#ref: &str) -> Result<(String, i64)> {
        let url = format!("https://api.github.com/repos/{owner}/{repo}/commits/{ref}");
        let mut req = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "tend-operator");
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "github HEAD lookup {owner}/{repo}@{ref} failed: HTTP {}",
                resp.status()
            ));
        }
        let body: GitHubCommit = resp.json().await?;
        let last_modified = chrono::DateTime::parse_from_rfc3339(&body.commit.author.date)
            .map(|dt| dt.timestamp())
            .unwrap_or(0);
        Ok((body.sha, last_modified))
    }
}

/// Walk a parsed extended flake.lock, ask `resolver` for HEAD of each
/// GitHub input, and emit a `CandidateAdvance` for every input whose
/// current rev differs from upstream HEAD.
pub async fn discover_advances<R: HeadResolver + ?Sized>(
    lock: &ExtendedLockFile,
    resolver: &R,
) -> Result<Vec<CandidateAdvance>> {
    let mut out = Vec::new();
    for (name, node) in &lock.nodes {
        if name == &lock.root {
            continue;
        }
        let Some(locked) = &node.locked else { continue };
        if locked.kind != "github" {
            continue;
        }
        let (Some(owner), Some(repo), Some(rev)) =
            (locked.owner.as_deref(), locked.repo.as_deref(), locked.rev.as_deref())
        else {
            continue;
        };
        let r#ref = locked.r#ref.as_deref().unwrap_or("HEAD");

        let (head_sha, head_time) = match resolver.head_sha(owner, repo, r#ref).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(input = %name, error = %e, "head lookup failed; skipping");
                continue;
            }
        };
        if head_sha == rev {
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
                rev: head_sha,
                // narHash isn't returned by the commits API — left empty
                // and resolved by `nix flake lock --update-input` at apply time.
                nar_hash: String::new(),
                last_modified: head_time,
            },
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticResolver(BTreeMap<String, (String, i64)>);

    #[async_trait::async_trait]
    impl HeadResolver for StaticResolver {
        async fn head_sha(&self, owner: &str, repo: &str, _r: &str) -> Result<(String, i64)> {
            self.0
                .get(&format!("{owner}/{repo}"))
                .cloned()
                .ok_or_else(|| anyhow!("no mock for {owner}/{repo}"))
        }
    }

    const SAMPLE: &str = r#"{
      "nodes": {
        "root": { "inputs": { "substrate": "substrate" } },
        "substrate": {
          "locked": {
            "lastModified": 100,
            "narHash": "sha256-old=",
            "owner": "pleme-io",
            "repo": "substrate",
            "rev": "abc",
            "type": "github"
          }
        },
        "tarball-thing": {
          "locked": {
            "type": "tarball",
            "url": "https://example.com/x.tgz",
            "narHash": "sha256-tar="
          }
        }
      },
      "root": "root",
      "version": 7
    }"#;

    #[tokio::test]
    async fn detects_advance_when_head_differs() {
        let lock = ExtendedLockFile::parse(SAMPLE).unwrap();
        let mock = StaticResolver([
            ("pleme-io/substrate".into(), ("xyz".into(), 200)),
        ].into_iter().collect());
        let advances = discover_advances(&lock, &mock).await.unwrap();
        assert_eq!(advances.len(), 1);
        assert_eq!(advances[0].input, "substrate");
        assert_eq!(advances[0].from.rev, "abc");
        assert_eq!(advances[0].to.rev, "xyz");
        assert_eq!(advances[0].to.last_modified, 200);
    }

    #[tokio::test]
    async fn skips_when_head_matches_pin() {
        let lock = ExtendedLockFile::parse(SAMPLE).unwrap();
        let mock = StaticResolver([
            ("pleme-io/substrate".into(), ("abc".into(), 100)),
        ].into_iter().collect());
        let advances = discover_advances(&lock, &mock).await.unwrap();
        assert!(advances.is_empty());
    }

    #[tokio::test]
    async fn skips_non_github_inputs() {
        let lock = ExtendedLockFile::parse(SAMPLE).unwrap();
        let mock = StaticResolver(BTreeMap::new());
        let advances = discover_advances(&lock, &mock).await.unwrap();
        // tarball-thing is type=tarball; substrate fails head lookup
        // with empty mock — both yield zero advances without panicking.
        assert!(advances.is_empty());
    }
}
