mod audit;
mod cache;
mod ci_trim;
mod config;
mod daemon;
mod display;
mod flake;
mod flake_lock;
mod git;
mod github;
mod head_cache;
mod planner;
mod provider;
mod release_swarm;
mod sync;
mod watch;
mod watch_cache;

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

        /// Bypass discovery cache and always hit the GitHub API
        #[arg(long)]
        refresh: bool,
    },

    /// Fast-forward every clean repo in the workspace (git pull --ff-only)
    Pull {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only pull a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// Suppress per-repo output, only show summary
        #[arg(long)]
        quiet: bool,

        /// Bypass discovery cache and always hit the GitHub API
        #[arg(long)]
        refresh: bool,
    },

    /// Show repo status (clean/dirty/missing/unknown)
    Status {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only show status for a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Bypass discovery cache and always hit the GitHub API
        #[arg(long)]
        refresh: bool,
    },

    /// List configured repos
    List {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only list repos for a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Bypass discovery cache and always hit the GitHub API
        #[arg(long)]
        refresh: bool,
    },

    /// Discover repos from a GitHub org
    Discover {
        /// GitHub org name
        org: String,

        /// Provider (only github supported)
        #[arg(long, default_value = "github")]
        provider: String,
    },

    /// Run as a persistent daemon — sync + fetch on interval
    Daemon {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only sync a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// Sync interval in seconds
        #[arg(long, default_value = "300")]
        interval: u64,

        /// Fetch existing repos (git fetch --all)
        #[arg(long, default_value = "true")]
        fetch: bool,

        /// Suppress per-repo output
        #[arg(long)]
        quiet: bool,

        /// Path to file containing GitHub token (for launchd environments)
        #[arg(long)]
        github_token_file: Option<PathBuf>,
    },

    /// Run watch cycle once (detect new versions)
    Watch {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only watch a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Bypass discovery cache
        #[arg(long)]
        refresh: bool,
    },

    /// Generate a starter config file
    Init,

    /// View the structured audit log
    AuditLog {
        /// Filter by event type
        #[arg(long)]
        event: Option<String>,

        /// Show last N entries
        #[arg(long, default_value = "20")]
        last: usize,

        /// Output raw JSON lines
        #[arg(long)]
        json: bool,

        /// Filter events since this date (YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
    },

    /// Propagate nix flake update through the dependency chain
    FlakeUpdate {
        /// Repo that was just pushed (trigger). Mutually exclusive with --all.
        #[arg(long, conflicts_with = "all")]
        changed: Option<String>,

        /// Treat every repo with flake_deps as changed; rebuild the union DAG and
        /// execute each affected repo exactly once across every workspace that
        /// has flake_deps configured.
        #[arg(long, conflicts_with = "changed")]
        all: bool,

        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Show the chain without executing
        #[arg(long)]
        dry_run: bool,

        /// Suppress per-step output
        #[arg(long)]
        quiet: bool,

        /// Skip git pull --ff-only before each nix flake update
        #[arg(long)]
        no_pull: bool,

        /// Fail (instead of cloning) when a chain repo isn't on disk
        #[arg(long)]
        no_clone: bool,

        /// Skip the flake.lock vs upstream-HEAD pre-flight check. By default,
        /// steps whose lockfile already matches the upstream HEAD on every
        /// dep are dropped from the chain without running nix flake update.
        #[arg(long)]
        no_preflight: bool,
    },

    /// Run flake-update --all continuously with exponential backoff.
    /// Idempotent: cycles where every workspace is converged do no work.
    FlakeUpdateDaemon {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Minimum sleep between cycles, in seconds (reset interval after work).
        #[arg(long, default_value = "60")]
        min_interval: u64,

        /// Maximum sleep between cycles when converged, in seconds.
        #[arg(long, default_value = "3600")]
        max_interval: u64,

        /// Suppress per-step output
        #[arg(long)]
        quiet: bool,

        /// Path to file containing GitHub token (for launchd environments)
        #[arg(long)]
        github_token_file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Sync {
            config: config_path,
            workspace: ws_filter,
            quiet,
            refresh,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws, refresh).await?;
                let (cloned, present) = sync::sync_repos(ws, &repos, quiet).await?;
                if !quiet || cloned > 0 {
                    display::print_sync_summary(&ws.name, cloned, present);
                }
            }
        }

        Commands::Pull {
            config: config_path,
            workspace: ws_filter,
            quiet,
            refresh,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws, refresh).await?;
                let summary = sync::pull_repos(ws, &repos, quiet).await?;
                display::print_pull_summary(&ws.name, &summary);
            }
        }

        Commands::Status {
            config: config_path,
            workspace: ws_filter,
            refresh,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws, refresh).await?;
                let entries = sync::check_status(ws, &repos).await?;
                display::print_status(&ws.name, &entries);
            }
        }

        Commands::List {
            config: config_path,
            workspace: ws_filter,
            refresh,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws, refresh).await?;
                display::print_repo_list(&ws.name, &repos);
            }
        }

        Commands::Discover { org, provider: _ } => {
            let repos = provider::discover_github_repos(&org).await?;
            display::print_discover_results(&org, &repos);
        }

        Commands::FlakeUpdate {
            changed,
            all,
            config: config_path,
            workspace: ws_filter,
            dry_run,
            quiet,
            no_pull,
            no_clone,
            no_preflight,
        } => {
            if !all && changed.is_none() {
                anyhow::bail!("flake-update requires either --changed <repo> or --all");
            }

            let cfg = load_config(config_path.as_deref())?;
            let opts = flake::ExecOptions {
                dry_run,
                quiet,
                auto_clone: !no_clone,
                pull_before_update: !no_pull,
                retry_on_push_reject: true,
            };

            let github_client = github::HttpGitHubClient::new()?;
            let upstream = head_cache::CachedGitHubHead::new(&github_client);
            let use_preflight = !no_preflight;

            let audit_log = audit::AuditLog::default_path();
            let mut summary = flake::ExecSummary::default();

            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                if ws.flake_deps.is_empty() {
                    continue;
                }

                let (label, chain) = if all {
                    ("(all)".to_string(), flake::compute_update_chain_all(&ws.flake_deps)?)
                } else {
                    let trigger = changed.as_deref().expect("validated above");
                    (
                        trigger.to_string(),
                        flake::compute_update_chain(trigger, &ws.flake_deps)?,
                    )
                };

                if chain.is_empty() {
                    if !quiet {
                        println!("{}: no work for {}", ws.name, label);
                    }
                    continue;
                }

                let (chain, dropped) = if use_preflight {
                    flake::filter_to_divergent(ws, chain, Some(&upstream)).await?
                } else {
                    (chain, Vec::new())
                };

                if !quiet && !dropped.is_empty() {
                    println!(
                        "{}: pre-flight skipped {} converged step(s)",
                        ws.name,
                        dropped.len()
                    );
                }
                if chain.is_empty() {
                    if !quiet {
                        println!("{}: converged — no work to do", ws.name);
                    }
                    audit_log.log(
                        "flake_update_converged",
                        serde_json::json!({ "workspace": ws.name }),
                    );
                    continue;
                }

                if !quiet {
                    display::print_flake_chain_header(&ws.name, &label, &chain);
                }
                let ws_summary = flake::execute_update_chain(ws, &chain, opts)?;
                summary.updated += ws_summary.updated;
                summary.no_change += ws_summary.no_change;
                summary.skipped += ws_summary.skipped;

                audit_log.log(
                    "flake_update_workspace_complete",
                    serde_json::json!({
                        "workspace": ws.name,
                        "updated": ws_summary.updated,
                        "no_change": ws_summary.no_change,
                        "skipped": ws_summary.skipped,
                    }),
                );
                if !quiet {
                    display::print_flake_chain_complete(chain.len());
                }
            }

            if all && !quiet {
                println!(
                    "\nsummary: {} updated, {} no-change, {} skipped",
                    summary.updated, summary.no_change, summary.skipped
                );
            }
        }

        Commands::FlakeUpdateDaemon {
            config: config_path,
            workspace: ws_filter,
            min_interval,
            max_interval,
            quiet,
            github_token_file,
        } => {
            if let Some(ref token_path) = github_token_file {
                let token = std::fs::read_to_string(token_path)
                    .with_context(|| format!("reading token from {}", token_path.display()))?;
                std::env::set_var("GITHUB_TOKEN", token.trim());
            }

            run_flake_update_daemon(
                config_path,
                ws_filter,
                min_interval,
                max_interval,
                quiet,
            )
            .await?;
        }

        Commands::Watch {
            config: config_path,
            workspace: ws_filter,
            refresh: _refresh,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            let audit_log = audit::AuditLog::default_path();
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                if let Some(ref watch_cfg) = ws.watch {
                    if watch_cfg.enable {
                        let gh = github::HttpGitHubClient::new()?;
                        let cache_store = watch_cache::FsWatchStateStore;
                        let matrix_appender = watch::TomlMatrixAppender;
                        let git_ops = git::SystemGitOps;

                        let summary = watch::run_watch_cycle(
                            ws, false, &gh, &cache_store, &matrix_appender, &git_ops,
                            &audit_log,
                        ).await?;
                        display::print_watch_summary(&ws.name, &summary);
                    }
                }
            }
        }

        Commands::AuditLog {
            event,
            last,
            json,
            since,
        } => {
            let audit_log = audit::AuditLog::default_path();
            let path = audit_log.path();
            if !path.exists() {
                println!("no audit log found at {}", path.display());
                return Ok(());
            }
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;

            let mut entries: Vec<serde_json::Value> = content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();

            // Filter by event type
            if let Some(ref evt) = event {
                entries.retain(|e| e.get("event").and_then(|v| v.as_str()) == Some(evt));
            }

            // Filter by since date
            if let Some(ref since_date) = since {
                entries.retain(|e| {
                    e.get("timestamp")
                        .and_then(|v| v.as_str())
                        .is_some_and(|ts| ts >= since_date.as_str())
                });
            }

            // Take last N entries
            let start = entries.len().saturating_sub(last);
            let entries = &entries[start..];

            if json {
                for entry in entries {
                    println!("{}", serde_json::to_string(entry).unwrap_or_default());
                }
            } else {
                for entry in entries {
                    let ts = entry
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");
                    let evt = entry
                        .get("event")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?");

                    // Collect data fields (everything except timestamp and event)
                    let data_fields: Vec<String> = entry
                        .as_object()
                        .map(|obj| {
                            obj.iter()
                                .filter(|(k, _)| *k != "timestamp" && *k != "event")
                                .map(|(k, v)| {
                                    let val = match v {
                                        serde_json::Value::String(s) => s.clone(),
                                        serde_json::Value::Null => "null".to_string(),
                                        other => other.to_string(),
                                    };
                                    format!("{k}={val}")
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    println!("[{ts}] {evt}  {}", data_fields.join(" "));
                }
                println!("\n{} entries (from {})", entries.len(), path.display());
            }
        }

        Commands::Daemon {
            config: config_path,
            workspace: ws_filter,
            interval,
            fetch,
            quiet,
            github_token_file,
        } => {
            // In launchd/systemd environments, env vars may not be inherited.
            // Read the token from a file and set GITHUB_TOKEN for provider discovery.
            if let Some(ref token_path) = github_token_file {
                let token = std::fs::read_to_string(token_path)
                    .with_context(|| format!("reading token from {}", token_path.display()))?;
                std::env::set_var("GITHUB_TOKEN", token.trim());
            }

            daemon::run(daemon::DaemonOpts {
                config: config_path,
                workspace: ws_filter,
                interval,
                fetch,
                quiet,
            })
            .await?;
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
            let content = config::generate_starter_config()?;
            std::fs::write(&path, &content)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("config written to {}", path.display());
        }
    }

    Ok(())
}

pub(crate) fn load_config(path: Option<&std::path::Path>) -> Result<config::Config> {
    let config_path = match path {
        Some(p) => p.to_path_buf(),
        None => config::Config::default_path(),
    };
    config::Config::load(&config_path)
}

async fn run_flake_update_daemon(
    config_path: Option<PathBuf>,
    ws_filter: Option<String>,
    min_interval: u64,
    max_interval: u64,
    quiet: bool,
) -> Result<()> {
    let min = min_interval.max(1);
    let max = max_interval.max(min);
    let mut interval = min;
    let audit_log = audit::AuditLog::default_path();

    if !quiet {
        println!(
            "flake-update daemon starting (min={min}s max={max}s). Ctrl-C to stop."
        );
    }

    loop {
        let cycle_start = std::time::Instant::now();
        audit_log.log(
            "flake_update_cycle_start",
            serde_json::json!({ "interval_secs": interval }),
        );

        match run_flake_update_cycle(config_path.as_deref(), ws_filter.as_deref(), quiet).await
        {
            Ok(summary) => {
                let duration_ms = cycle_start.elapsed().as_millis() as u64;
                audit_log.log(
                    "flake_update_cycle_complete",
                    serde_json::json!({
                        "duration_ms": duration_ms,
                        "updated": summary.updated,
                        "no_change": summary.no_change,
                        "skipped": summary.skipped,
                    }),
                );
                if summary.work() > 0 {
                    interval = min;
                } else {
                    interval = (interval.saturating_mul(2)).min(max);
                }
            }
            Err(e) => {
                eprintln!("flake-update daemon cycle failed: {e:#}");
                audit_log.log(
                    "flake_update_cycle_error",
                    serde_json::json!({ "error": format!("{e:#}") }),
                );
                interval = min;
            }
        }

        if !quiet {
            println!("flake-update daemon sleeping {interval}s");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

async fn run_flake_update_cycle(
    config_path: Option<&std::path::Path>,
    ws_filter: Option<&str>,
    quiet: bool,
) -> Result<flake::ExecSummary> {
    let cfg = load_config(config_path)?;
    let opts = flake::ExecOptions {
        dry_run: false,
        quiet,
        auto_clone: true,
        pull_before_update: true,
        retry_on_push_reject: true,
    };
    let github_client = github::HttpGitHubClient::new()?;
    let upstream = head_cache::CachedGitHubHead::new(&github_client);

    let mut summary = flake::ExecSummary::default();
    for ws in filter_workspaces(&cfg.workspaces, ws_filter) {
        if ws.flake_deps.is_empty() {
            continue;
        }
        let chain = flake::compute_update_chain_all(&ws.flake_deps)?;
        if chain.is_empty() {
            continue;
        }
        let (chain, _dropped) =
            flake::filter_to_divergent(ws, chain, Some(&upstream)).await?;
        if chain.is_empty() {
            continue;
        }
        if !quiet {
            display::print_flake_chain_header(&ws.name, "(daemon cycle)", &chain);
        }
        let ws_summary = flake::execute_update_chain(ws, &chain, opts)?;
        summary.updated += ws_summary.updated;
        summary.no_change += ws_summary.no_change;
        summary.skipped += ws_summary.skipped;
    }
    Ok(summary)
}

pub(crate) fn filter_workspaces<'a>(
    workspaces: &'a [config::Workspace],
    filter: Option<&str>,
) -> Vec<&'a config::Workspace> {
    match filter {
        Some(name) => workspaces.iter().filter(|ws| ws.name == name).collect(),
        None => workspaces.iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_workspaces_no_filter_returns_all() {
        let workspaces = vec![
            config::Workspace::test_default("ws-a"),
            config::Workspace::test_default("ws-b"),
            config::Workspace::test_default("ws-c"),
        ];
        let filtered = filter_workspaces(&workspaces, None);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn test_filter_workspaces_with_matching_name() {
        let workspaces = vec![
            config::Workspace::test_default("ws-a"),
            config::Workspace::test_default("ws-b"),
            config::Workspace::test_default("ws-c"),
        ];
        let filtered = filter_workspaces(&workspaces, Some("ws-b"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "ws-b");
    }

    #[test]
    fn test_filter_workspaces_with_nonexistent_name() {
        let workspaces = vec![
            config::Workspace::test_default("ws-a"),
            config::Workspace::test_default("ws-b"),
        ];
        let filtered = filter_workspaces(&workspaces, Some("ws-z"));
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_workspaces_empty_input() {
        let workspaces: Vec<config::Workspace> = vec![];
        let filtered = filter_workspaces(&workspaces, None);
        assert!(filtered.is_empty());

        let filtered = filter_workspaces(&workspaces, Some("anything"));
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_workspaces_duplicate_names() {
        let workspaces = vec![
            config::Workspace::test_default("dup"),
            config::Workspace::test_default("dup"),
        ];
        let filtered = filter_workspaces(&workspaces, Some("dup"));
        assert_eq!(filtered.len(), 2, "should return all matching entries");
    }

    #[test]
    fn test_load_config_nonexistent_path() {
        let result = load_config(Some(std::path::Path::new("/nonexistent/tend.yaml")));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_config_valid_file() {
        let dir = std::env::temp_dir().join("tend-main-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("valid-config.yaml");
        std::fs::write(&path, "workspaces:\n  - name: test\n    base_dir: /tmp\n").unwrap();
        let cfg = load_config(Some(&path)).unwrap();
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.workspaces[0].name, "test");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_config_invalid_yaml_returns_error() {
        let dir = std::env::temp_dir().join("tend-main-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad-main-config.yaml");
        std::fs::write(&path, "not: [valid: yaml: here").unwrap();
        let result = load_config(Some(&path));
        assert!(result.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_filter_workspaces_preserves_order() {
        let workspaces = vec![
            config::Workspace::test_default("charlie"),
            config::Workspace::test_default("alpha"),
            config::Workspace::test_default("bravo"),
        ];
        let filtered = filter_workspaces(&workspaces, None);
        assert_eq!(filtered[0].name, "charlie");
        assert_eq!(filtered[1].name, "alpha");
        assert_eq!(filtered[2].name, "bravo");
    }

    #[test]
    fn test_filter_workspaces_returns_references() {
        let workspaces = vec![config::Workspace::test_default("ws-a")];
        let filtered = filter_workspaces(&workspaces, None);
        assert!(std::ptr::eq(filtered[0], &workspaces[0]));
    }

    #[test]
    fn test_load_config_with_multiple_workspaces() {
        let dir = std::env::temp_dir().join("tend-main-test-multi");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("multi-config.yaml");
        std::fs::write(&path, "workspaces:\n  - name: a\n    base_dir: /a\n  - name: b\n    base_dir: /b\n").unwrap();
        let cfg = load_config(Some(&path)).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
