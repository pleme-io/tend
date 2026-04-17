use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Command;

use crate::config::Workspace;
use crate::display;

/// Options that govern execution of an update chain.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExecOptions {
    pub dry_run: bool,
    pub quiet: bool,
    /// Clone the repo from its workspace's git host if it's not on disk.
    pub auto_clone: bool,
    /// Run `git pull --ff-only` before `nix flake update`.
    pub pull_before_update: bool,
}

/// A single step in the update chain.
#[derive(Debug)]
pub(crate) struct UpdateStep {
    /// Repo to update (directory name under base_dir)
    pub repo: String,
    /// Flake inputs to pass to `nix flake update`
    pub inputs: Vec<String>,
}

/// Compute the ordered chain of repos to update after `changed` was pushed.
///
/// Uses the `flake_deps` map (repo → list of inputs it depends on) to:
/// 1. Build a reverse map (input → repos that depend on it)
/// 2. BFS from `changed` to find all transitively affected repos
/// 3. Topological sort (Kahn's) the affected repos
/// 4. For each repo, compute which inputs were updated earlier in the chain
pub(crate) fn compute_update_chain(
    changed: &str,
    flake_deps: &HashMap<String, Vec<String>>,
) -> Result<Vec<UpdateStep>> {
    // Build reverse dependency map: input → set of repos that depend on it
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for (repo, deps) in flake_deps {
        for dep in deps {
            reverse.entry(dep.as_str()).or_default().push(repo.as_str());
        }
    }

    // BFS to find all transitively affected repos
    let mut affected: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    queue.push_back(changed);

    while let Some(current) = queue.pop_front() {
        if let Some(dependents) = reverse.get(current) {
            for &dep in dependents {
                if affected.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }
    }

    if affected.is_empty() {
        return Ok(vec![]);
    }

    // Kahn's topological sort over affected repos only
    // Build in-degree map restricted to affected set
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();

    for &repo in &affected {
        in_degree.entry(repo).or_insert(0);
        if let Some(deps) = flake_deps.get(repo) {
            for dep in deps {
                // Only count ordering edges between affected repos.
                // The `changed` node is the external trigger and is not part of
                // the topological sort — edges from it must not inflate in-degree
                // (they would never be decremented, causing false cycle errors).
                if affected.contains(dep.as_str()) {
                    forward.entry(dep.as_str()).or_default().push(repo);
                    *in_degree.entry(repo).or_insert(0) += 1;
                }
            }
        }
    }

    let mut sorted: Vec<&str> = Vec::new();
    let mut topo_queue: VecDeque<&str> = VecDeque::new();

    for (&repo, &deg) in &in_degree {
        if deg == 0 {
            topo_queue.push_back(repo);
        }
    }

    while let Some(repo) = topo_queue.pop_front() {
        sorted.push(repo);
        if let Some(dependents) = forward.get(repo) {
            for &dep in dependents {
                if let Some(deg) = in_degree.get_mut(dep) {
                    *deg -= 1;
                    if *deg == 0 {
                        topo_queue.push_back(dep);
                    }
                }
            }
        }
    }

    if sorted.len() != affected.len() {
        bail!(
            "cycle detected in flake_deps among: {:?}",
            affected.difference(&sorted.iter().copied().collect())
        );
    }

    // For each repo in sorted order, figure out which inputs to update.
    // An input should be updated if it is `changed` itself or was updated
    // in an earlier step.
    let mut updated_so_far: HashSet<&str> = HashSet::new();
    updated_so_far.insert(changed);

    let mut steps = Vec::new();
    for &repo in &sorted {
        let deps = flake_deps.get(repo)
            .ok_or_else(|| anyhow::anyhow!("repo '{repo}' missing from flake_deps after topo sort"))?;
        let inputs: Vec<String> = deps
            .iter()
            .filter(|d| updated_so_far.contains(d.as_str()))
            .cloned()
            .collect();

        if !inputs.is_empty() {
            steps.push(UpdateStep {
                repo: repo.to_string(),
                inputs,
            });
            updated_so_far.insert(repo);
        }
    }

    Ok(steps)
}

/// Compute the update chain assuming every repo in `flake_deps` was just changed.
///
/// Emits one step per repo (in topological dependency order), with that repo's full set
/// of inputs to update. Used by `--all` mode to perform a single union pass without
/// duplicating work across multiple per-trigger invocations.
pub(crate) fn compute_update_chain_all(
    flake_deps: &HashMap<String, Vec<String>>,
) -> Result<Vec<UpdateStep>> {
    if flake_deps.is_empty() {
        return Ok(vec![]);
    }

    // Collect every node referenced by the graph (sources + leaves).
    let mut all_nodes: HashSet<&str> = HashSet::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();

    for (repo, deps) in flake_deps {
        all_nodes.insert(repo.as_str());
        in_degree.entry(repo.as_str()).or_insert(0);
        for dep in deps {
            all_nodes.insert(dep.as_str());
            in_degree.entry(dep.as_str()).or_insert(0);
            reverse.entry(dep.as_str()).or_default().push(repo.as_str());
            *in_degree.get_mut(repo.as_str()).unwrap() += 1;
        }
    }

    // Kahn's topological sort over the full DAG.
    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter_map(|(&k, &v)| if v == 0 { Some(k) } else { None })
        .collect();
    let mut sorted: Vec<&str> = Vec::new();

    while let Some(node) = queue.pop_front() {
        sorted.push(node);
        if let Some(dependents) = reverse.get(node) {
            for &dep in dependents {
                if let Some(deg) = in_degree.get_mut(dep) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(dep);
                    }
                }
            }
        }
    }

    if sorted.len() != all_nodes.len() {
        let missing: HashSet<&str> = all_nodes
            .difference(&sorted.iter().copied().collect())
            .copied()
            .collect();
        bail!("cycle detected in flake_deps among: {:?}", missing);
    }

    // Emit a step for every node that has flake_deps. Each step carries that
    // repo's full input list, since all of them are considered updated.
    let mut steps = Vec::new();
    for node in sorted {
        if let Some(deps) = flake_deps.get(node) {
            if !deps.is_empty() {
                steps.push(UpdateStep {
                    repo: node.to_string(),
                    inputs: deps.clone(),
                });
            }
        }
    }

    Ok(steps)
}

/// Execute the update chain: for each step, run nix flake update, commit, push.
pub(crate) fn execute_update_chain(
    workspace: &Workspace,
    chain: &[UpdateStep],
    opts: ExecOptions,
) -> Result<()> {
    let base_dir = workspace.resolved_base_dir()?;
    let total = chain.len();

    for (i, step) in chain.iter().enumerate() {
        let step_num = i + 1;
        let repo_path = base_dir.join(&step.repo);

        if !opts.quiet {
            display::print_flake_step_start(step_num, total, &step.repo, &step.inputs);
        }

        if !repo_path.exists() {
            if !opts.auto_clone {
                bail!("repo directory does not exist: {}", repo_path.display());
            }
            if opts.dry_run {
                if !opts.quiet {
                    println!("    would clone {} into {}", step.repo, base_dir.display());
                }
            } else {
                clone_repo(workspace, &step.repo, &base_dir, opts.quiet)?;
            }
        }

        if opts.dry_run {
            if !opts.quiet {
                display::print_flake_step_dry_run();
            }
            continue;
        }

        // Check for clean working tree
        ensure_clean(&repo_path)
            .with_context(|| format!("{} has uncommitted changes", step.repo))?;

        if opts.pull_before_update {
            git_pull_ff(&repo_path, &step.repo, opts.quiet)?;
        }

        // nix flake update <inputs...>
        let mut args = vec!["flake", "update"];
        for input in &step.inputs {
            args.push(input);
        }

        let output = Command::new("nix")
            .args(&args)
            .current_dir(&repo_path)
            .output()
            .with_context(|| format!("running nix flake update in {}", step.repo))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("nix flake update failed in {}: {}", step.repo, stderr);
        }

        // git add flake.lock
        let output = Command::new("git")
            .args(["add", "flake.lock"])
            .current_dir(&repo_path)
            .output()
            .with_context(|| format!("git add flake.lock in {}", step.repo))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git add failed in {}: {}", step.repo, stderr);
        }

        // Check if flake.lock actually changed
        let diff = Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&repo_path)
            .status()
            .with_context(|| format!("checking staged changes in {}", step.repo))?;

        if diff.success() {
            // No changes staged — lock file unchanged
            if !opts.quiet {
                display::print_flake_step_no_changes(&step.repo);
            }
            continue;
        }

        // Commit
        let msg = format!("chore: update {}", step.inputs.join(" "));
        let output = Command::new("git")
            .args(["commit", "-m", &msg])
            .current_dir(&repo_path)
            .output()
            .with_context(|| format!("git commit in {}", step.repo))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git commit failed in {}: {}", step.repo, stderr);
        }

        // Push
        let output = Command::new("git")
            .args(["push"])
            .current_dir(&repo_path)
            .output()
            .with_context(|| format!("git push in {}", step.repo))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git push failed in {}: {}", step.repo, stderr);
        }

        if !opts.quiet {
            display::print_flake_step_done(&step.repo);
        }
    }

    Ok(())
}

fn clone_repo(workspace: &Workspace, repo: &str, base_dir: &Path, quiet: bool) -> Result<()> {
    std::fs::create_dir_all(base_dir)
        .with_context(|| format!("creating {}", base_dir.display()))?;
    let url = workspace.clone_url(repo);
    let target = base_dir.join(repo);
    if !quiet {
        println!("    cloning {url} → {}", target.display());
    }
    let output = Command::new("git")
        .args(["clone", &url, &target.to_string_lossy()])
        .output()
        .with_context(|| format!("git clone {url}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git clone failed for {repo}: {}", stderr.trim());
    }
    Ok(())
}

fn git_pull_ff(repo_path: &Path, repo: &str, quiet: bool) -> Result<()> {
    let output = Command::new("git")
        .args(["pull", "--ff-only", "--quiet"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("git pull --ff-only in {repo}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git pull --ff-only failed in {repo}: {}", stderr.trim());
    }
    if !quiet {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        if !combined.trim().is_empty() && !combined.contains("Already up to date") {
            println!("    pulled origin into {repo}");
        }
    }
    Ok(())
}

fn ensure_clean(repo_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("checking git status in {}", repo_path.display()))?;

    if !output.stdout.is_empty() {
        bail!("working tree is dirty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_compute_update_chain_empty_deps() {
        let deps: HashMap<String, Vec<String>> = HashMap::new();
        let chain = compute_update_chain("foo", &deps).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_compute_update_chain_no_dependents() {
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["unrelated".to_string()]);
        let chain = compute_update_chain("foo", &deps).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_compute_update_chain_single_dependent() {
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        let chain = compute_update_chain("lib-x", &deps).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].repo, "repo-a");
        assert_eq!(chain[0].inputs, vec!["lib-x".to_string()]);
    }

    #[test]
    fn test_compute_update_chain_transitive_deps() {
        // lib-x → repo-a → repo-b (repo-a depends on lib-x, repo-b depends on repo-a)
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        deps.insert("repo-b".to_string(), vec!["repo-a".to_string()]);

        let chain = compute_update_chain("lib-x", &deps).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].repo, "repo-a");
        assert_eq!(chain[0].inputs, vec!["lib-x".to_string()]);
        assert_eq!(chain[1].repo, "repo-b");
        assert_eq!(chain[1].inputs, vec!["repo-a".to_string()]);
    }

    #[test]
    fn test_compute_update_chain_diamond_dependency() {
        // lib-x → repo-a, lib-x → repo-b, repo-a → repo-c, repo-b → repo-c
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        deps.insert("repo-b".to_string(), vec!["lib-x".to_string()]);
        deps.insert("repo-c".to_string(), vec!["repo-a".to_string(), "repo-b".to_string()]);

        let chain = compute_update_chain("lib-x", &deps).unwrap();
        assert_eq!(chain.len(), 3);

        // repo-a and repo-b must come before repo-c
        let positions: HashMap<&str, usize> = chain.iter().enumerate()
            .map(|(i, s)| (s.repo.as_str(), i))
            .collect();
        assert!(positions["repo-a"] < positions["repo-c"]);
        assert!(positions["repo-b"] < positions["repo-c"]);

        // repo-c should list both repo-a and repo-b as inputs
        let repo_c_step = chain.iter().find(|s| s.repo == "repo-c").unwrap();
        assert!(repo_c_step.inputs.contains(&"repo-a".to_string()));
        assert!(repo_c_step.inputs.contains(&"repo-b".to_string()));
    }

    #[test]
    fn test_compute_update_chain_cycle_detection() {
        // A → B → A (cycle)
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["repo-b".to_string()]);
        deps.insert("repo-b".to_string(), vec!["repo-a".to_string()]);

        let result = compute_update_chain("repo-a", &deps);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cycle detected"), "expected cycle error, got: {err_msg}");
    }

    #[test]
    fn test_compute_update_chain_multiple_inputs_per_repo() {
        // repo-a depends on both lib-x and lib-y; only lib-x changed
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string(), "lib-y".to_string()]);

        let chain = compute_update_chain("lib-x", &deps).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].repo, "repo-a");
        assert_eq!(chain[0].inputs, vec!["lib-x".to_string()]);
    }

    #[test]
    fn test_compute_update_chain_changed_repo_not_in_deps() {
        // The changed repo itself isn't listed as anyone's dependency
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["other".to_string()]);
        let chain = compute_update_chain("nobody-uses-this", &deps).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_compute_update_chain_deep_chain() {
        // Linear chain: root → a → b → c → d
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["root".to_string()]);
        deps.insert("b".to_string(), vec!["a".to_string()]);
        deps.insert("c".to_string(), vec!["b".to_string()]);
        deps.insert("d".to_string(), vec!["c".to_string()]);

        let chain = compute_update_chain("root", &deps).unwrap();
        assert_eq!(chain.len(), 4);
        assert_eq!(chain[0].repo, "a");
        assert_eq!(chain[1].repo, "b");
        assert_eq!(chain[2].repo, "c");
        assert_eq!(chain[3].repo, "d");
    }

    #[test]
    fn test_compute_update_chain_self_dependency_not_triggered() {
        // repo-a depends on itself (odd but shouldn't panic)
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["repo-a".to_string()]);
        // "lib-x" has no dependents
        let chain = compute_update_chain("lib-x", &deps).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_update_step_inputs_reflect_transitive_updates() {
        // a depends on root, b depends on root AND a
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["root".to_string()]);
        deps.insert("b".to_string(), vec!["root".to_string(), "a".to_string()]);

        let chain = compute_update_chain("root", &deps).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].repo, "a");
        assert_eq!(chain[0].inputs, vec!["root".to_string()]);
        // b should see both root and a as updated inputs
        assert_eq!(chain[1].repo, "b");
        assert!(chain[1].inputs.contains(&"root".to_string()));
        assert!(chain[1].inputs.contains(&"a".to_string()));
    }

    #[test]
    fn test_compute_update_chain_partial_dependency_only_changed_inputs() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["lib-x".to_string(), "lib-y".to_string()]);
        deps.insert("b".to_string(), vec!["lib-y".to_string()]);

        let chain = compute_update_chain("lib-x", &deps).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].repo, "a");
        assert_eq!(chain[0].inputs, vec!["lib-x".to_string()]);
    }

    #[test]
    fn test_compute_update_chain_wide_fan_out() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["root".to_string()]);
        deps.insert("b".to_string(), vec!["root".to_string()]);
        deps.insert("c".to_string(), vec!["root".to_string()]);
        deps.insert("d".to_string(), vec!["root".to_string()]);

        let chain = compute_update_chain("root", &deps).unwrap();
        assert_eq!(chain.len(), 4);
        let repos: Vec<&str> = chain.iter().map(|s| s.repo.as_str()).collect();
        assert!(repos.contains(&"a"));
        assert!(repos.contains(&"b"));
        assert!(repos.contains(&"c"));
        assert!(repos.contains(&"d"));
        for step in &chain {
            assert_eq!(step.inputs, vec!["root".to_string()]);
        }
    }

    #[test]
    fn test_compute_update_chain_changed_is_not_in_output() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["root".to_string()]);

        let chain = compute_update_chain("root", &deps).unwrap();
        for step in &chain {
            assert_ne!(step.repo, "root", "changed repo should not appear as an update step");
        }
    }

    #[test]
    fn test_compute_update_chain_three_level_cycle_detection() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["b".to_string()]);
        deps.insert("b".to_string(), vec!["c".to_string()]);
        deps.insert("c".to_string(), vec!["a".to_string()]);

        let result = compute_update_chain("a", &deps);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cycle detected"));
    }

    #[test]
    fn test_compute_update_chain_all_empty() {
        let deps: HashMap<String, Vec<String>> = HashMap::new();
        let chain = compute_update_chain_all(&deps).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_compute_update_chain_all_single_repo() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["lib-x".to_string()]);
        let chain = compute_update_chain_all(&deps).unwrap();
        // a is the only repo with deps; lib-x has no deps so no step emitted
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].repo, "a");
        assert_eq!(chain[0].inputs, vec!["lib-x".to_string()]);
    }

    #[test]
    fn test_compute_update_chain_all_topological_order() {
        // lib-x → repo-a → repo-b
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        deps.insert("repo-b".to_string(), vec!["repo-a".to_string()]);

        let chain = compute_update_chain_all(&deps).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].repo, "repo-a");
        assert_eq!(chain[1].repo, "repo-b");
    }

    #[test]
    fn test_compute_update_chain_all_deduplicates_work() {
        // Same DAG as the diamond case: lib-x is shared by repo-a and repo-b,
        // both feed into repo-c. compute_update_chain_all should emit each repo
        // exactly once.
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        deps.insert("repo-b".to_string(), vec!["lib-x".to_string()]);
        deps.insert(
            "repo-c".to_string(),
            vec!["repo-a".to_string(), "repo-b".to_string()],
        );

        let chain = compute_update_chain_all(&deps).unwrap();
        assert_eq!(chain.len(), 3);
        let repos: Vec<&str> = chain.iter().map(|s| s.repo.as_str()).collect();
        // All three repos appear, exactly once
        assert!(repos.contains(&"repo-a"));
        assert!(repos.contains(&"repo-b"));
        assert!(repos.contains(&"repo-c"));
        let positions: HashMap<&str, usize> = chain
            .iter()
            .enumerate()
            .map(|(i, s)| (s.repo.as_str(), i))
            .collect();
        assert!(positions["repo-a"] < positions["repo-c"]);
        assert!(positions["repo-b"] < positions["repo-c"]);
    }

    #[test]
    fn test_compute_update_chain_all_full_inputs_per_repo() {
        // repo-c depends on both lib-x and repo-a; in --all mode it should
        // update both inputs, not just one.
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        deps.insert(
            "repo-c".to_string(),
            vec!["lib-x".to_string(), "repo-a".to_string()],
        );

        let chain = compute_update_chain_all(&deps).unwrap();
        let repo_c_step = chain.iter().find(|s| s.repo == "repo-c").unwrap();
        assert_eq!(repo_c_step.inputs.len(), 2);
        assert!(repo_c_step.inputs.contains(&"lib-x".to_string()));
        assert!(repo_c_step.inputs.contains(&"repo-a".to_string()));
    }

    #[test]
    fn test_compute_update_chain_all_cycle_detection() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), vec!["b".to_string()]);
        deps.insert("b".to_string(), vec!["a".to_string()]);
        let result = compute_update_chain_all(&deps);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cycle detected"));
    }

    #[test]
    fn test_compute_update_chain_all_skips_pure_leaves() {
        // lib-x is a leaf with no flake_deps entry — it shouldn't get a step
        let mut deps = HashMap::new();
        deps.insert("repo-a".to_string(), vec!["lib-x".to_string()]);
        let chain = compute_update_chain_all(&deps).unwrap();
        for step in &chain {
            assert_ne!(step.repo, "lib-x");
        }
    }
}
