//! AI Flow Planner - DAG validation and execution order
//!
//! Validates flow definitions, detects cycles, computes topological sort.

use crate::ai_flow::{AiFlowPlan, AiStep};
use anyhow::{bail, Result};
use std::collections::HashMap;

#[derive(Debug)]
pub struct PlannedFlow {
    pub plan: AiFlowPlan,
    pub deps: Vec<Vec<usize>>,
    pub execution_order: Vec<usize>,
}

pub fn plan(flow: &AiFlowPlan) -> Result<PlannedFlow> {
    let mut id_to_idx: HashMap<&str, usize> = HashMap::new();

    for (i, step) in flow.steps.iter().enumerate() {
        if id_to_idx.insert(&step.id, i).is_some() {
            bail!("Duplicate step ID: {}", step.id);
        }
    }

    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(flow.steps.len());
    for step in &flow.steps {
        let mut step_deps = Vec::new();
        for dep_id in &step.depends_on {
            match id_to_idx.get(dep_id.as_str()) {
                Some(&idx) => step_deps.push(idx),
                None => bail!("Step '{}' depends on unknown step '{}'", step.id, dep_id),
            }
        }
        deps.push(step_deps);
    }

    detect_cycle(flow, &deps)?;

    let execution_order = topological_sort(&deps)?;

    Ok(PlannedFlow {
        plan: flow.clone(),
        deps,
        execution_order,
    })
}

fn detect_cycle(flow: &AiFlowPlan, deps: &[Vec<usize>]) -> Result<()> {
    let n = flow.steps.len();
    let mut color = vec![0u8; n];

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (step, step_deps) in deps.iter().enumerate() {
        for &dep in step_deps {
            adj[dep].push(step);
        }
    }

    for start in 0..n {
        if color[start] == 0 {
            dfs_visit(start, &adj, &mut color, flow)?;
        }
    }
    Ok(())
}

fn dfs_visit(node: usize, adj: &[Vec<usize>], color: &mut [u8], flow: &AiFlowPlan) -> Result<()> {
    color[node] = 1;
    for &next in &adj[node] {
        if color[next] == 1 {
            bail!(
                "Cycle detected: step '{}' → step '{}'",
                flow.steps[node].id,
                flow.steps[next].id
            );
        }
        if color[next] == 0 {
            dfs_visit(next, adj, color, flow)?;
        }
    }
    color[node] = 2;
    Ok(())
}

fn topological_sort(deps: &[Vec<usize>]) -> Result<Vec<usize>> {
    let n = deps.len();
    // in_degree[i] = number of deps step i is waiting on. A step is
    // ready to execute when in_degree[i] reaches 0 (all parents have
    // already been emitted).
    let mut in_degree = vec![0usize; n];
    for (i, step_deps) in deps.iter().enumerate() {
        in_degree[i] = step_deps.len();
    }

    // Process in ascending-index order so unconstrained steps emit
    // in declaration order — tests assert this for ergonomic
    // determinism.
    let mut queue: std::collections::VecDeque<usize> =
        (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut result = Vec::with_capacity(n);

    while let Some(node) = queue.pop_front() {
        result.push(node);
        // Every step `i` that listed `node` as a dep can decrement
        // its remaining-deps counter; if it hits 0 the step is ready.
        for (i, step_deps) in deps.iter().enumerate() {
            if step_deps.contains(&node) {
                in_degree[i] -= 1;
                if in_degree[i] == 0 {
                    queue.push_back(i);
                }
            }
        }
    }

    if result.len() != n {
        bail!("Unable to resolve dependencies - cycle detected");
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_flow::AiTaskRef;

    #[test]
    fn test_simple_linear_flow() {
        let mut flow = AiFlowPlan::new("test");
        flow.add_step(
            "a",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "a".into(),
            },
        );
        flow.add_step(
            "b",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "b".into(),
            },
        );

        let planned = plan(&flow).unwrap();
        assert_eq!(planned.execution_order, vec![0, 1]);
    }

    #[test]
    fn test_flow_with_dependencies() {
        let mut flow = AiFlowPlan::new("test");
        flow.add_step(
            "a",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "a".into(),
            },
        );
        flow.add_step(
            "b",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "b".into(),
            },
        );
        flow.add_step(
            "c",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "c".into(),
            },
        );

        flow.with_deps("b", vec!["a".into()]);
        flow.with_deps("c", vec!["a".into(), "b".into()]);

        let planned = plan(&flow).unwrap();
        assert_eq!(planned.execution_order[0], 0);
    }

    #[test]
    fn test_cycle_detection() {
        let mut flow = AiFlowPlan::new("test");
        flow.add_step(
            "a",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "a".into(),
            },
        );
        flow.add_step(
            "b",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "b".into(),
            },
        );
        flow.add_step(
            "c",
            AiTaskRef::Inline {
                model: "test".into(),
                prompt: "c".into(),
            },
        );

        flow.with_deps("b", vec!["a".into()]);
        flow.with_deps("c", vec!["b".into()]);
        flow.with_deps("a", vec!["c".into()]); // cycle!

        let result = plan(&flow);
        assert!(result.is_err());
    }
}
