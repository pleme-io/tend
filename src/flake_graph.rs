use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use crate::config::Workspace;
use crate::flake::UpdateStep;
use crate::flake_lock::ExtendedLockFile;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FlakeGraph {
    deps: HashMap<String, Vec<String>>,
    input_names: HashMap<String, BTreeMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeriveSkip {
    NoLock { repo: String },
    BadLock { repo: String, reason: String },
    CycleEdge { repo: String, dep: String },
}

impl FlakeGraph {
    pub(crate) fn derive(base_dir: &Path, org: &str) -> (Self, Vec<DeriveSkip>) {
        let mut repos: Vec<String> = std::fs::read_dir(base_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().join("flake.nix").is_file())
                    .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        repos.sort();
        let mut graph = Self::default();
        let mut skips = Vec::new();
        for repo in &repos {
            let lock_path = base_dir.join(repo).join("flake.lock");
            if !lock_path.is_file() {
                skips.push(DeriveSkip::NoLock { repo: repo.clone() });
                continue;
            }
            match ExtendedLockFile::read(&lock_path) {
                Ok(lock) => graph.add_repo(repo, &lock, org, |r| repos.iter().any(|x| x == r)),
                Err(e) => skips.push(DeriveSkip::BadLock {
                    repo: repo.clone(),
                    reason: format!("{e:#}"),
                }),
            }
        }
        graph.break_cycles(&mut skips);
        (graph, skips)
    }

    fn add_repo(
        &mut self,
        repo: &str,
        lock: &ExtendedLockFile,
        org: &str,
        known: impl Fn(&str) -> bool,
    ) {
        let mut by_dep: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (input, node_name) in lock.root_input_nodes() {
            let Some(locked) = lock.nodes.get(&node_name).and_then(|n| n.locked.as_ref()) else {
                continue;
            };
            if locked.kind != "github" {
                continue;
            }
            let (Some(owner), Some(dep)) = (&locked.owner, &locked.repo) else {
                continue;
            };
            if !owner.eq_ignore_ascii_case(org) || dep == repo || !known(dep) {
                continue;
            }
            by_dep.entry(dep.clone()).or_default().push(input);
        }
        if by_dep.is_empty() {
            return;
        }
        self.deps
            .insert(repo.to_owned(), by_dep.keys().cloned().collect());
        self.input_names.insert(repo.to_owned(), by_dep);
    }

    fn sccs(&self) -> Vec<Vec<String>> {
        struct Tarjan<'a> {
            g: &'a HashMap<String, Vec<String>>,
            idx: HashMap<&'a str, usize>,
            low: HashMap<&'a str, usize>,
            on: HashSet<&'a str>,
            stack: Vec<&'a str>,
            next: usize,
            out: Vec<Vec<String>>,
        }
        impl<'a> Tarjan<'a> {
            fn visit(&mut self, v: &'a str) {
                self.idx.insert(v, self.next);
                self.low.insert(v, self.next);
                self.next += 1;
                self.stack.push(v);
                self.on.insert(v);
                let g = self.g;
                for w in g.get(v).into_iter().flatten() {
                    let w = w.as_str();
                    if !self.idx.contains_key(w) {
                        self.visit(w);
                        let lw = self.low[w];
                        let lv = self.low.get_mut(v).unwrap();
                        *lv = (*lv).min(lw);
                    } else if self.on.contains(w) {
                        let iw = self.idx[w];
                        let lv = self.low.get_mut(v).unwrap();
                        *lv = (*lv).min(iw);
                    }
                }
                if self.low[v] == self.idx[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = self.stack.pop() {
                        self.on.remove(w);
                        comp.push(w.to_owned());
                        if w == v {
                            break;
                        }
                    }
                    if comp.len() > 1 {
                        comp.sort();
                        self.out.push(comp);
                    }
                }
            }
        }
        let mut t = Tarjan {
            g: &self.deps,
            idx: HashMap::new(),
            low: HashMap::new(),
            on: HashSet::new(),
            stack: Vec::new(),
            next: 0,
            out: Vec::new(),
        };
        let mut keys: Vec<&String> = self.deps.keys().collect();
        keys.sort();
        for k in keys {
            if !t.idx.contains_key(k.as_str()) {
                t.visit(k.as_str());
            }
        }
        t.out
    }

    fn break_cycles(&mut self, skips: &mut Vec<DeriveSkip>) {
        loop {
            let comps = self.sccs();
            if comps.is_empty() {
                break;
            }
            let picks: Vec<(String, Vec<String>)> = comps
                .iter()
                .map(|comp| {
                    let mut dependents: HashMap<&str, usize> = HashMap::new();
                    for (r, deps) in &self.deps {
                        if comp.contains(r) {
                            continue;
                        }
                        for d in deps {
                            *dependents.entry(d.as_str()).or_default() += 1;
                        }
                    }
                    let root = comp
                        .iter()
                        .max_by(|a, b| {
                            let ka = dependents.get(a.as_str()).copied().unwrap_or(0);
                            let kb = dependents.get(b.as_str()).copied().unwrap_or(0);
                            ka.cmp(&kb).then_with(|| b.cmp(a))
                        })
                        .cloned()
                        .unwrap_or_default();
                    (root, comp.clone())
                })
                .collect();
            for (root, comp) in picks {
                let Some(deps) = self.deps.get_mut(&root) else {
                    continue;
                };
                let (inside, outside): (Vec<String>, Vec<String>) =
                    deps.drain(..).partition(|d| comp.contains(d));
                *deps = outside;
                for dep in inside {
                    if let Some(names) = self.input_names.get_mut(&root) {
                        names.remove(&dep);
                    }
                    skips.push(DeriveSkip::CycleEdge {
                        repo: root.clone(),
                        dep,
                    });
                }
            }
            self.deps.retain(|_, d| !d.is_empty());
        }
        self.input_names.retain(|r, _| self.deps.contains_key(r));
    }

    pub(crate) fn with_overrides(mut self, explicit: &HashMap<String, Vec<String>>) -> Self {
        for (repo, deps) in explicit {
            self.deps.insert(repo.clone(), deps.clone());
            self.input_names.remove(repo);
        }
        self
    }

    pub(crate) fn for_workspace(ws: &Workspace) -> anyhow::Result<(Self, Vec<DeriveSkip>)> {
        let org = ws.org.clone().unwrap_or_else(|| ws.name.clone());
        let (g, skips) = Self::derive(&ws.resolved_base_dir()?, &org);
        Ok((g.with_overrides(&ws.flake_deps), skips))
    }

    pub(crate) fn deps(&self) -> &HashMap<String, Vec<String>> {
        &self.deps
    }

    pub(crate) fn to_input_names(&self, steps: Vec<UpdateStep>) -> Vec<UpdateStep> {
        steps
            .into_iter()
            .map(|s| {
                let inputs = match self.input_names.get(&s.repo) {
                    Some(names) => s
                        .inputs
                        .iter()
                        .flat_map(|d| names.get(d).cloned().unwrap_or_else(|| vec![d.clone()]))
                        .collect(),
                    None => s.inputs,
                };
                UpdateStep {
                    repo: s.repo,
                    inputs,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flake::compute_update_chain;

    fn lock(inputs: &[(&str, &str, &str)]) -> String {
        let mut nodes = serde_json::Map::new();
        let mut root_inputs = serde_json::Map::new();
        for (name, owner, repo) in inputs {
            root_inputs.insert((*name).into(), serde_json::json!(name));
            nodes.insert(
                (*name).into(),
                serde_json::json!({"locked": {"type": "github", "owner": owner, "repo": repo, "rev": "0"}}),
            );
        }
        root_inputs.insert(
            "nixpkgs-follow".into(),
            serde_json::json!(["substrate", "nixpkgs"]),
        );
        nodes.insert("root".into(), serde_json::json!({"inputs": root_inputs}));
        serde_json::json!({"nodes": nodes, "root": "root", "version": 7}).to_string()
    }

    fn write_repo(dir: &Path, repo: &str, lock_json: Option<String>) {
        let p = dir.join(repo);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("flake.nix"), "{}").unwrap();
        if let Some(l) = lock_json {
            std::fs::write(p.join("flake.lock"), l).unwrap();
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write_repo(
            d,
            "substrate",
            Some(lock(&[("nixpkgs", "NixOS", "nixpkgs")])),
        );
        write_repo(
            d,
            "lib-a",
            Some(lock(&[
                ("substrate", "pleme-io", "substrate"),
                ("nixpkgs", "NixOS", "nixpkgs"),
            ])),
        );
        write_repo(
            d,
            "app-b",
            Some(lock(&[
                ("sub", "Pleme-IO", "substrate"),
                ("lib-a", "pleme-io", "lib-a"),
                ("stranger", "someone", "lib-a"),
                ("gone", "pleme-io", "not-cloned"),
            ])),
        );
        write_repo(d, "bare", None);
        dir
    }

    #[test]
    fn derives_edges_from_locks_only_for_org_repos_on_disk() {
        let dir = fixture();
        let (g, skips) = FlakeGraph::derive(dir.path(), "pleme-io");
        assert_eq!(g.deps()["lib-a"], vec!["substrate"]);
        assert_eq!(g.deps()["app-b"], vec!["lib-a", "substrate"]);
        assert!(!g.deps().contains_key("substrate"));
        assert_eq!(
            skips,
            vec![DeriveSkip::NoLock {
                repo: "bare".into()
            }]
        );
    }

    #[test]
    fn substrate_chain_is_ordered_and_uses_input_names() {
        let dir = fixture();
        let (g, _) = FlakeGraph::derive(dir.path(), "pleme-io");
        let chain = g.to_input_names(compute_update_chain("substrate", g.deps()).unwrap());
        let order: Vec<&str> = chain.iter().map(|s| s.repo.as_str()).collect();
        assert_eq!(order, vec!["lib-a", "app-b"]);
        assert_eq!(chain[0].inputs, vec!["substrate"]);
        let mut b = chain[1].inputs.clone();
        b.sort();
        assert_eq!(b, vec!["lib-a", "sub"]);
    }

    #[test]
    fn explicit_flake_deps_override_one_repo_and_leave_the_rest_derived() {
        let dir = fixture();
        let (g, _) = FlakeGraph::derive(dir.path(), "pleme-io");
        let explicit = HashMap::from([("app-b".to_string(), vec!["lib-a".to_string()])]);
        let g = g.with_overrides(&explicit);
        assert_eq!(g.deps()["app-b"], vec!["lib-a"]);
        assert_eq!(g.deps()["lib-a"], vec!["substrate"]);
    }

    #[test]
    fn unreadable_lock_is_a_recorded_skip_not_an_abort() {
        let dir = fixture();
        std::fs::write(dir.path().join("lib-a/flake.lock"), "not json").unwrap();
        let (g, skips) = FlakeGraph::derive(dir.path(), "pleme-io");
        assert!(!g.deps().contains_key("lib-a"));
        assert!(skips
            .iter()
            .any(|s| matches!(s, DeriveSkip::BadLock { repo, .. } if repo == "lib-a")));
        assert_eq!(g.deps()["app-b"], vec!["lib-a", "substrate"]);
    }

    #[test]
    fn a_cycle_drops_the_foundations_back_edge_and_the_chain_still_runs() {
        let dir = fixture();
        let d = dir.path();
        std::fs::write(
            d.join("substrate/flake.lock"),
            lock(&[("lib-a", "pleme-io", "lib-a")]),
        )
        .unwrap();
        write_repo(
            d,
            "app-c",
            Some(lock(&[("substrate", "pleme-io", "substrate")])),
        );
        let (g, skips) = FlakeGraph::derive(d, "pleme-io");
        assert!(!g.deps().contains_key("substrate"));
        assert_eq!(g.deps()["lib-a"], vec!["substrate"]);
        assert_eq!(
            skips,
            vec![
                DeriveSkip::NoLock {
                    repo: "bare".into()
                },
                DeriveSkip::CycleEdge {
                    repo: "substrate".into(),
                    dep: "lib-a".into()
                },
            ]
        );
        let chain = compute_update_chain("substrate", g.deps()).unwrap();
        assert_eq!(chain.len(), 3);
    }
}
