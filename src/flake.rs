use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Command;

use crate::config::Workspace;
use crate::display;

/// A single step in the update chain.
#[derive(Debug)]
pub struct UpdateStep {
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
pub fn compute_update_chain(
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
                // Only count edges from affected repos or the changed repo
                if affected.contains(dep.as_str()) || dep == changed {
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
        let deps = flake_deps.get(repo).unwrap();
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

/// Execute the update chain: for each step, run nix flake update, commit, push.
pub fn execute_update_chain(
    workspace: &Workspace,
    chain: &[UpdateStep],
    dry_run: bool,
    quiet: bool,
) -> Result<()> {
    let base_dir = workspace.resolved_base_dir()?;
    let total = chain.len();

    for (i, step) in chain.iter().enumerate() {
        let step_num = i + 1;
        let repo_path = base_dir.join(&step.repo);

        if !repo_path.exists() {
            bail!("repo directory does not exist: {}", repo_path.display());
        }

        if !quiet {
            display::print_flake_step_start(step_num, total, &step.repo, &step.inputs);
        }

        if dry_run {
            if !quiet {
                display::print_flake_step_dry_run();
            }
            continue;
        }

        // Check for clean working tree
        ensure_clean(&repo_path)
            .with_context(|| format!("{} has uncommitted changes", step.repo))?;

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
            if !quiet {
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

        if !quiet {
            display::print_flake_step_done(&step.repo);
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
