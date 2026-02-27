mod config;
mod display;
mod provider;
mod sync;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tend", version, about = "Workspace repository manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Clone missing repos into the workspace
    Sync {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only sync a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// Suppress per-repo output, only show summary
        #[arg(long)]
        quiet: bool,
    },

    /// Show repo status (clean/dirty/missing/unknown)
    Status {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only show status for a specific workspace
        #[arg(long)]
        workspace: Option<String>,
    },

    /// List configured repos
    List {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only list repos for a specific workspace
        #[arg(long)]
        workspace: Option<String>,
    },

    /// Discover repos from a GitHub org
    Discover {
        /// GitHub org name
        org: String,

        /// Provider (only github supported)
        #[arg(long, default_value = "github")]
        provider: String,
    },

    /// Generate a starter config file
    Init,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Sync {
            config: config_path,
            workspace: ws_filter,
            quiet,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws).await?;
                let (cloned, present) = sync::sync_repos(ws, &repos, quiet).await?;
                if !quiet || cloned > 0 {
                    display::print_sync_summary(&ws.name, cloned, present);
                }
            }
        }

        Commands::Status {
            config: config_path,
            workspace: ws_filter,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws).await?;
                let entries = sync::check_status(ws, &repos).await?;
                display::print_status(&ws.name, &entries);
            }
        }

        Commands::List {
            config: config_path,
            workspace: ws_filter,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws).await?;
                display::print_repo_list(&ws.name, &repos);
            }
        }

        Commands::Discover { org, provider: _ } => {
            let repos = provider::discover_github_repos(&org).await?;
            display::print_discover_results(&org, &repos);
        }

        Commands::Init => {
            let path = config::Config::default_path();
            if path.exists() {
                anyhow::bail!("config already exists at {}", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let content = config::generate_starter_config();
            std::fs::write(&path, &content)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("config written to {}", path.display());
        }
    }

    Ok(())
}

fn load_config(path: Option<&std::path::Path>) -> Result<config::Config> {
    let config_path = match path {
        Some(p) => p.to_path_buf(),
        None => config::Config::default_path(),
    };
    config::Config::load(&config_path)
}

fn filter_workspaces<'a>(
    workspaces: &'a [config::Workspace],
    filter: Option<&str>,
) -> Vec<&'a config::Workspace> {
    match filter {
        Some(name) => workspaces.iter().filter(|ws| ws.name == name).collect(),
        None => workspaces.iter().collect(),
    }
}
