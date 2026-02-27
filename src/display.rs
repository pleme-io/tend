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
