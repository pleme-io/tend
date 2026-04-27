//! Plan generator — mechanical analysis of which upstream lookups
//! to actually perform this cycle. Emits a typed `FlakeUpdatePlan`
//! before any HTTP traffic.
//!
//! Inputs (all already in scope from earlier phases):
//!   - `ExtendedLockFile` — the parsed flake.lock
//!   - `BTreeSet<String>` declared inputs from `flake_nix::read_flake_inputs`
//!   - `FlakeUpdatePolicy` — per-input mode (Auto/Gated/Locked/Forbidden)
//!   - `RequestBudget` — current effective req/hr + remaining slots
//!   - `HashMap<UpstreamId, CachedHead>` — what we already know
//!
//! Output: `FlakeUpdatePlanSpec` (CRD persists for audit) + a derived
//! ordered list of `UpstreamId`s to actually check.
//!
//! The planner is pure (no I/O) so it's easy to test and replay. The
//! reconciler decides whether to emit the CR; the planner just builds
//! the data.

use std::collections::{BTreeSet, HashMap};

use chrono::{Duration, Utc};

use super::budget::RequestBudget;
use super::crds::{
    FlakeUpdatePlanSpec, FlakeUpdatePolicy, PlanAction, PlannedCheck, UpdateMode,
};
use super::upstream::{CachedHead, UpstreamId};
use crate::flake_lock::ExtendedLockFile;

/// Default TTL for `UseCached` decisions — entries fetched within
/// this window don't warrant a refresh, even when free (etag-only).
/// Tunable via `TEND_USE_CACHED_TTL_SECONDS` env (Helm chart values:
/// `cache.useCachedTtlSeconds`). Default keeps reconcile cycles
/// cheap when nothing has likely moved.
const DEFAULT_USE_CACHED_TTL: Duration = Duration::minutes(20);

fn use_cached_ttl() -> Duration {
    std::env::var("TEND_USE_CACHED_TTL_SECONDS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::seconds)
        .unwrap_or(DEFAULT_USE_CACHED_TTL)
}

/// One plan output: the CRD spec for audit + a parallel list of
/// `UpstreamId`s the discovery layer should actually call. Splitting
/// these so the reconciler can ship the plan to k8s before doing any
/// network traffic, then iterate the typed list.
pub struct PlanOutput {
    pub spec: FlakeUpdatePlanSpec,
    pub refresh_targets: Vec<(String, UpstreamId)>,
}

#[allow(clippy::too_many_arguments)] // typed dataflow > arg count
pub fn plan(
    policy: &FlakeUpdatePolicy,
    lock: &ExtendedLockFile,
    declared_inputs: Option<&BTreeSet<String>>,
    head_cache: &HashMap<UpstreamId, CachedHead>,
    budget: &RequestBudget,
) -> PlanOutput {
    let now = Utc::now();
    let ttl = use_cached_ttl();
    let effective_max = budget.effective_max_per_hour();
    let mut slots_remaining = budget.slots_remaining();

    let mut checks: Vec<PlannedCheck> = Vec::new();
    let mut refresh_targets: Vec<(String, UpstreamId)> = Vec::new();

    // Sort root inputs deterministically so plans diff cleanly across
    // cycles and policies aren't sensitive to map iteration order.
    let mut roots = lock.root_input_nodes();
    roots.sort();

    for (local_name, node_name) in roots {
        // 1. Skip ghosts (not declared in flake.nix).
        if let Some(declared) = declared_inputs {
            if !declared.contains(&local_name) {
                checks.push(PlannedCheck {
                    input: local_name,
                    action: PlanAction::Skip,
                    rationale: "ghost: input absent from flake.nix".into(),
                });
                continue;
            }
        }

        // 2. Skip Locked / Forbidden inputs.
        let mode = policy
            .spec
            .inputs
            .get(&local_name)
            .copied()
            .unwrap_or(policy.spec.default_mode);
        if matches!(mode, UpdateMode::Locked | UpdateMode::Forbidden) {
            checks.push(PlannedCheck {
                input: local_name,
                action: PlanAction::Skip,
                rationale: format!("policy mode: {mode:?}"),
            });
            continue;
        }

        // 3. Resolve UpstreamId. Skip non-github + non-locked inputs.
        let Some(node) = lock.nodes.get(&node_name) else {
            checks.push(PlannedCheck {
                input: local_name,
                action: PlanAction::Skip,
                rationale: format!("lock node `{node_name}` missing"),
            });
            continue;
        };
        let Some(locked) = &node.locked else {
            checks.push(PlannedCheck {
                input: local_name,
                action: PlanAction::Skip,
                rationale: "lock entry has no `locked` block".into(),
            });
            continue;
        };
        if locked.kind != "github" {
            checks.push(PlannedCheck {
                input: local_name,
                action: PlanAction::Skip,
                rationale: format!("non-github source: {}", locked.kind),
            });
            continue;
        }
        let (Some(owner), Some(repo)) = (locked.owner.as_deref(), locked.repo.as_deref()) else {
            checks.push(PlannedCheck {
                input: local_name,
                action: PlanAction::Skip,
                rationale: "locked block missing owner/repo".into(),
            });
            continue;
        };
        let id = UpstreamId::new_github(owner, repo, node.tracking_ref());

        // 4. UseCached when we have a recent entry. Discovery still
        //    gets the cached info to compare against current pin —
        //    no budget cost.
        if let Some(cached) = head_cache.get(&id) {
            if now.signed_duration_since(cached.fetched_at) < ttl {
                checks.push(PlannedCheck {
                    input: local_name,
                    action: PlanAction::UseCached,
                    rationale: format!(
                        "cache hit, age {}s",
                        now.signed_duration_since(cached.fetched_at).num_seconds(),
                    ),
                });
                continue;
            }
        }

        // 5. Budget check. Refresh fits → schedule it. Doesn't fit →
        //    Defer. Budget is decremented in plan order so earlier
        //    inputs win when there's contention; sorting by name
        //    above gives this stable, predictable behavior.
        if slots_remaining > 0 {
            slots_remaining -= 1;
            checks.push(PlannedCheck {
                input: local_name.clone(),
                action: PlanAction::Refresh,
                rationale: if head_cache.contains_key(&id) {
                    "scheduled refresh (etag-conditional)".into()
                } else {
                    "scheduled refresh (no cache, fresh fetch)".into()
                },
            });
            refresh_targets.push((local_name, id));
        } else {
            checks.push(PlannedCheck {
                input: local_name,
                action: PlanAction::Defer,
                rationale: "budget exhausted, retry next cycle".into(),
            });
        }
    }

    // Stable order for Refreshes first, then UseCached, Defer, Skip;
    // alphabetical within each. Makes plan diffs across cycles legible.
    checks.sort_by(|a, b| {
        action_order(a.action)
            .cmp(&action_order(b.action))
            .then_with(|| a.input.cmp(&b.input))
    });

    let spec = FlakeUpdatePlanSpec {
        policy_name: policy.metadata.name.clone().unwrap_or_default(),
        generated_at: now,
        budget_effective_max_per_hour: effective_max,
        budget_slots_remaining: budget.slots_remaining(),
        checks,
    };
    PlanOutput { spec, refresh_targets }
}

const fn action_order(a: PlanAction) -> u8 {
    match a {
        PlanAction::Refresh => 0,
        PlanAction::UseCached => 1,
        PlanAction::Defer => 2,
        PlanAction::Skip => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operator::crds::{FlakeUpdatePolicy, FlakeUpdatePolicySpec, RepoRef};
    use crate::operator::upstream::HeadInfo;
    use kube::api::ObjectMeta;
    use std::collections::BTreeMap;

    fn make_policy(name: &str, modes: BTreeMap<String, UpdateMode>) -> FlakeUpdatePolicy {
        FlakeUpdatePolicy {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some("tend-system".into()),
                ..Default::default()
            },
            spec: FlakeUpdatePolicySpec {
                repo: RepoRef { workspace: "ws".into(), repo: "r".into() },
                inputs: modes,
                default_mode: UpdateMode::Gated,
                gates: vec![],
                window: None,
            },
            status: None,
        }
    }

    fn lock_with_inputs(inputs: &[(&str, &str, &str, &str, &str)]) -> ExtendedLockFile {
        // Each tuple: (local_name, node_name, owner, repo, rev)
        let mut nodes_json = serde_json::json!({
            "root": { "inputs": {} }
        });
        let root_inputs = nodes_json["root"]["inputs"].as_object_mut().unwrap();
        for (local, node, _, _, _) in inputs {
            root_inputs.insert((*local).into(), serde_json::json!(*node));
        }
        let nodes_obj = nodes_json.as_object_mut().unwrap();
        for (_, node, owner, repo, rev) in inputs {
            nodes_obj.insert(
                (*node).into(),
                serde_json::json!({
                    "locked": {
                        "type": "github",
                        "owner": owner,
                        "repo": repo,
                        "rev": rev,
                        "lastModified": 1700000000,
                    }
                }),
            );
        }
        let raw = serde_json::json!({ "nodes": nodes_json, "root": "root", "version": 7 });
        ExtendedLockFile::parse(&raw.to_string()).expect("parse synthetic lock")
    }

    #[test]
    fn ghost_inputs_are_skipped_with_clear_rationale() {
        let policy = make_policy("p", BTreeMap::new());
        let lock = lock_with_inputs(&[("ghost", "ghost", "x", "y", "abc")]);
        let declared: BTreeSet<String> = ["other".into()].into_iter().collect();
        let cache = HashMap::new();
        let budget = RequestBudget::new(100);
        let out = plan(&policy, &lock, Some(&declared), &cache, &budget);
        let g = out.spec.checks.iter().find(|c| c.input == "ghost").unwrap();
        assert_eq!(g.action, PlanAction::Skip);
        assert!(g.rationale.contains("ghost"));
        assert!(out.refresh_targets.is_empty());
    }

    #[test]
    fn locked_and_forbidden_modes_are_skipped() {
        let modes: BTreeMap<String, UpdateMode> = [
            ("locked-input".into(), UpdateMode::Locked),
            ("forbidden-input".into(), UpdateMode::Forbidden),
            ("auto-input".into(), UpdateMode::Auto),
        ]
        .into_iter()
        .collect();
        let policy = make_policy("p", modes);
        let lock = lock_with_inputs(&[
            ("locked-input", "locked-input", "x", "y", "1"),
            ("forbidden-input", "forbidden-input", "x", "y", "2"),
            ("auto-input", "auto-input", "x", "y", "3"),
        ]);
        let cache = HashMap::new();
        let budget = RequestBudget::new(100);
        let out = plan(&policy, &lock, None, &cache, &budget);

        let l = out.spec.checks.iter().find(|c| c.input == "locked-input").unwrap();
        let f = out.spec.checks.iter().find(|c| c.input == "forbidden-input").unwrap();
        let a = out.spec.checks.iter().find(|c| c.input == "auto-input").unwrap();
        assert_eq!(l.action, PlanAction::Skip);
        assert_eq!(f.action, PlanAction::Skip);
        assert_eq!(a.action, PlanAction::Refresh);
        assert_eq!(out.refresh_targets.len(), 1);
    }

    #[test]
    fn cache_hit_within_ttl_uses_cached_no_refresh() {
        let policy = make_policy("p", BTreeMap::new());
        let lock = lock_with_inputs(&[("input", "input", "ns", "r", "abc")]);
        let id = UpstreamId::new_github("ns", "r", "HEAD");
        let mut cache = HashMap::new();
        cache.insert(
            id,
            CachedHead {
                info: HeadInfo {
                    upstream_rev: "abc".into(),
                    upstream_modified: 1700000000,
                },
                etag: Some("\"x\"".into()),
                fetched_at: Utc::now() - Duration::minutes(5),
            },
        );
        let budget = RequestBudget::new(100);
        let out = plan(&policy, &lock, None, &cache, &budget);
        let c = out.spec.checks.iter().find(|c| c.input == "input").unwrap();
        assert_eq!(c.action, PlanAction::UseCached);
        assert!(out.refresh_targets.is_empty());
    }

    #[test]
    fn cache_hit_beyond_ttl_schedules_refresh() {
        let policy = make_policy("p", BTreeMap::new());
        let lock = lock_with_inputs(&[("input", "input", "ns", "r", "abc")]);
        let id = UpstreamId::new_github("ns", "r", "HEAD");
        let mut cache = HashMap::new();
        cache.insert(
            id,
            CachedHead {
                info: HeadInfo { upstream_rev: "abc".into(), upstream_modified: 0 },
                etag: Some("\"x\"".into()),
                fetched_at: Utc::now() - Duration::hours(1),
            },
        );
        let budget = RequestBudget::new(100);
        let out = plan(&policy, &lock, None, &cache, &budget);
        let c = out.spec.checks.iter().find(|c| c.input == "input").unwrap();
        assert_eq!(c.action, PlanAction::Refresh);
        assert!(c.rationale.contains("etag-conditional"));
        assert_eq!(out.refresh_targets.len(), 1);
    }

    #[tokio::test]
    async fn budget_exhaustion_defers_remaining() {
        // Build 5 inputs, budget allows 2.
        let inputs: Vec<(&str, &str, &str, &str, &str)> = vec![
            ("a", "a", "x", "y", "1"),
            ("b", "b", "x", "y", "2"),
            ("c", "c", "x", "y", "3"),
            ("d", "d", "x", "y", "4"),
            ("e", "e", "x", "y", "5"),
        ];
        let policy = make_policy("p", BTreeMap::new());
        let lock = lock_with_inputs(&inputs);
        let cache = HashMap::new();
        let budget = RequestBudget::new(2);
        // Pre-burn 0 — fresh budget gives 2 slots.
        let out = plan(&policy, &lock, None, &cache, &budget);

        let refreshes: Vec<&str> = out
            .spec
            .checks
            .iter()
            .filter(|c| c.action == PlanAction::Refresh)
            .map(|c| c.input.as_str())
            .collect();
        let defers: Vec<&str> = out
            .spec
            .checks
            .iter()
            .filter(|c| c.action == PlanAction::Defer)
            .map(|c| c.input.as_str())
            .collect();
        assert_eq!(refreshes.len(), 2, "got refreshes: {refreshes:?}");
        assert_eq!(defers.len(), 3, "got defers: {defers:?}");
        // Alphabetically earliest two should win (sort + greedy).
        assert_eq!(refreshes, vec!["a", "b"]);
    }

    #[test]
    fn checks_sort_refresh_then_cached_then_defer_then_skip() {
        // Mix of every action class — verify ordering.
        let modes: BTreeMap<String, UpdateMode> = [
            ("z-locked".into(), UpdateMode::Locked),
            ("y-auto".into(), UpdateMode::Auto),
        ]
        .into_iter()
        .collect();
        let policy = make_policy("p", modes);
        let inputs: Vec<(&str, &str, &str, &str, &str)> = vec![
            ("y-auto", "y-auto", "ns", "r", "1"),
            ("z-locked", "z-locked", "ns", "r", "2"),
        ];
        let lock = lock_with_inputs(&inputs);
        let cache = HashMap::new();
        let budget = RequestBudget::new(10);
        let out = plan(&policy, &lock, None, &cache, &budget);

        // First entry should be Refresh (y-auto), last should be Skip (z-locked).
        assert_eq!(out.spec.checks.first().unwrap().action, PlanAction::Refresh);
        assert_eq!(out.spec.checks.last().unwrap().action, PlanAction::Skip);
    }
}
