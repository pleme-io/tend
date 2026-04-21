//! CI hygiene — the little tree trimmer for GitHub Actions queues.
//!
//! tend is tending to orgs and repos; trimming piled-up queued workflow
//! runs on the same tag/branch is part of that tending. This module
//! contains the PURE policy (no I/O, no API calls) — the daemon loop +
//! trait-backed API client compose on top of it.
//!
//! ## The policy
//!
//! GitHub-hosted runners are free for public repos but have concurrency
//! limits. When you `git push --tags` and the workflow fails (so you
//! fix + re-tag + re-push), three runs can end up queued on the same
//! tag — only the newest one actually has your fix. The older ones
//! occupy queue slots, sometimes indefinitely.
//!
//! The trim policy:
//! - For each (workflow_id, head_branch) bucket, if there are ≥2 queued
//!   runs, keep the NEWEST and mark older ones to cancel.
//! - Never touch runs in any status other than `queued`.
//! - Never touch runs whose head_sha matches a currently-running
//!   (`in_progress`) run on the same workflow (safety — in case the
//!   caller triggered a manual re-run from the UI that's actively
//!   making progress).

use serde::{Deserialize, Serialize};

/// A single workflow run identity — the minimal shape the trim policy
/// needs. Richer GitHub API response objects decompose into this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub workflow_id: u64,
    pub head_branch: String,
    pub head_sha: String,
    pub status: RunStatus,
    /// ISO-8601 UTC timestamp. Lexicographic sort matches chronological
    /// order for this format; we don't need full DateTime parsing.
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    InProgress,
    Completed,
    Cancelled,
    /// Any other string GitHub emits — treated as a no-op for trim.
    Unknown,
}

/// Identifies workflow runs to cancel. Returns run IDs in stable order
/// (ascending by id) so the caller and tests can assert easily.
///
/// Pure function. No I/O. Deterministic.
pub fn identify_duplicates_to_cancel(runs: &[WorkflowRun]) -> Vec<u64> {
    use std::collections::BTreeMap;

    // Bucket queued runs by (workflow_id, head_branch).
    let mut queued_buckets: BTreeMap<(u64, String), Vec<&WorkflowRun>> = BTreeMap::new();
    for run in runs {
        if run.status == RunStatus::Queued {
            queued_buckets
                .entry((run.workflow_id, run.head_branch.clone()))
                .or_default()
                .push(run);
        }
    }

    // For each bucket with ≥2 queued runs, cancel all but the newest.
    let mut to_cancel: Vec<u64> = Vec::new();
    for (_key, mut bucket) in queued_buckets {
        if bucket.len() < 2 {
            continue;
        }
        // Sort newest-first by created_at (ISO-8601 lex order).
        bucket.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        // Keep the first (newest); cancel the rest.
        for older in bucket.into_iter().skip(1) {
            // Safety check: don't cancel if an in_progress run on the
            // same head_sha exists (manual re-run from UI mid-flight).
            let has_active_on_sha = runs.iter().any(|r| {
                r.status == RunStatus::InProgress && r.head_sha == older.head_sha
            });
            if !has_active_on_sha {
                to_cancel.push(older.id);
            }
        }
    }

    to_cancel.sort_unstable();
    to_cancel
}

/// Summary the caller can log + emit to the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrimReport {
    pub org: String,
    pub repo: String,
    pub cancelled_ids: Vec<u64>,
    pub queued_kept: Vec<u64>,
    pub total_queued_before: usize,
}

impl TrimReport {
    /// Build a report from a policy pass over a repo's runs.
    pub fn from_runs(org: impl Into<String>, repo: impl Into<String>, runs: &[WorkflowRun]) -> Self {
        let cancelled = identify_duplicates_to_cancel(runs);
        let total_queued: Vec<u64> = runs
            .iter()
            .filter(|r| r.status == RunStatus::Queued)
            .map(|r| r.id)
            .collect();
        let kept: Vec<u64> = total_queued
            .iter()
            .copied()
            .filter(|id| !cancelled.contains(id))
            .collect();
        Self {
            org: org.into(),
            repo: repo.into(),
            total_queued_before: total_queued.len(),
            cancelled_ids: cancelled,
            queued_kept: kept,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: u64, workflow: u64, branch: &str, sha: &str, status: RunStatus, created: &str) -> WorkflowRun {
        WorkflowRun {
            id,
            workflow_id: workflow,
            head_branch: branch.to_string(),
            head_sha: sha.to_string(),
            status,
            created_at: created.to_string(),
        }
    }

    #[test]
    fn no_duplicates_returns_empty() {
        let runs = vec![
            run(1, 10, "main", "sha1", RunStatus::Queued, "2026-04-20T10:00:00Z"),
            run(2, 10, "feat", "sha2", RunStatus::Queued, "2026-04-20T10:00:00Z"),
        ];
        assert!(identify_duplicates_to_cancel(&runs).is_empty());
    }

    #[test]
    fn three_queued_on_same_tag_keeps_newest_cancels_older_two() {
        // Modeled on the real pleme-io/dq v0.1.0 situation.
        let runs = vec![
            run(24690390497, 42, "v0.1.0", "a6b5dd9", RunStatus::Queued, "2026-04-20T21:05:58Z"),
            run(24690847968, 42, "v0.1.0", "2e3b6c3", RunStatus::Queued, "2026-04-20T21:17:02Z"),
            run(24695026106, 42, "v0.1.0", "6161f03", RunStatus::Queued, "2026-04-20T23:09:23Z"),
        ];
        let to_cancel = identify_duplicates_to_cancel(&runs);
        assert_eq!(to_cancel, vec![24690390497, 24690847968]);
    }

    #[test]
    fn in_progress_on_same_sha_blocks_cancellation() {
        let runs = vec![
            run(1, 42, "v0.1.0", "sha1", RunStatus::Queued, "2026-04-20T10:00:00Z"),
            run(2, 42, "v0.1.0", "sha1", RunStatus::InProgress, "2026-04-20T10:30:00Z"),
            run(3, 42, "v0.1.0", "sha2", RunStatus::Queued, "2026-04-20T11:00:00Z"),
        ];
        // Would normally cancel run 1 (older queued on v0.1.0), but
        // run 2 is InProgress on sha1 — so we DON'T cancel run 1.
        assert!(identify_duplicates_to_cancel(&runs).is_empty());
    }

    #[test]
    fn different_workflows_on_same_branch_independent() {
        let runs = vec![
            run(1, 10, "main", "sha", RunStatus::Queued, "2026-04-20T10:00:00Z"),
            run(2, 10, "main", "sha", RunStatus::Queued, "2026-04-20T10:05:00Z"),
            run(3, 20, "main", "sha", RunStatus::Queued, "2026-04-20T10:00:00Z"),
            run(4, 20, "main", "sha", RunStatus::Queued, "2026-04-20T10:05:00Z"),
        ];
        let to_cancel = identify_duplicates_to_cancel(&runs);
        // Cancels older run in each workflow's bucket independently.
        assert_eq!(to_cancel, vec![1, 3]);
    }

    #[test]
    fn completed_and_cancelled_runs_ignored() {
        let runs = vec![
            run(1, 42, "main", "sha1", RunStatus::Completed, "2026-04-20T10:00:00Z"),
            run(2, 42, "main", "sha2", RunStatus::Cancelled, "2026-04-20T10:05:00Z"),
            run(3, 42, "main", "sha3", RunStatus::Queued, "2026-04-20T10:10:00Z"),
        ];
        assert!(identify_duplicates_to_cancel(&runs).is_empty());
    }

    #[test]
    fn result_is_deterministic_sorted() {
        // Same input with different insertion orders should produce the
        // same output. Guarantees audit-log reproducibility.
        let a = vec![
            run(5, 1, "b", "s", RunStatus::Queued, "2026-04-20T10:00:00Z"),
            run(3, 1, "b", "s", RunStatus::Queued, "2026-04-20T09:00:00Z"),
            run(4, 1, "b", "s", RunStatus::Queued, "2026-04-20T11:00:00Z"),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(identify_duplicates_to_cancel(&a), identify_duplicates_to_cancel(&b));
    }

    #[test]
    fn trim_report_captures_counts() {
        let runs = vec![
            run(1, 42, "v1", "s1", RunStatus::Queued, "2026-04-20T10:00:00Z"),
            run(2, 42, "v1", "s2", RunStatus::Queued, "2026-04-20T10:05:00Z"),
            run(3, 42, "v1", "s3", RunStatus::Queued, "2026-04-20T10:10:00Z"),
        ];
        let r = TrimReport::from_runs("pleme-io", "dq", &runs);
        assert_eq!(r.org, "pleme-io");
        assert_eq!(r.repo, "dq");
        assert_eq!(r.total_queued_before, 3);
        assert_eq!(r.cancelled_ids, vec![1, 2]);
        assert_eq!(r.queued_kept, vec![3]);
    }
}
