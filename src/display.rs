use colored::Colorize;

use crate::sync::{RepoEntry, RepoStatus};

pub fn print_status(workspace_name: &str, entries: &[RepoEntry]) {
    let clean = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Clean))
        .count();
    let dirty = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Dirty))
        .count();
    let missing = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Missing))
        .count();
    let unknown = entries
        .iter()
        .filter(|e| matches!(e.status, RepoStatus::Unknown))
        .count();

    println!("{}", format!("workspace: {workspace_name}").bold());
    println!();

    for entry in entries {
        let (icon, label) = match &entry.status {
            RepoStatus::Clean => ("ok".green().to_string(), "clean"),
            RepoStatus::Dirty => ("!!".yellow().to_string(), "dirty"),
            RepoStatus::Missing => ("--".red().to_string(), "missing"),
            RepoStatus::Unknown => ("??".cyan().to_string(), "unknown"),
        };
        println!("  [{icon}] {:<40} {label}", entry.name);
    }

    println!();
    println!(
        "  {} clean, {} dirty, {} missing, {} unknown",
        clean.to_string().green(),
        dirty.to_string().yellow(),
        missing.to_string().red(),
        unknown.to_string().cyan(),
    );
}

pub fn print_sync_summary(workspace_name: &str, cloned: usize, present: usize) {
    if cloned == 0 {
        println!(
            "{}: all {} repos present",
            workspace_name.bold(),
            present
        );
    } else {
        println!(
            "{}: cloned {} new, {} already present",
            workspace_name.bold(),
            cloned.to_string().green(),
            present
        );
    }
}

pub fn print_repo_list(workspace_name: &str, repos: &[String]) {
    println!("{} ({} repos):", workspace_name.bold(), repos.len());
    for repo in repos {
        println!("  {repo}");
    }
}

pub fn print_discover_results(org: &str, repos: &[String]) {
    println!(
        "discovered {} repos in {}:",
        repos.len().to_string().green(),
        org.bold()
    );
    for repo in repos {
        println!("  {repo}");
    }
}

pub fn print_daemon_cycle_start(cycle: u64) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!(
        "[{}] {} cycle {}",
        now,
        "daemon:".bold(),
        cycle.to_string().cyan()
    );
}

pub fn print_daemon_cycle_done(cycle: u64, workspaces: usize) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!(
        "[{}] {} cycle {} done ({} workspaces)",
        now,
        "daemon:".bold(),
        cycle,
        workspaces.to_string().green()
    );
}

pub fn print_fetch_summary(workspace_name: &str, fetched: usize, skipped: usize) {
    if fetched == 0 && skipped == 0 {
        return;
    }
    println!(
        "{}: fetched {}, skipped {}",
        workspace_name.bold(),
        fetched.to_string().green(),
        skipped.to_string().yellow(),
    );
}

pub fn print_daemon_error(workspace_name: &str, err: &anyhow::Error) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    eprintln!(
        "[{}] {}: {} {}",
        now,
        "error".red().bold(),
        workspace_name,
        err
    );
}

pub fn print_daemon_sleeping(interval: u64) {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    println!(
        "[{}] {} sleeping {}s",
        now,
        "daemon:".bold(),
        interval
    );
}

pub fn print_flake_chain_header(workspace_name: &str, changed: &str, steps: &[crate::flake::UpdateStep]) {
    println!("{}", format!("workspace: {workspace_name}").bold());
    println!("  changed: {}", changed.cyan());
    println!("  chain ({} steps):", steps.len().to_string().green());
    for (i, step) in steps.iter().enumerate() {
        println!(
            "    {}. {} → nix flake update {}",
            i + 1,
            step.repo.bold(),
            step.inputs.join(" ")
        );
    }
    println!();
}

pub fn print_flake_step_start(step: usize, total: usize, repo: &str, inputs: &[String]) {
    println!(
        "  [{}/{}] {} nix flake update {}",
        step,
        total,
        repo.bold(),
        inputs.join(" ")
    );
}

pub fn print_flake_step_done(repo: &str) {
    println!("  [{}] {} committed and pushed", "ok".green(), repo);
}

pub fn print_flake_step_dry_run() {
    println!("  [{}] (dry-run, skipped)", ">>".yellow());
}

pub fn print_flake_step_no_changes(repo: &str) {
    println!("  [{}] {} flake.lock unchanged", "==".cyan(), repo);
}

pub fn print_flake_chain_complete(updated: usize) {
    if updated == 0 {
        println!("\n  {}", "no repos needed updating".cyan());
    } else {
        println!(
            "\n  {} {} updated",
            "done:".green().bold(),
            updated.to_string().green()
        );
    }
}
