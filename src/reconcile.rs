//! `tend reconcile` — typed scheduler-driven reconcile loop.
//!
//! Unlike the legacy batch `tend pull`, this command runs every
//! per-repo pull as an isolated `PullRepoJob` registered against a
//! real `shigoto::InProcessScheduler`. The scheduler advances each
//! Job through Pending → Ready → Running → Succeeded; per-Job typed
//! `PullOutcome` values stream into an `InMemorySink<PullOutcome>`;
//! the consumer reads back a typed `ReconcileReceipt` rather than
//! the legacy `PullSummary` integer counters.
//!
//! Why this shape:
//! - Per-repo Job grain — concurrency is bounded at the natural unit
//!   (one repo's pull is independent of another's; the daemon migration
//!   will install a workspace-level Budget here without a refactor).
//! - Typed outcomes — receipts carry the per-repo `PullOutcome`,
//!   keyed by `JobId`. Operators see *which* repo updated, dirty-
//!   skipped, or failed without grepping logs.
//! - Idempotent — re-running on a clean tree yields all `UpToDate`;
//!   the FSM idempotence proof (shigoto-test::idempotence_quickcheck)
//!   directly applies.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use shigoto_budget::{BudgetSpec, BudgetTree};
use shigoto_dag::Dag;
use shigoto_emit::{InMemorySink, NullEmitter};
use shigoto_scheduler::{InProcessScheduler, Scheduler};
use shigoto_types::{JobId, JobKindId, JobPhase, OutputSink};

use crate::config::Workspace;
use crate::jobs::pull_repo::{PullRepoJob, PULL_REPO_KIND};
use crate::sync::PullOutcome;

/// Default max parallel `tend.pull-repo` jobs per workspace. Chosen
/// well below typical OS limits (file handles, network sockets) so a
/// 1000-repo workspace doesn't exhaust resources, but high enough to
/// saturate a typical broadband link with concurrent `git pull`.
pub(crate) const DEFAULT_MAX_INFLIGHT_PULL: u32 = 16;

/// Typed receipt of one workspace's reconcile cycle. Replaces the
/// legacy `sync::PullSummary` for scheduler-driven paths — carries
/// per-`JobId` outcomes so callers can answer "what happened to
/// *this* repo?" without re-running.
#[derive(Debug, Clone)]
pub(crate) struct ReconcileReceipt {
    /// Workspace name this receipt covers.
    pub workspace: String,
    /// Per-Job typed outcomes captured from the InMemorySink.
    pub outcomes: HashMap<JobId, PullOutcome>,
    /// JobIds whose terminal phase was not Succeeded. Pairs with
    /// the scheduler's final snapshot — Failed / Deadlettered /
    /// Retrying jobs all surface here.
    pub failed_jobs: Vec<(JobId, JobPhase)>,
}

impl ReconcileReceipt {
    /// Project this typed receipt to the legacy `sync::PullSummary`
    /// shape so existing callers (audit log, display formatters) can
    /// consume it without a parallel surface. `Failed` Job-phase
    /// outcomes fold into the same `failed` bucket as
    /// `PullOutcome::Failed`, so the count reflects "anything that
    /// went wrong" rather than splitting between FSM-Failed and
    /// outcome-Failed.
    #[must_use]
    pub fn as_pull_summary(&self) -> crate::sync::PullSummary {
        let counts = self.outcome_counts();
        crate::sync::PullSummary {
            updated: counts.updated,
            up_to_date: counts.up_to_date,
            dirty_skipped: counts.dirty_skipped,
            missing_skipped: counts.missing_skipped,
            failed: counts.failed_pull + self.failed_jobs.len(),
        }
    }

    /// Aggregated counts by outcome variant. Useful for summary
    /// printing without iterating the per-Job map.
    #[must_use]
    pub fn outcome_counts(&self) -> OutcomeCounts {
        let mut c = OutcomeCounts::default();
        for outcome in self.outcomes.values() {
            match outcome {
                PullOutcome::Updated => c.updated += 1,
                PullOutcome::UpToDate => c.up_to_date += 1,
                PullOutcome::DirtySkipped => c.dirty_skipped += 1,
                PullOutcome::MissingSkipped => c.missing_skipped += 1,
                PullOutcome::Failed { .. } => c.failed_pull += 1,
            }
        }
        c
    }

    /// True iff every Job reached Succeeded AND every outcome was a
    /// non-Failed variant. This is "the workspace is fully reconciled"
    /// — both the FSM phase AND the typed outcome are clean.
    #[must_use]
    pub fn all_clean(&self) -> bool {
        self.failed_jobs.is_empty()
            && self
                .outcomes
                .values()
                .all(|o| !matches!(o, PullOutcome::Failed { .. }))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct OutcomeCounts {
    pub updated: usize,
    pub up_to_date: usize,
    pub dirty_skipped: usize,
    pub missing_skipped: usize,
    pub failed_pull: usize,
}

/// Reconcile one workspace by running a `PullRepoJob` for every
/// configured repo through `InProcessScheduler`. Returns a typed
/// receipt.
///
/// `max_inflight` bounds how many `git pull` processes can run
/// concurrently — wired into the scheduler as a per-kind BudgetSpec
/// against `tend.pull-repo`. The tick loop will only execute that
/// many Jobs simultaneously even if more are Ready. Pass
/// `DEFAULT_MAX_INFLIGHT_PULL` for the standard cap.
///
/// Quiescence: the loop calls `tick` until the receipt's
/// `transitions_this_tick` is empty (no Job advanced this cycle).
/// Capped at `max_ticks` to avoid pathological infinite loops if a
/// gate misconfiguration leaves Jobs Gated forever.
pub(crate) async fn reconcile_workspace_pull(
    workspace: &Workspace,
    repos: &[String],
    max_inflight: u32,
) -> Result<ReconcileReceipt> {
    const MAX_TICKS: usize = 64;

    let base_dir = workspace.resolved_base_dir()?;

    // Walk the on-disk tree the same way pull_repos does so we
    // reconcile "every repo in the workspace dir," not just configured
    // ones. This matches the legacy `tend pull` behavior.
    let mut all: Vec<String> = repos.to_vec();
    if base_dir.exists() {
        let on_disk = std::fs::read_dir(&base_dir)
            .with_context(|| format!("reading {}", base_dir.display()))?;
        for entry in on_disk.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if !all.contains(&name) {
                all.push(name);
            }
        }
    }
    all.sort();
    all.dedup();

    let sink: Arc<InMemorySink<PullOutcome>> = Arc::new(InMemorySink::new());
    let sink_for_jobs: Arc<dyn OutputSink<PullOutcome>> = sink.clone();

    let scheduler =
        InProcessScheduler::new(&workspace.name).with_emitter(Arc::new(NullEmitter));

    // Cap concurrent `git pull` processes via the scheduler's
    // per-kind budget. Without this, a workspace with hundreds of
    // repos would launch hundreds of git processes simultaneously,
    // exhausting file handles + network sockets.
    let mut budget = BudgetTree::new();
    budget
        .by_kind
        .insert(JobKindId::new(PULL_REPO_KIND), BudgetSpec::max_concurrent(max_inflight));
    scheduler.install_budget(budget).await;

    let mut dag = Dag::new();

    let mut all_ids: Vec<JobId> = Vec::with_capacity(all.len());
    for repo_name in &all {
        let job = Arc::new(
            PullRepoJob::new(&workspace.name, repo_name, base_dir.join(repo_name))
                .with_output_sink(sink_for_jobs.clone()),
        );
        let id = <PullRepoJob as shigoto_types::Job>::id(&job);
        all_ids.push(id.clone());
        dag.ensure_node(id);
        scheduler.register_job(job).await;
    }

    for _ in 0..MAX_TICKS {
        let receipt = scheduler.tick(&mut dag).await?;
        if receipt.transitions_this_tick.is_empty() {
            break;
        }
    }

    let snap = scheduler.snapshot(&dag).await;
    let failed_jobs: Vec<(JobId, JobPhase)> = all_ids
        .into_iter()
        .filter_map(|id| match snap.phases.get(&id) {
            Some(JobPhase::Succeeded) => None,
            Some(other) => Some((id, other.clone())),
            None => None,
        })
        .collect();

    Ok(ReconcileReceipt {
        workspace: workspace.name.clone(),
        outcomes: sink.drain(),
        failed_jobs,
    })
}

/// Pretty-print a receipt for the operator. Matches the existing
/// `display::print_pull_summary` shape so the two commands feel
/// consistent.
pub(crate) fn print_receipt(receipt: &ReconcileReceipt) {
    let counts = receipt.outcome_counts();
    println!(
        "[{}] reconcile: {} updated, {} up-to-date, {} dirty (skipped), {} missing, {} failed{}",
        receipt.workspace,
        counts.updated,
        counts.up_to_date,
        counts.dirty_skipped,
        counts.missing_skipped,
        counts.failed_pull,
        if receipt.failed_jobs.is_empty() {
            String::new()
        } else {
            format!(" ({} job(s) didn't reach Succeeded)", receipt.failed_jobs.len())
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Workspace;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo(path: &std::path::Path) {
        Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.email", "t@t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "user.name", "t"]).current_dir(path).status().unwrap();
        Command::new("git").args(["config", "commit.gpgsign", "false"]).current_dir(path).status().unwrap();
        std::fs::write(path.join("file"), "x\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(path).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "init"]).current_dir(path).status().unwrap();
    }

    fn clone_from(upstream: &std::path::Path, dest: &std::path::Path) {
        Command::new("git")
            .args(["clone", "-q", upstream.to_str().unwrap(), dest.to_str().unwrap()])
            .status()
            .unwrap();
    }

    fn workspace_at(tmp: &TempDir, name: &str) -> Workspace {
        let mut ws = Workspace::test_default(name);
        ws.base_dir = tmp.path().to_string_lossy().to_string();
        ws
    }

    #[tokio::test]
    async fn empty_workspace_yields_empty_receipt() {
        let tmp = TempDir::new().unwrap();
        let ws = workspace_at(&tmp, "test");
        let receipt = reconcile_workspace_pull(&ws, &[], DEFAULT_MAX_INFLIGHT_PULL).await.unwrap();
        assert_eq!(receipt.workspace, "test");
        assert!(receipt.outcomes.is_empty());
        assert!(receipt.failed_jobs.is_empty());
        assert!(receipt.all_clean());
        assert_eq!(receipt.outcome_counts().updated, 0);
    }

    #[tokio::test]
    async fn workspace_with_mixed_repos_produces_typed_receipt() {
        // Order matters here — we want three repos in three distinct
        // states relative to upstream:
        //   "behind"  — cloned at commit A, upstream then advanced to B → Updated
        //   "current" — cloned at commit B (same as upstream HEAD)      → UpToDate
        //   "dirty"   — cloned at any commit + working tree dirtied     → DirtySkipped
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join(".upstream");
        std::fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);

        // Clone "behind" first while upstream is still at commit A.
        clone_from(&upstream, &tmp.path().join("behind"));

        // Advance upstream to commit B.
        std::fs::write(upstream.join("file"), "two\n").unwrap();
        Command::new("git").args(["add", "."]).current_dir(&upstream).status().unwrap();
        Command::new("git").args(["commit", "-q", "-m", "two"]).current_dir(&upstream).status().unwrap();

        // Now clone "current" + "dirty" — they're at the same B commit
        // as upstream, so "current" pulls UpToDate.
        clone_from(&upstream, &tmp.path().join("current"));
        clone_from(&upstream, &tmp.path().join("dirty"));

        // Make "dirty" actually dirty.
        std::fs::write(tmp.path().join("dirty").join("dirt"), "dirt\n").unwrap();

        let ws = workspace_at(&tmp, "test-ws");
        let receipt = reconcile_workspace_pull(
            &ws,
            &["behind".into(), "current".into(), "dirty".into()],
            DEFAULT_MAX_INFLIGHT_PULL,
        )
        .await
        .unwrap();

        let counts = receipt.outcome_counts();
        assert_eq!(counts.updated, 1, "behind should be Updated");
        assert_eq!(counts.up_to_date, 1, "current should be UpToDate");
        assert_eq!(counts.dirty_skipped, 1, "dirty should be DirtySkipped");
        assert!(receipt.failed_jobs.is_empty(), "no Jobs should fail the FSM");
        assert!(receipt.all_clean(), "no Failed outcomes, no failed jobs");
    }

    /// Budget enforcement proof: spin up many repos, set
    /// max_inflight=2, time the reconcile. Since pull is O(milliseconds)
    /// in the local-file://-upstream test setup, this is mostly a
    /// correctness check — the reconcile must complete + produce the
    /// expected receipt with the budget cap installed (and not deadlock).
    /// A deeper concurrency-vs-budget test lives in shigoto-scheduler.
    #[tokio::test]
    async fn budget_capped_reconcile_still_completes_all_repos() {
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join(".upstream");
        std::fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);

        let n_repos = 8;
        let mut names = Vec::with_capacity(n_repos);
        for i in 0..n_repos {
            let name = format!("repo{i}");
            clone_from(&upstream, &tmp.path().join(&name));
            names.push(name);
        }

        let ws = workspace_at(&tmp, "budget-test-ws");
        // max_inflight=2 means at most 2 pull jobs can be Running
        // concurrently. With 8 repos that's 4 waves of execution
        // (within the same dag wave, but serialized by budget).
        let receipt = reconcile_workspace_pull(&ws, &names, 2).await.unwrap();
        let counts = receipt.outcome_counts();
        // All 8 cloned-then-not-advanced repos report UpToDate.
        assert_eq!(counts.up_to_date, n_repos);
        assert!(receipt.failed_jobs.is_empty());
        assert!(receipt.all_clean());
    }

    #[tokio::test]
    async fn ondisk_repos_not_in_config_are_still_reconciled() {
        // Match the legacy tend pull behavior: any directory under
        // base_dir (with .git) gets pulled even if it's not in the
        // resolved repo list.
        let tmp = TempDir::new().unwrap();
        let upstream = tmp.path().join(".upstream");
        std::fs::create_dir(&upstream).unwrap();
        init_repo(&upstream);

        clone_from(&upstream, &tmp.path().join("not-in-config"));

        let ws = workspace_at(&tmp, "test-ws");
        let receipt = reconcile_workspace_pull(&ws, &[], DEFAULT_MAX_INFLIGHT_PULL).await.unwrap();
        // Empty config but on-disk repo present → reconciled.
        assert_eq!(receipt.outcomes.len(), 1);
        assert_eq!(receipt.outcome_counts().up_to_date, 1);
    }
}
