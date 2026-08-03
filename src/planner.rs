//! DAG planner for the tend daemon cycle.
//!
//! Every daemon tick:
//! 1. **Collect** — each concern (sync / pull / watch / flake_update /
//!    nix_audit / ci_trim / release_swarm) reports its candidate work
//!    via `has_work()`. Cheap, pure, no API calls.
//! 2. **Plan** — build a DAG from the candidate items, honoring
//!    cross-concern dependencies. Topologically sort into stages that
//!    can run in parallel inside each stage, sequential across stages.
//! 3. **Decide** — inspect the plan; if `is_empty()`, daemon tick
//!    returns silently (no log output at default level). Otherwise
//!    log the plan at INFO.
//! 4. **Execute** — walk stages in order, dispatch per-`WorkKind`.
//!
//! This module is the **pure planning layer** — no I/O, no async, no
//! config-loading. The daemon owns collect (because it knows state)
//! and execute (because it owns handles to the various executors);
//! this module owns the DAG math.
//!
//! Blackmatter Pillar 6 (Typescape) + Pillar 10 (proof via pure
//! function tests) + Pillar 12 (generation: the plan is derived from
//! the config + cache state, never hand-rolled).

use serde::{Deserialize, Serialize};

/// Every distinct kind of work the tend daemon can do.
///
/// Adding a new concern to tend = adding a variant here + wiring its
/// `has_work` probe into `collect_work` + its executor into the
/// daemon's dispatch table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// Clone repos that exist in the config / GitHub org but not on
    /// disk. Precedes everything that reads local repos.
    Sync,
    /// `git fetch` + fast-forward `git pull --ff-only` on clean repos.
    /// Precedes Watch (so watch sees the latest tags).
    Pull,
    /// Watch cycle — detect new tags or HEAD commits, append matrix
    /// entries, run certify + commit + propagate.
    Watch,
    /// `tend flake-update` — propagate nix flake updates through the
    /// dependency chain. Follows Watch when auto_propagate is set.
    FlakeUpdate,
    /// Nix-audit convergence loop — check/fix/propagate lints.
    NixAudit,
    /// Cancel duplicate queued GitHub Actions runs (the tree trimmer).
    /// Independent of other concerns; pure GitHub API.
    CiTrim,
    /// Dry-run the release-swarm (build the plan, don't mutate).
    ReleaseSwarmPlan,
    /// Apply the release-swarm (open PRs). Depends on
    /// ReleaseSwarmPlan for the same workspace so plan + apply see a
    /// consistent view of org state.
    ReleaseSwarmApply,
}

/// A single unit of work. The `id` is implicit: `(kind, workspace,
/// scope)` triple uniquely identifies any WorkItem in one tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub kind: WorkKind,
    pub workspace: String,
    /// Optional per-repo scope (for kinds like Pull/Watch that operate
    /// per-repo). `None` means workspace-scoped.
    pub scope: Option<String>,
}

impl WorkItem {
    #[must_use]
    pub fn workspace(kind: WorkKind, workspace: impl Into<String>) -> Self {
        Self {
            kind,
            workspace: workspace.into(),
            scope: None,
        }
    }

    #[must_use]
    pub fn per_repo(kind: WorkKind, workspace: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            kind,
            workspace: workspace.into(),
            scope: Some(repo.into()),
        }
    }
}

/// A full execution plan: one or more stages, each stage a bag of
/// items that run in parallel, stages run sequentially.
///
/// Empty plan = nothing to do. Daemon should return silently on
/// empty plans (quiet discipline).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub stages: Vec<Vec<WorkItem>>,
}

impl ExecutionPlan {
    pub fn is_empty(&self) -> bool {
        self.stages.iter().all(|s| s.is_empty())
    }

    pub fn total_items(&self) -> usize {
        self.stages.iter().map(|s| s.len()).sum()
    }

    /// Human-readable one-line summary: `"3 items across 2 stages: stage[0]=2 sync + 1 ci_trim; stage[1]=..."`
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "no work".to_string();
        }
        let mut out = format!(
            "{} items across {} stages",
            self.total_items(),
            self.stages.len()
        );
        for (i, stage) in self.stages.iter().enumerate() {
            if stage.is_empty() {
                continue;
            }
            let mut kinds: std::collections::BTreeMap<WorkKind, usize> =
                std::collections::BTreeMap::new();
            for item in stage {
                *kinds.entry(item.kind).or_insert(0) += 1;
            }
            let parts: Vec<String> = kinds
                .iter()
                .map(|(k, n)| format!("{n} {}", kind_label(*k)))
                .collect();
            out.push_str(&format!("; stage[{i}]={}", parts.join(" + ")));
        }
        out
    }
}

fn kind_label(k: WorkKind) -> &'static str {
    match k {
        WorkKind::Sync => "sync",
        WorkKind::Pull => "pull",
        WorkKind::Watch => "watch",
        WorkKind::FlakeUpdate => "flake_update",
        WorkKind::NixAudit => "nix_audit",
        WorkKind::CiTrim => "ci_trim",
        WorkKind::ReleaseSwarmPlan => "release_swarm_plan",
        WorkKind::ReleaseSwarmApply => "release_swarm_apply",
    }
}

/// Per-kind stage assignment. Kinds in the same stage run in parallel;
/// lower stage numbers run first.
///
/// Canonical ordering (workspace-scoped chain):
/// - Stage 0: Sync (nothing can touch repos before they exist)
/// - Stage 1: Pull + NixAudit + CiTrim + ReleaseSwarmPlan (independent
///            reads/writes — pull is per-repo-git, nix-audit is local
///            evaluation, ci-trim + release-swarm-plan are pure GH API
///            reads)
/// - Stage 2: Watch (needs latest tags from Pull)
/// - Stage 3: FlakeUpdate + ReleaseSwarmApply (depend on Stage 2 /
///            plan outputs respectively)
fn stage_for(kind: WorkKind) -> usize {
    match kind {
        WorkKind::Sync => 0,
        WorkKind::Pull => 1,
        WorkKind::NixAudit => 1,
        WorkKind::CiTrim => 1,
        WorkKind::ReleaseSwarmPlan => 1,
        WorkKind::Watch => 2,
        WorkKind::FlakeUpdate => 3,
        WorkKind::ReleaseSwarmApply => 3,
    }
}

/// Pure DAG builder — take a flat list of WorkItems, emit an
/// ExecutionPlan with stages honoring the dependency rules.
///
/// Stable within a stage: items sorted by (workspace, scope) so
/// reports + audit logs are reproducible across daemon ticks.
pub fn plan(items: Vec<WorkItem>) -> ExecutionPlan {
    // Deduplicate (same (kind, workspace, scope) can't appear twice).
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<WorkItem> = items
        .into_iter()
        .filter(|i| seen.insert((i.kind, i.workspace.clone(), i.scope.clone())))
        .collect();

    // Bucket by stage.
    let mut stages: Vec<Vec<WorkItem>> = (0..=3).map(|_| Vec::new()).collect();
    for item in deduped {
        let s = stage_for(item.kind);
        stages[s].push(item);
    }

    // Stable sort within each stage for reproducibility.
    for stage in &mut stages {
        stage.sort_by(|a, b| {
            (a.kind as u8, &a.workspace, &a.scope).cmp(&(b.kind as u8, &b.workspace, &b.scope))
        });
    }

    // Trim trailing empty stages, but keep the earliest non-empty
    // position index. If every stage is empty → empty plan.
    while stages.last().is_some_and(Vec::is_empty) {
        stages.pop();
    }

    ExecutionPlan { stages }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_empty_plan() {
        let p = plan(vec![]);
        assert!(p.is_empty());
        assert_eq!(p.total_items(), 0);
        assert_eq!(p.summary(), "no work");
    }

    #[test]
    fn single_sync_item_lands_in_stage_zero() {
        let p = plan(vec![WorkItem::workspace(WorkKind::Sync, "pleme-io")]);
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].len(), 1);
        assert_eq!(p.stages[0][0].kind, WorkKind::Sync);
    }

    #[test]
    fn pull_and_ci_trim_in_same_stage() {
        let p = plan(vec![
            WorkItem::workspace(WorkKind::Pull, "pleme-io"),
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
        ]);
        // Stages 0 empty (no Sync), stage 1 contains both
        assert_eq!(p.stages.len(), 2);
        assert!(p.stages[0].is_empty());
        assert_eq!(p.stages[1].len(), 2);
    }

    #[test]
    fn full_chain_produces_four_stages() {
        let p = plan(vec![
            WorkItem::workspace(WorkKind::Sync, "pleme-io"),
            WorkItem::workspace(WorkKind::Pull, "pleme-io"),
            WorkItem::workspace(WorkKind::Watch, "pleme-io"),
            WorkItem::workspace(WorkKind::FlakeUpdate, "pleme-io"),
        ]);
        assert_eq!(p.stages.len(), 4);
        assert_eq!(p.stages[0][0].kind, WorkKind::Sync);
        assert_eq!(p.stages[1][0].kind, WorkKind::Pull);
        assert_eq!(p.stages[2][0].kind, WorkKind::Watch);
        assert_eq!(p.stages[3][0].kind, WorkKind::FlakeUpdate);
    }

    #[test]
    fn release_swarm_apply_follows_plan() {
        let p = plan(vec![
            WorkItem::workspace(WorkKind::ReleaseSwarmPlan, "pleme-io"),
            WorkItem::workspace(WorkKind::ReleaseSwarmApply, "pleme-io"),
        ]);
        // Plan in stage 1, Apply in stage 3
        assert!(p.stages.len() >= 4);
        assert!(p.stages[1]
            .iter()
            .any(|i| i.kind == WorkKind::ReleaseSwarmPlan));
        assert!(p.stages[3]
            .iter()
            .any(|i| i.kind == WorkKind::ReleaseSwarmApply));
    }

    #[test]
    fn deduplication_removes_exact_duplicates() {
        let p = plan(vec![
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
        ]);
        assert_eq!(p.total_items(), 1);
    }

    #[test]
    fn per_repo_items_dont_deduplicate_across_repos() {
        let p = plan(vec![
            WorkItem::per_repo(WorkKind::Pull, "pleme-io", "dq"),
            WorkItem::per_repo(WorkKind::Pull, "pleme-io", "tend"),
        ]);
        assert_eq!(p.total_items(), 2);
    }

    #[test]
    fn multiple_workspaces_parallelize_in_same_stage() {
        let p = plan(vec![
            WorkItem::workspace(WorkKind::Sync, "pleme-io"),
            WorkItem::workspace(WorkKind::Sync, "drzzln"),
            WorkItem::workspace(WorkKind::Sync, "akeyless-community"),
        ]);
        assert_eq!(p.stages.len(), 1);
        assert_eq!(p.stages[0].len(), 3);
        // Sorted by (kind, workspace)
        assert_eq!(p.stages[0][0].workspace, "akeyless-community");
        assert_eq!(p.stages[0][1].workspace, "drzzln");
        assert_eq!(p.stages[0][2].workspace, "pleme-io");
    }

    #[test]
    fn summary_is_human_readable() {
        let p = plan(vec![
            WorkItem::workspace(WorkKind::Sync, "pleme-io"),
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
            WorkItem::workspace(WorkKind::Watch, "pleme-io"),
        ]);
        let s = p.summary();
        assert!(s.contains("3 items across 3 stages"));
        assert!(s.contains("1 sync"));
        assert!(s.contains("1 ci_trim"));
        assert!(s.contains("1 watch"));
    }

    #[test]
    fn plan_is_deterministic_across_input_orders() {
        let a = plan(vec![
            WorkItem::workspace(WorkKind::CiTrim, "drzzln"),
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
            WorkItem::workspace(WorkKind::Sync, "pleme-io"),
            WorkItem::workspace(WorkKind::Pull, "pleme-io"),
        ]);
        let b = plan(vec![
            WorkItem::workspace(WorkKind::Sync, "pleme-io"),
            WorkItem::workspace(WorkKind::Pull, "pleme-io"),
            WorkItem::workspace(WorkKind::CiTrim, "pleme-io"),
            WorkItem::workspace(WorkKind::CiTrim, "drzzln"),
        ]);
        assert_eq!(a, b, "plan must be stable under input permutation");
    }

    #[test]
    fn idle_plan_is_silent_by_contract() {
        // This is the quiet-when-idle contract: empty input →
        // is_empty() → daemon logs nothing.
        let p = plan(vec![]);
        assert!(p.is_empty());
        assert_eq!(p.total_items(), 0);
        assert_eq!(p.stages.len(), 0);
        // Summary returns a string — the daemon uses is_empty() to
        // decide whether to emit it, so the string itself can exist
        // without being logged.
        assert_eq!(p.summary(), "no work");
    }
}
