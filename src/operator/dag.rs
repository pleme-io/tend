//! DAG planner.
//!
//! Builds a typed dependency graph across the union of every active
//! `FlakeUpdatePolicy`'s flake.lock, then partitions the affected
//! pin set into topological waves. Each wave's proposals run their
//! verification gates in parallel; promotion to wave N+1 requires
//! every proposal in wave N to be `Verified`.
//!
//! For Phase 1 the DAG is *flake-only*. Phase 2-4 will add nodes
//! from Helm/Cargo/image domains; the planner stays domain-agnostic
//! because edges flow through the `LockFormat` trait.

use anyhow::Result;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A node in the fleet update DAG. For Phase 1, `(repo_path, input_name)`
/// is the unique identifier — repo-qualified so two repos pinning the
/// same input are distinct nodes (they may verify independently).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DagNodeId {
    pub repo: String,
    pub input: String,
}

impl DagNodeId {
    pub fn new(repo: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            input: input.into(),
        }
    }
}

/// Constructed DAG ready for wave partitioning.
pub struct FleetDag {
    graph: DiGraph<DagNodeId, ()>,
    index: HashMap<DagNodeId, NodeIndex>,
}

impl FleetDag {
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            index: HashMap::new(),
        }
    }

    /// Add a node if absent; return its index either way.
    pub fn ensure_node(&mut self, id: DagNodeId) -> NodeIndex {
        if let Some(idx) = self.index.get(&id) {
            return *idx;
        }
        let idx = self.graph.add_node(id.clone());
        self.index.insert(id, idx);
        idx
    }

    /// Add edge `from → to`. Idempotent: re-adding an existing edge is
    /// a no-op (we de-dup explicitly because petgraph would otherwise
    /// store parallel edges).
    pub fn add_edge(&mut self, from: DagNodeId, to: DagNodeId) {
        let f = self.ensure_node(from);
        let t = self.ensure_node(to);
        if !self.graph.contains_edge(f, t) {
            self.graph.add_edge(f, t, ());
        }
    }

    /// Topologically sort the entire DAG. Returns nodes in order
    /// (parents before children). Errors if a cycle is found —
    /// flake.lock follows-graphs are acyclic by construction so this
    /// indicates a malformed lock file.
    pub fn toposort(&self) -> Result<Vec<DagNodeId>> {
        let order = toposort(&self.graph, None)
            .map_err(|cyc| anyhow::anyhow!("cycle in fleet DAG at {:?}", self.graph[cyc.node_id()]))?;
        Ok(order.into_iter().map(|i| self.graph[i].clone()).collect())
    }

    /// Partition the DAG into topological waves. Wave 0 = nodes with
    /// zero remaining dependencies; wave N = nodes whose dependencies
    /// all live in waves [0..N).
    ///
    /// Restricted to `affected` if provided — useful when only a subset
    /// of nodes have actually advanced upstream and need verification.
    /// Unaffected nodes are skipped entirely.
    pub fn waves(&self, affected: Option<&BTreeSet<DagNodeId>>) -> Result<Vec<Vec<DagNodeId>>> {
        let order = self.toposort()?;
        let restrict_to: Option<BTreeSet<DagNodeId>> = affected.cloned();

        let want = |id: &DagNodeId| -> bool {
            restrict_to.as_ref().map_or(true, |s| s.contains(id))
        };

        let mut wave_of: BTreeMap<DagNodeId, usize> = BTreeMap::new();
        for id in &order {
            if !want(id) {
                continue;
            }
            let idx = self.index[id];
            let mut depth = 0;
            for parent in self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
            {
                let pid = &self.graph[parent];
                if want(pid) {
                    if let Some(pd) = wave_of.get(pid) {
                        depth = depth.max(*pd + 1);
                    }
                }
            }
            wave_of.insert(id.clone(), depth);
        }

        let max_wave = wave_of.values().copied().max().unwrap_or(0);
        let mut waves = vec![Vec::new(); max_wave + 1];
        for (id, depth) in wave_of {
            waves[depth].push(id);
        }
        // Sort within each wave for deterministic ordering.
        for w in &mut waves {
            w.sort();
        }
        Ok(waves)
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }
}

impl Default for FleetDag {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(repo: &str, input: &str) -> DagNodeId {
        DagNodeId::new(repo, input)
    }

    #[test]
    fn empty_dag_has_zero_nodes() {
        let d = FleetDag::new();
        assert_eq!(d.node_count(), 0);
        assert_eq!(d.toposort().unwrap(), Vec::<DagNodeId>::new());
    }

    #[test]
    fn linear_chain_partitions_into_waves() {
        let mut d = FleetDag::new();
        d.add_edge(n("nix", "substrate"), n("nix", "blackmatter"));
        d.add_edge(n("nix", "blackmatter"), n("nix", "shell"));

        let waves = d.waves(None).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0], vec![n("nix", "substrate")]);
        assert_eq!(waves[1], vec![n("nix", "blackmatter")]);
        assert_eq!(waves[2], vec![n("nix", "shell")]);
    }

    #[test]
    fn diamond_collapses_to_three_waves() {
        // root → {a, b} → c
        let mut d = FleetDag::new();
        d.add_edge(n("r", "root"), n("r", "a"));
        d.add_edge(n("r", "root"), n("r", "b"));
        d.add_edge(n("r", "a"), n("r", "c"));
        d.add_edge(n("r", "b"), n("r", "c"));

        let waves = d.waves(None).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].len(), 1);
        assert_eq!(waves[1].len(), 2);
        assert_eq!(waves[2].len(), 1);
        assert_eq!(waves[2][0], n("r", "c"));
    }

    #[test]
    fn affected_filter_prunes_unaffected_nodes() {
        let mut d = FleetDag::new();
        d.add_edge(n("r", "a"), n("r", "b"));
        d.add_edge(n("r", "b"), n("r", "c"));

        let affected: BTreeSet<DagNodeId> = [n("r", "b"), n("r", "c")].into_iter().collect();
        let waves = d.waves(Some(&affected)).unwrap();
        // Only b + c — no a wave even though it's a parent.
        let flat: Vec<DagNodeId> = waves.into_iter().flatten().collect();
        assert!(flat.contains(&n("r", "b")));
        assert!(flat.contains(&n("r", "c")));
        assert!(!flat.contains(&n("r", "a")));
    }

    #[test]
    fn idempotent_edge_add() {
        let mut d = FleetDag::new();
        d.add_edge(n("r", "a"), n("r", "b"));
        d.add_edge(n("r", "a"), n("r", "b"));
        d.add_edge(n("r", "a"), n("r", "b"));
        assert_eq!(d.edge_count(), 1);
    }
}
