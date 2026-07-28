mod ai_cron;
mod ai_executor;
mod ai_flow;
mod ai_lisp;
mod ai_models;
mod ai_planner;
mod audit;
mod cache;
mod ci_trim;
mod config;
mod daemon;
mod display;
mod kanshou_state;
// Typed failure classification for a reconcile cycle. See src/failure.rs --
// it exists because a String reason produced three contradictory readings of
// one log on 2026-07-28, and because "the host was asleep" must not count as
// residue the way "the credential is gone" does.
mod failure;
mod flake;
mod flake_lock;
mod git;
mod github;
mod drift;
mod head_cache;
mod host_health;
mod jobs;
mod nixpkgs_align;
mod placeholder;
mod anomaly;
mod planner;
mod prebuild;
mod prebuild_cache;
mod reconcile;
mod report;
mod provider;
mod release_swarm;
mod release_swarm_http;
mod sync;
mod watch;
mod watch_cache;

#[cfg(feature = "operator")]
mod operator;

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

    /// Align every clean repo's nixpkgs to follow substrate's pinned rev (the
    /// one true version) so identical software builds once and is a cache hit
    /// fleet-wide. Converts flake.nix to `nixpkgs.follows = "substrate/nixpkgs"`,
    /// pulls the pinned substrate, commits + pushes. Idempotent; never touches a
    /// dirty repo. The standing enforcement of the substrate-anchored nixpkgs.
    NixpkgsAlign {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only align a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,
    },

    /// Mark or unmark a directory as a tend placeholder. Marked
    /// dirs are skipped by reconcile (sync / pull / fetch) — useful
    /// for scratch dirs, symlinks managed by other tools, or
    /// reserved names where you don't want tend to auto-clone.
    Placeholder {
        /// Directory path to mark. Resolved relative to current dir
        /// if not absolute. Created if it doesn't exist (so the
        /// marker file has a home).
        path: PathBuf,

        /// Remove the marker instead of adding it.
        #[arg(long)]
        unmark: bool,
    },

    /// Show operator-actionable drift + suggested fixes. Like
    /// `tend report` but framed around "what's wrong + what to do
    /// about it." Reads the drift log written by reconcile.
    Doctor {
        /// Path to the drift log. Defaults to
        /// $XDG_DATA_HOME/tend/drift-events.jsonl.
        #[arg(long)]
        log: Option<PathBuf>,
    },

    /// Add an on-disk repo to a workspace's extra_repos list so
    /// future reconciles include it. Useful for local-only repos
    /// or repos the org discovery doesn't see (archived, private
    /// without token, etc.).
    Adopt {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Workspace name to adopt into.
        workspace: String,

        /// Repo name to adopt (must match an existing dir under
        /// the workspace's base_dir).
        repo: String,
    },

    /// Report on recent scheduler activity. Reads the
    /// scheduler-transitions.jsonl audit log written by daemon +
    /// reconcile cycles and prints a rollup: per-kind counts, recent
    /// failures, and jobs currently stuck in non-terminal phases.
    Report {
        /// Path to the transition log. Default:
        /// $XDG_DATA_HOME/tend/scheduler-transitions.jsonl
        #[arg(long)]
        log: Option<PathBuf>,

        /// Look-back window in hours. Default: 168 (7 days).
        #[arg(long)]
        window_hours: Option<i64>,
    },

    /// Reconcile workspace via the shigoto scheduler. Per-repo
    /// PullRepoJob runs through InProcessScheduler; outputs flow
    /// through an InMemorySink<PullOutcome>; prints a typed
    /// ReconcileReceipt. The destination shape that replaces
    /// `tend pull`'s batch summary with per-Job typed outcomes.
    Reconcile {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only reconcile a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// Bypass discovery cache and always hit the GitHub API
        #[arg(long)]
        refresh: bool,

        /// Maximum concurrent `git pull` processes. Bounds the
        /// scheduler's per-kind Budget for `tend.pull-repo`. Default
        /// is 16 — high enough to saturate a typical broadband link
        /// without exhausting OS file handles or SSH connection
        /// multiplexers.
        #[arg(long, default_value_t = reconcile::DEFAULT_MAX_INFLIGHT_PULL)]
        max_inflight: u32,
    },

    /// Show repo status (clean/dirty/stuck/no-remote/missing/unknown)
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

        /// Emit a machine-readable JSON array of `{name, path, state}`
        /// (all filtered workspaces merged; `path` is empty for missing
        /// repos). The contract izumi's `tend-repos` board source reads.
        #[arg(long)]
        json: bool,

        /// Skip the host-health autocorrect: report orphaned watched
        /// processes (see host_health.rs) without SIGKILLing them.
        /// Detection + audit logging still run either way.
        #[arg(long)]
        no_fix: bool,
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

    /// Run as a persistent daemon — sync + pull + watch on interval. Drives
    /// the workspace toward the org's current state continuously, not on
    /// demand. The reconciler shape.
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

        /// Fast-forward clean repos every cycle (`git pull --ff-only`).
        /// Default true — this is the reconciler behavior. Accepts bare
        /// `--pull` (true), `--pull=false`, or no flag (default true).
        #[arg(
            long,
            default_value_t = true,
            num_args = 0..=1,
            default_missing_value = "true",
            require_equals = false,
            action = clap::ArgAction::Set,
        )]
        pull: bool,

        /// Plain `git fetch --all --prune` each cycle. Only takes effect
        /// when `--pull=false` (pull already fetches). Kept so a
        /// fetch-only daemon remains expressible, and so legacy launchd
        /// configs passing bare `--fetch` continue to parse.
        #[arg(
            long,
            default_value_t = true,
            num_args = 0..=1,
            default_missing_value = "true",
            require_equals = false,
            action = clap::ArgAction::Set,
        )]
        fetch: bool,

        /// Suppress per-repo output
        #[arg(long)]
        quiet: bool,

        /// Path to file containing GitHub token (for launchd environments)
        #[arg(long)]
        github_token_file: Option<PathBuf>,

        /// Maximum concurrent `git pull` processes per workspace per
        /// cycle. Bounds the shigoto scheduler's per-kind Budget for
        /// `tend.pull-repo`. Default matches `tend reconcile`.
        #[arg(long, default_value_t = reconcile::DEFAULT_MAX_INFLIGHT_PULL)]
        max_inflight: u32,
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

    /// Preview the org-level release-swarm without touching GitHub.
    /// Lists eligible repos per workspace (deny-by-default at both
    /// org and repo level).
    ReleaseSwarmPlan {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,
    },

    /// Apply the release-swarm — for each eligible repo, render the
    /// canonical release.yml and open a PR. Dry-run by default so
    /// nothing mutates without explicit opt-in.
    ReleaseSwarmApply {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// If true (default), render but skip the PR-open call.
        #[arg(long, default_value_t = true)]
        dry_run: bool,
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

    /// Build every workspace flake repo whose HEAD has moved since
    /// last cycle, optionally pushing each closure to an Attic cache.
    /// One-shot; pair with `prebuild-daemon` for continuous fill.
    Prebuild {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Suppress per-repo log lines (audit log still written)
        #[arg(long)]
        quiet: bool,

        /// Maximum concurrent `nix build` invocations
        #[arg(long, default_value = "1")]
        max_inflight: usize,

        /// Attic cache name (e.g. `nexus`). Omit to build without push.
        #[arg(long)]
        attic_cache: Option<String>,

        /// Attic server alias for `attic login`
        #[arg(long, default_value = "nexus")]
        attic_server: String,

        /// Attic server URL (e.g. `http://rio:8080/`)
        #[arg(long)]
        attic_url: Option<String>,

        /// Path to file containing the Attic JWT token (SOPS-managed)
        #[arg(long)]
        attic_token_file: Option<PathBuf>,

        /// Which flake outputs to build: "all" (every
        /// `packages.${system}.*` — max cache coverage), "default", or a
        /// comma-separated allow-list (e.g. "mado,tear"). A `prebuild:`
        /// block in the config overrides this when present.
        #[arg(long, default_value = "all")]
        packages: String,

        /// Reproducibility gate before pushing: "trusting" (fast) or
        /// "verify" (build-and-compare via `nix build --rebuild`; never
        /// push a non-reproducible closure to a substitution-source
        /// cache — the anti-poison gate).
        #[arg(long, default_value = "trusting")]
        repro: String,

        /// JSON array of cache targets to fan every closure out to —
        /// rendered by the typed Nix module from its `caches` list. Each
        /// element: `{name, server, url, token_file, enabled?}`. Takes
        /// precedence over the single `--attic-*` quartet; default `[]`
        /// (use the single cache).
        #[arg(long, default_value = "[]")]
        caches_json: String,
    },

    /// Run `prebuild` continuously with exponential backoff. Idempotent:
    /// converged cycles (zero builds) double the sleep up to
    /// `max_interval`; any build resets sleep to `min_interval`.
    PrebuildDaemon {
        /// Path to config file
        #[arg(long)]
        config: Option<PathBuf>,

        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Minimum sleep between cycles, in seconds (reset interval after work)
        #[arg(long, default_value = "120")]
        min_interval: u64,

        /// Maximum sleep between cycles when converged, in seconds
        #[arg(long, default_value = "3600")]
        max_interval: u64,

        /// Maximum concurrent `nix build` invocations
        #[arg(long, default_value = "1")]
        max_inflight: usize,

        /// Suppress per-step output
        #[arg(long)]
        quiet: bool,

        /// Attic cache name (e.g. `nexus`). Omit to build without push.
        #[arg(long)]
        attic_cache: Option<String>,

        /// Attic server alias for `attic login`
        #[arg(long, default_value = "nexus")]
        attic_server: String,

        /// Attic server URL (e.g. `http://rio:8080/`)
        #[arg(long)]
        attic_url: Option<String>,

        /// Path to file containing the Attic JWT token (SOPS-managed)
        #[arg(long)]
        attic_token_file: Option<PathBuf>,

        /// Probe whether the Attic server (`--attic-url`) is reachable
        /// before each cycle; when it's down, back off (separately from
        /// the converged backoff) and don't build closures we can't
        /// push. Defaults to ON when `--attic-url` is set, OFF otherwise.
        /// Pass `--attic-probe=false` to force-disable.
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
        attic_probe: bool,

        /// Floor of the unreachable-backoff, in seconds (first
        /// unreachable cycle sleeps this long).
        #[arg(long, default_value = "60")]
        attic_unreachable_min_interval: u64,

        /// Ceiling of the unreachable-backoff, in seconds (the doubling
        /// caps here during a sustained outage).
        #[arg(long, default_value = "1800")]
        attic_unreachable_max_interval: u64,

        /// Per-probe HTTP timeout, in seconds.
        #[arg(long, default_value = "5")]
        attic_probe_timeout: u64,

        /// Which flake outputs to build: "all" (every
        /// `packages.${system}.*` — max cache coverage; the fill
        /// default), "default", or a comma-separated allow-list. A
        /// `prebuild:` block in the config overrides this when present.
        #[arg(long, default_value = "all")]
        packages: String,

        /// Reproducibility gate before pushing: "trusting" (fast) or
        /// "verify" (build-and-compare; never push a non-reproducible
        /// closure to a substitution-source cache).
        #[arg(long, default_value = "trusting")]
        repro: String,

        /// JSON array of cache targets to fan every closure out to —
        /// rendered by the typed Nix module from its `caches` list. Each
        /// element: `{name, server, url, token_file, enabled?}`. Takes
        /// precedence over the single `--attic-*` quartet; default `[]`.
        #[arg(long, default_value = "[]")]
        caches_json: String,
    },

    /// Run as the fleet update controller (K8s operator).
    /// Build with `--features operator` to enable.
    /// See docs/OPERATOR-DESIGN.md.
    #[cfg(feature = "operator")]
    Operator,

    /// Run as the rate-limited GitHub API throttle worker. Pulls jobs
    /// from the TEND_GITHUB_JOBS NATS stream and dispatches at a fixed
    /// pace (default ≤8 req/min ≈ 9.6% of GitHub's 5000/hr cap), with
    /// adaptive halving when X-RateLimit-Remaining drops below
    /// pressure thresholds.
    ///
    /// Built on samba (pleme-io/samba) for the typed primitive. See
    /// pleme-io/theory/RATE-LIMITED-CONSUMERS.md.
    #[cfg(feature = "operator")]
    Throttle {
        /// Path to samba config YAML. Defaults to /etc/pleme-worker/config.yaml
        /// (matches pleme-lib.rate-limit-worker.config Helm template output).
        #[arg(long)]
        config: Option<PathBuf>,
    },

    /// Show the materialized config at a tier (bare/default/env/...).
    ConfigShow(shikumi::cli::ConfigShowCommand),
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

        Commands::NixpkgsAlign {
            config: config_path,
            workspace: ws_filter,
        } => {
            use crate::nixpkgs_align::{align_one_repo, substrate_canonical_rev, AlignOutcome};
            let cfg = load_config(config_path.as_deref())?;
            let git = crate::git::SystemGitOps;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let base_dir = ws.resolved_base_dir()?;
                let Some(canonical) = substrate_canonical_rev(&base_dir.join("substrate")) else {
                    eprintln!(
                        "[{}] substrate not found at {} — can't read the canonical nixpkgs rev; skipping",
                        ws.name,
                        base_dir.join("substrate").display()
                    );
                    continue;
                };
                let repos = sync::resolve_repos(ws, false).await?;
                let (mut aligned, mut skipped, mut failed) = (0u32, 0u32, 0u32);
                for repo in &repos {
                    let repo_path = base_dir.join(repo);
                    if !repo_path.join(".git").exists() {
                        skipped += 1;
                        continue;
                    }
                    match align_one_repo(&repo_path, &canonical, &git) {
                        Ok(AlignOutcome::Aligned) => {
                            aligned += 1;
                            println!("  OK   {repo}");
                        }
                        Ok(AlignOutcome::Skipped(reason)) => {
                            skipped += 1;
                            if reason == "dirty" {
                                println!("  skip {repo}: dirty (WIP untouched)");
                            }
                        }
                        Ok(AlignOutcome::Failed(reason)) => {
                            failed += 1;
                            println!("  FAIL {repo}: {reason}");
                        }
                        Err(e) => {
                            failed += 1;
                            println!("  FAIL {repo}: {e}");
                        }
                    }
                }
                println!(
                    "[{}] nixpkgs-align -> {canonical}: aligned={aligned} skipped={skipped} failed={failed}",
                    ws.name
                );
            }
        }

        Commands::Placeholder { path, unmark } => {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()?.join(&path)
            };
            if unmark {
                placeholder::unmark_placeholder(&resolved)?;
                println!("unmarked {} as placeholder", resolved.display());
            } else {
                placeholder::mark_placeholder(&resolved)?;
                println!("marked {} as placeholder", resolved.display());
            }
        }

        Commands::Doctor { log } => {
            let drift_path = log
                .or_else(|| {
                    audit::AuditLog::default_path()
                        .path()
                        .parent()
                        .map(|p| p.join("drift-events.jsonl"))
                })
                .ok_or_else(|| anyhow::anyhow!("no drift log path"))?;
            if !drift_path.exists() {
                anyhow::bail!(
                    "drift log {} does not exist — run `tend daemon` or `tend reconcile` first to populate it",
                    drift_path.display()
                );
            }
            let content = std::fs::read_to_string(&drift_path)?;
            let mut total = 0usize;
            let mut by_kind: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            let mut events: Vec<drift::DriftEvent> = Vec::new();
            for line in content.lines() {
                if let Ok(e) = serde_json::from_str::<drift::DriftEvent>(line) {
                    total += 1;
                    let kind = match &e {
                        drift::DriftEvent::StubDirectoryFound { .. } => "stub-directory",
                        drift::DriftEvent::DirtyTreeBlocksPull { .. } => "dirty-tree",
                        drift::DriftEvent::PullFailed { .. } => "pull-failed",
                        drift::DriftEvent::PullFailedNoUpstream { .. } => "pull-failed-no-upstream",
                        drift::DriftEvent::PullFailedBranchRenamed { .. } => {
                            "pull-failed-branch-renamed"
                        }
                        drift::DriftEvent::PullFailedDiverged { .. } => "pull-failed-diverged",
                        drift::DriftEvent::PullFailedRepoMissing { .. } => {
                            "pull-failed-repo-missing"
                        }
                        drift::DriftEvent::PullFailedTransient { .. } => "pull-failed-transient",
                        drift::DriftEvent::SyncFailed { .. } => "sync-failed",
                        drift::DriftEvent::RepoHasNoRemote { .. } => "repo-has-no-remote",
                        drift::DriftEvent::JobUnhealed { .. } => "job-unhealed",
                        drift::DriftEvent::LocalRepoNotInDiscovery { .. } => {
                            "local-not-in-discovery"
                        }
                    };
                    *by_kind.entry(kind).or_default() += 1;
                    events.push(e);
                }
            }

            use colored::Colorize;
            println!(
                "{} {} drift event(s) total",
                "tend doctor:".bold(),
                total.to_string().cyan()
            );
            if total == 0 {
                println!("  workspace is clean — no operator action needed");
            } else {
                for (kind, n) in &by_kind {
                    println!("  {}: {}", kind.bold(), n.to_string().yellow());
                }
                println!();
                println!("{}", "suggested actions:".bold());
                let mut suggestions_seen = std::collections::HashSet::new();
                for e in &events {
                    let suggestion = match e {
                        drift::DriftEvent::StubDirectoryFound { repo_name, .. } => Some(format!(
                            "  stub dir [{repo_name}]: remove it manually if expected — `rm -rf <path>` — OR mark as intentional with `tend placeholder <path>`"
                        )),
                        drift::DriftEvent::DirtyTreeBlocksPull { repo_name, .. } => Some(format!(
                            "  dirty tree [{repo_name}]: review changes with `git -C <path> status` — commit, stash, or discard"
                        )),
                        drift::DriftEvent::PullFailed { repo_name, .. } => Some(format!(
                            "  pull failed (unclassified) [{repo_name}]: check `tend report` for the full stderr — consider adding a typed classifier arm if the failure shape repeats"
                        )),
                        drift::DriftEvent::PullFailedNoUpstream { repo_name, .. } => Some(format!(
                            "  no upstream [{repo_name}]: `git -C <path> branch --set-upstream-to=origin/<branch> <branch>` — SAFE-CONVERGENCE M2 will auto-apply"
                        )),
                        drift::DriftEvent::PullFailedBranchRenamed { repo_name, expected_ref, .. } => Some(format!(
                            "  branch renamed [{repo_name}]: expected {expected_ref} missing on remote — M6 auto-fetches; if persistent, set tracking to the new default branch (`git remote set-head origin --auto` + retry)"
                        )),
                        drift::DriftEvent::PullFailedDiverged { repo_name, .. } => Some(format!(
                            "  diverged [{repo_name}]: local has commits not on origin (often stale substrate-bump leftover); decide push vs `git reset --hard origin/<branch>` — see `incident_pleme_io_mass_rebase_wedge_2026_05_28`"
                        )),
                        drift::DriftEvent::PullFailedRepoMissing { repo_name, .. } => Some(format!(
                            "  upstream gone [{repo_name}]: remote returned 404 — confirm intent then `tend adopt` (preserve local) or `rm -rf <path>` (remove)"
                        )),
                        drift::DriftEvent::PullFailedTransient { repo_name, snippet, .. } => Some(format!(
                            "  transient [{repo_name}]: {snippet} — next reconcile cycle should retry; no action needed unless it persists"
                        )),
                        drift::DriftEvent::SyncFailed { repo_name, .. } => Some(format!(
                            "  sync failed [{repo_name}]: check GitHub creds + network; retry with `tend reconcile`"
                        )),
                        drift::DriftEvent::RepoHasNoRemote { repo_name, .. } => Some(format!(
                            "  NO REMOTE [{repo_name}]: this history exists on this machine ONLY — no git, GitHub, or tend backup covers it. \
                             Decide where it belongs, then `git -C <path> remote add origin <url>` + `git -C <path> push -u origin <branch>`. \
                             tend will NOT do this for you: the obvious guessed URL may already hold unrelated content, and force-pushing to it would destroy a history"
                        )),
                        drift::DriftEvent::LocalRepoNotInDiscovery {
                            workspace,
                            repo_name,
                        } => Some(format!(
                            "  local-only [{repo_name}]: archived/deleted upstream or local-only repo; `tend adopt {workspace} {repo_name}` to keep it, or remove the local dir"
                        )),
                        drift::DriftEvent::JobUnhealed { .. } => None,
                    };
                    if let Some(s) = suggestion {
                        if suggestions_seen.insert(s.clone()) {
                            println!("{s}");
                        }
                    }
                }
            }
        }

        Commands::Adopt {
            config: config_path,
            workspace,
            repo,
        } => {
            // Validate the workspace exists + the repo dir is on disk
            // BEFORE touching the config file, so a typo doesn't
            // corrupt the YAML.
            let cfg = load_config(config_path.as_deref())?;
            let ws = cfg
                .workspaces
                .iter()
                .find(|w| w.name == workspace)
                .ok_or_else(|| anyhow::anyhow!("workspace '{workspace}' not found in config"))?;
            let base_dir = ws.resolved_base_dir()?;
            let repo_path = base_dir.join(&repo);
            if !repo_path.exists() {
                anyhow::bail!(
                    "repo dir {} does not exist — clone it manually first or use `tend sync`",
                    repo_path.display()
                );
            }
            if ws.extra_repos.contains(&repo) {
                println!(
                    "{}/{} is already in extra_repos — nothing to do",
                    workspace, repo
                );
            } else {
                // Real edit: read → parse YAML AST → mutate → write.
                // serde_yaml_ng::Value lets us navigate the document
                // structurally rather than splicing text, so the YAML
                // type system stays load-bearing.
                let cfg_path = match config_path {
                    Some(p) => p.to_path_buf(),
                    None => config::Config::default_path(),
                };
                let content = std::fs::read_to_string(&cfg_path).with_context(|| {
                    format!("reading {}", cfg_path.display())
                })?;
                let mut doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&content)
                    .with_context(|| format!("parsing {}", cfg_path.display()))?;

                let workspaces = doc
                    .get_mut("workspaces")
                    .and_then(|v| v.as_sequence_mut())
                    .ok_or_else(|| anyhow::anyhow!(
                        "config has no `workspaces:` sequence"
                    ))?;

                let mut found = false;
                for ws_val in workspaces.iter_mut() {
                    let name_match = ws_val
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map_or(false, |n| n == workspace);
                    if !name_match {
                        continue;
                    }
                    found = true;
                    let map = ws_val.as_mapping_mut().ok_or_else(|| {
                        anyhow::anyhow!("workspace entry is not a mapping")
                    })?;
                    let key = serde_yaml_ng::Value::String("extra_repos".into());
                    let entry = map
                        .entry(key)
                        .or_insert_with(|| serde_yaml_ng::Value::Sequence(vec![]));
                    let seq = entry.as_sequence_mut().ok_or_else(|| {
                        anyhow::anyhow!("extra_repos exists but is not a sequence")
                    })?;
                    seq.push(serde_yaml_ng::Value::String(repo.clone()));
                    break;
                }
                if !found {
                    anyhow::bail!(
                        "workspace '{workspace}' present in parsed config but missing in YAML AST — refusing to write"
                    );
                }

                let serialized = serde_yaml_ng::to_string(&doc)
                    .with_context(|| "re-serializing config YAML")?;
                std::fs::write(&cfg_path, &serialized).with_context(|| {
                    format!("writing {}", cfg_path.display())
                })?;
                println!(
                    "adopted {}/{} into config at {} (extra_repos updated)",
                    workspace,
                    repo,
                    cfg_path.display()
                );
            }
        }

        Commands::Report {
            log,
            window_hours,
        } => {
            let path = log
                .or_else(|| {
                    audit::AuditLog::default_path()
                        .path()
                        .parent()
                        .map(|p| p.join("scheduler-transitions.jsonl"))
                })
                .ok_or_else(|| anyhow::anyhow!("no transition log path"))?;
            if !path.exists() {
                anyhow::bail!(
                    "transition log {} does not exist — run `tend daemon` or `tend reconcile` first to populate it",
                    path.display()
                );
            }
            // Drift log lives next to the transition log; both written
            // by the same reconcile path.
            let drift_path = path
                .parent()
                .map(|p| p.join("drift-events.jsonl"))
                .filter(|p| p.exists());
            let report = report::build_report(&path, drift_path.as_deref(), window_hours)?;
            report::print_report(&report);
        }

        Commands::Reconcile {
            config: config_path,
            workspace: ws_filter,
            refresh,
            max_inflight,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            let mut any_failed = false;
            // Same transition log as the daemon path — operator can
            // grep one file across both `tend daemon` and `tend reconcile`.
            let transition_log_path = audit::AuditLog::default_path()
                .path()
                .parent()
                .map(|p| p.join("scheduler-transitions.jsonl"));
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws, refresh).await?;
                let receipt = reconcile::reconcile_workspace_pull(
                    ws,
                    &repos,
                    max_inflight,
                    transition_log_path.as_deref(),
                )
                .await?;
                reconcile::print_receipt(&receipt);
                if !receipt.all_clean() {
                    any_failed = true;
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }

        Commands::Status {
            config: config_path,
            workspace: ws_filter,
            refresh,
            json,
            no_fix,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            let mut rows: Vec<display::StatusJsonRow> = Vec::new();
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let repos = sync::resolve_repos(ws, refresh).await?;
                let entries = sync::check_status(ws, &repos).await?;
                if json {
                    let base_dir = ws.resolved_base_dir()?;
                    rows.extend(entries.iter().map(|e| display::StatusJsonRow::new(e, &base_dir)));
                } else {
                    display::print_status(&ws.name, &entries);
                }
            }
            if json {
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                let host_audit = audit::AuditLog::default_path();
                let report = host_health::run_host_health_check(
                    &host_health::SystemProcessLister,
                    &host_health::SystemProcessKiller,
                    &host_health::SystemSysctlReader,
                    &cfg.host_health.watched_commands,
                    cfg.host_health.fd_pressure_threshold,
                    cfg.host_health.fix && !no_fix,
                    &host_audit,
                );
                if !report.orphans.is_empty() {
                    println!();
                    println!(
                        "Host health: {} orphaned process(es) (PPID==1). Logged to {}:",
                        report.orphans.len(),
                        host_audit.path().display()
                    );
                    for o in &report.orphans {
                        println!("  pid {} etime {} tty {} -- {}", o.pid, o.etime, o.tty, o.command);
                    }
                    if report.reap_outcomes.is_empty() {
                        println!("  (not reaped -- pass without --no-fix, or set host_health.fix: true)");
                    } else {
                        for (o, outcome) in &report.reap_outcomes {
                            let desc = match outcome {
                                host_health::ReapOutcome::Killed => "killed".to_string(),
                                host_health::ReapOutcome::AlreadyGone => "already gone".to_string(),
                                host_health::ReapOutcome::Failed(e) => format!("FAILED: {e}"),
                            };
                            println!("  pid {} reap: {}", o.pid, desc);
                        }
                    }
                }
                if let Some(p) = report.fd_pressure {
                    if report.fd_pressure_exceeded {
                        println!();
                        println!(
                            "Host health: fd pressure {:.1}% ({}/{}) -- above the {:.0}% threshold. Logged to {}.",
                            p.ratio() * 100.0,
                            p.used,
                            p.max,
                            cfg.host_health.fd_pressure_threshold * 100.0,
                            host_audit.path().display()
                        );
                    }
                }
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
                prune_direnv: env_flag_enabled("TEND_PRUNE_DIRENV"),
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
                // Check for dirty repos that need attention before declaring convergence
                let dirty_repos = flake::find_dirty_repos(ws)?;
                if !dirty_repos.is_empty() && chain.is_empty() {
                    // Still try cargo updates for dirty repos before declaring no work to do
                    if !quiet {
                        eprintln!(
                            "{}: found {} dirty repos (running cargo updates first): {}",
                            ws.name,
                            dirty_repos.len(),
                            dirty_repos.join(", ")
                        );
                    }
                }
                if chain.is_empty() && dirty_repos.is_empty() {
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

                // Also run cargo updates for Rust repos that are dirty (have uncommitted changes)
                // This catches repos that aren't in flake_deps but have Cargo.lock changes
                let dirty_repos = flake::find_dirty_repos(ws)?;
                let dirty_cargo_repos: Vec<String> = dirty_repos
                    .into_iter()
                    .filter(|r| {
                        if let Ok(base_dir) = ws.resolved_base_dir() {
                            base_dir.join(r).join("Cargo.lock").exists()
                        } else {
                            false
                        }
                    })
                    .collect();
                if !dirty_cargo_repos.is_empty() && !opts.dry_run {
                    if !quiet {
                        println!("{}: running cargo updates for {} dirty Rust repos", ws.name, dirty_cargo_repos.len());
                    }
                    // Pull first for each dirty cargo repo
                    let base_dir = ws.resolved_base_dir()?;
                    for repo in &dirty_cargo_repos {
                        let repo_path = base_dir.join(repo);
                        if repo_path.exists() {
                            let _ = flake::git_pull_ff(&repo_path, repo, true);
                        }
                    }
                    let cargo_steps: Vec<flake::UpdateStep> = dirty_cargo_repos
                        .iter()
                        .map(|r| flake::UpdateStep {
                            repo: r.clone(),
                            inputs: vec!["cargo".to_string()],
                        })
                        .collect();
                    let cargo_summary = flake::execute_cargo_update(ws, &cargo_steps, opts)?;
                    summary.updated += cargo_summary.updated;
                    summary.no_change += cargo_summary.no_change;
                    summary.skipped += cargo_summary.skipped;
                    if !quiet && cargo_summary.updated > 0 {
                        println!("{}: {} cargo updates committed and pushed", ws.name, cargo_summary.updated);
                    }
                }

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

        Commands::Prebuild {
            config: config_path,
            workspace: ws_filter,
            quiet,
            max_inflight,
            attic_cache,
            attic_server,
            attic_url,
            attic_token_file,
            packages,
            repro,
            caches_json,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            let opts = prebuild::PrebuildOptions {
                quiet,
                max_inflight,
                attic: build_attic_push(
                    attic_cache.as_deref(),
                    &attic_server,
                    attic_url.as_deref(),
                    attic_token_file.as_deref(),
                )?,
                selector: prebuild_cache::PackageSelector::parse(&packages),
                repro: prebuild_cache::ReproPolicy::parse(&repro),
                caches: prebuild_cache::parse_caches_json(&caches_json).unwrap_or_else(|e| {
                    eprintln!("[prebuild] invalid --caches-json, using single cache: {e}");
                    Vec::new()
                }),
                ..Default::default()
            };
            let audit = audit::AuditLog::default_path();
            // run_cycle overlays the config's prebuild block
            // (packages/caches/repro/systems) onto these CLI options.
            let summary =
                prebuild::run_cycle(&cfg, ws_filter.as_deref(), &opts, &audit).await?;
            println!(
                "prebuild: {} built, {} no-change, {} no-default, {} failed, {} pushed",
                summary.built,
                summary.no_change,
                summary.skipped_no_default,
                summary.failed,
                summary.pushed,
            );
        }

        Commands::PrebuildDaemon {
            config: config_path,
            workspace: ws_filter,
            min_interval,
            max_interval,
            max_inflight,
            quiet,
            attic_cache,
            attic_server,
            attic_url,
            attic_token_file,
            attic_probe,
            attic_unreachable_min_interval,
            attic_unreachable_max_interval,
            attic_probe_timeout,
            packages,
            repro,
            caches_json,
        } => {
            // The probe is meaningful only when we know where attic
            // lives. Enable it iff requested AND a URL is present.
            let reachability = prebuild::ReachabilityOptions {
                enabled: attic_probe && attic_url.is_some(),
                url: attic_url.clone().unwrap_or_default(),
                min_interval: attic_unreachable_min_interval,
                max_interval: attic_unreachable_max_interval,
                probe_timeout: attic_probe_timeout,
            };
            run_prebuild_daemon(
                config_path,
                ws_filter,
                min_interval,
                max_interval,
                max_inflight,
                quiet,
                attic_cache,
                attic_server,
                attic_url,
                attic_token_file,
                reachability,
                packages,
                repro,
                caches_json,
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
            pull,
            fetch,
            quiet,
            github_token_file,
            max_inflight,
        } => {
            // In launchd/systemd environments, env vars may not be inherited.
            // Read the token from a file and set GITHUB_TOKEN for provider discovery.
            // The path is also threaded into DaemonOpts below so a SIGHUP poke can
            // re-read it and refresh this cache without a restart.
            if let Some(ref token_path) = github_token_file {
                let token = std::fs::read_to_string(token_path)
                    .with_context(|| format!("reading token from {}", token_path.display()))?;
                std::env::set_var("GITHUB_TOKEN", token.trim());
            }

            // Open the kanshou introspection socket so operators can
            // query the live tend daemon — ticks completed, current
            // workspace/repo, pull/fetch counters — via
            // `gen kanshou query tend <field>`. Best-effort: bind
            // failure logs and continues so introspection-disabled
            // mode is graceful.
            let kanshou_state = std::sync::Arc::new(
                kanshou_state::TendDaemonState::new(),
            );
            match kanshou_state::spawn_server("tend", std::sync::Arc::clone(&kanshou_state)) {
                Ok(path) => tracing::info!(
                    socket = %path.display(),
                    "kanshou introspection live"
                ),
                Err(e) => tracing::warn!(
                    err = %e,
                    "kanshou bind failed; introspection disabled"
                ),
            }

            daemon::run_with_kanshou(
                daemon::DaemonOpts {
                    config: config_path,
                    workspace: ws_filter,
                    interval,
                    pull,
                    fetch,
                    quiet,
                    max_inflight,
                    github_token_file,
                },
                kanshou_state,
            )
            .await?;
        }

        Commands::ReleaseSwarmPlan {
            config: config_path,
            workspace: ws_filter,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            let audit_log = audit::AuditLog::default_path();
            let mut total_eligible = 0usize;
            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let swarm_cfg = match ws.watch.as_ref().and_then(|w| w.release_swarm.as_ref()) {
                    Some(s) if s.enable => s,
                    _ => continue,
                };
                let plan = release_swarm::plan_swarm(swarm_cfg);
                total_eligible += plan.eligible_count;
                println!(
                    "workspace: {} — org: {} — enabled: {} — eligible: {} — disabled: {} — forbidden: {}",
                    ws.name,
                    plan.org,
                    plan.org_enabled,
                    plan.eligible_count,
                    plan.declared_but_disabled.len(),
                    plan.declared_but_forbidden.len(),
                );
                for repo in &plan.eligible_repos {
                    println!("  + {}/{repo}", plan.org);
                }
                for repo in &plan.declared_but_disabled {
                    println!("  - {}/{repo} (enable: false)", plan.org);
                }
                for repo in &plan.declared_but_forbidden {
                    // Loud — this is a misconfiguration. User listed a
                    // name that FORBIDDEN_PATTERNS forbids; config cannot
                    // override.
                    eprintln!(
                        "  ! {}/{repo} (FORBIDDEN — matches FORBIDDEN_PATTERNS; remove from config)",
                        plan.org
                    );
                    audit_log.log(
                        "release_swarm_repo_forbidden",
                        serde_json::json!({
                            "workspace": ws.name,
                            "org": plan.org,
                            "repo": repo,
                            "reason": "matches FORBIDDEN_PATTERNS",
                        }),
                    );
                }
                audit_log.log(
                    "release_swarm_plan_computed",
                    serde_json::json!({
                        "workspace": ws.name,
                        "org": plan.org,
                        "eligible_count": plan.eligible_count,
                        "eligible_repos": plan.eligible_repos,
                        "declared_but_disabled": plan.declared_but_disabled,
                        "declared_but_forbidden": plan.declared_but_forbidden,
                    }),
                );
            }
            if total_eligible == 0 {
                println!("no eligible repos across workspaces — deny-by-default holds");
            }
        }

        Commands::ReleaseSwarmApply {
            config: config_path,
            workspace: ws_filter,
            dry_run,
        } => {
            let cfg = load_config(config_path.as_deref())?;
            let audit_log = audit::AuditLog::default_path();
            // Render fn stub — produces the canonical 3-target workflow YAML
            // derived from repo_name + binary_name. Later swapped for a call
            // into arch-synthesizer's RustToolPublicReleaseDecl::render().
            let render =
                |repo_name: &str, repo_cfg: &release_swarm::RepoReleaseConfig| {
                    render_rust_tool_release_workflow_yaml(repo_name, repo_cfg)
                };

            for ws in filter_workspaces(&cfg.workspaces, ws_filter.as_deref()) {
                let swarm_cfg = match ws.watch.as_ref().and_then(|w| w.release_swarm.as_ref()) {
                    Some(s) if s.enable => s,
                    _ => continue,
                };
                // In dry-run, skip GitHub entirely via the in-process mock;
                // otherwise construct the real HTTP client (requires
                // GITHUB_TOKEN env or equivalent).
                let reports = if dry_run {
                    let mock = MockReleaseSwarmApi;
                    release_swarm::apply_swarm(&mock, swarm_cfg, dry_run, render).await?
                } else {
                    let token = provider::github_token().ok_or_else(|| {
                        anyhow::anyhow!(
                            "GITHUB_TOKEN not set — release-swarm apply needs a PAT \
                             with repo write scope"
                        )
                    })?;
                    let api = release_swarm_http::HttpReleaseSwarmApi::new(token)?;
                    release_swarm::apply_swarm(&api, swarm_cfg, dry_run, render).await?
                };
                for r in &reports {
                    match &r.outcome {
                        release_swarm::ApplyOutcome::DryRun { rendered_bytes } => {
                            println!(
                                "[dry-run] {}/{} — would render {rendered_bytes} bytes of release.yml",
                                r.org, r.repo
                            );
                        }
                        release_swarm::ApplyOutcome::PrOpened { pr_number } => {
                            println!("[applied] {}/{} — PR #{pr_number} opened", r.org, r.repo);
                            audit_log.log(
                                "release_swarm_pr_opened",
                                serde_json::json!({
                                    "workspace": ws.name,
                                    "org": r.org,
                                    "repo": r.repo,
                                    "pr_number": pr_number,
                                }),
                            );
                        }
                        release_swarm::ApplyOutcome::AlreadyInSync => {
                            println!("[in-sync] {}/{} — workflow already matches", r.org, r.repo);
                        }
                        release_swarm::ApplyOutcome::IneligibleSkipped => {
                            audit_log.log(
                                "release_swarm_repo_skipped",
                                serde_json::json!({
                                    "workspace": ws.name,
                                    "org": r.org,
                                    "repo": r.repo,
                                    "reason": "ineligible",
                                }),
                            );
                        }
                    }
                }
            }
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

        #[cfg(feature = "operator")]
        Commands::Operator => {
            operator::run().await?;
        }
        #[cfg(feature = "operator")]
        Commands::Throttle { config } => {
            operator::throttle::run(config.as_deref()).await?;
        }

        Commands::ConfigShow(cmd) => {
            cmd.run::<config::Config>("TEND_TIER")
                .map_err(|e| anyhow::anyhow!("config-show failed: {e}"))?;
        }
    }

    Ok(())
}

/// True iff the named environment variable is set to a truthy value
/// (`1`, `true`, `yes`, `on`, case-insensitive). Used for opt-in feature
/// gates that default off, like `TEND_PRUNE_DIRENV`.
pub(crate) fn env_flag_enabled(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
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
        prune_direnv: env_flag_enabled("TEND_PRUNE_DIRENV"),
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

/// Translate CLI flags into a `prebuild::AtticPush`. Returns `None`
/// if no cache is configured (i.e. build-only mode). Returns an
/// `Err` if a cache is named but the URL/token are missing, because
/// that's almost certainly an operator mistake we want to surface
/// loudly rather than silently building without pushing.
fn build_attic_push(
    cache_name: Option<&str>,
    server_name: &str,
    server_url: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> Result<Option<prebuild::AtticPush>> {
    let Some(cache) = cache_name else {
        return Ok(None);
    };
    let url = server_url.ok_or_else(|| {
        anyhow::anyhow!("--attic-url is required when --attic-cache is set")
    })?;
    let token = token_file.ok_or_else(|| {
        anyhow::anyhow!("--attic-token-file is required when --attic-cache is set")
    })?;
    Ok(Some(prebuild::AtticPush {
        cache_name: cache.to_string(),
        server_name: server_name.to_string(),
        server_url: url.to_string(),
        token_file: token.to_path_buf(),
    }))
}

#[allow(clippy::too_many_arguments)]
async fn run_prebuild_daemon(
    config_path: Option<PathBuf>,
    ws_filter: Option<String>,
    min_interval: u64,
    max_interval: u64,
    max_inflight: usize,
    quiet: bool,
    attic_cache: Option<String>,
    attic_server: String,
    attic_url: Option<String>,
    attic_token_file: Option<PathBuf>,
    reachability: prebuild::ReachabilityOptions,
    packages: String,
    repro: String,
    caches_json: String,
) -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::Notify;

    let min = min_interval.max(1);
    let max = max_interval.max(min);
    let mut interval = min;
    let audit = audit::AuditLog::default_path();

    // Tracks a run of consecutive "attic unreachable" cycles so the
    // separate unreachable-backoff can double. Reset to 0 whenever the
    // server is reachable (or probing is disabled).
    let mut unreachable_streak: u32 = 0;

    // Resolve the config path to a concrete file so we can install
    // an inotify-style watcher on it. K8s-controller-style live
    // config: file changes wake the daemon mid-sleep, the next
    // cycle runs immediately against the fresh config — no restart,
    // no waiting for the exp-backoff window to close.
    let resolved_path: PathBuf = match config_path.as_ref() {
        Some(p) => p.clone(),
        None => config::Config::default_path(),
    };

    let reload_signal = Arc::new(Notify::new());
    let watch_signal = Arc::clone(&reload_signal);
    let audit_for_watcher = audit::AuditLog::default_path();
    let watcher = shikumi::ConfigWatcher::watch(&resolved_path, move |event| {
        // shikumi's pinned ConfigWatcher hands us the raw notify::Event
        // and leaves classification to the caller. We trigger reload
        // on Create + Modify, ignore Access + Remove (Remove will
        // surface a not-found at next load_config() and re-arm on
        // the next file appearance via the parent-dir watch).
        use notify::EventKind;
        match event.kind {
            EventKind::Modify(_) | EventKind::Create(_) => {
                audit_for_watcher.log(
                    "prebuild_config_reload_detected",
                    serde_json::json!({
                        "kind": format!("{:?}", event.kind),
                        "paths": event.paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                    }),
                );
                watch_signal.notify_one();
            }
            _ => {}
        }
    });
    // Don't fail the daemon if the file doesn't exist yet — the
    // existing per-cycle load_config() error already reports that.
    // We just lose hot-reload until the path materialises.
    let _watcher_handle = watcher.ok();

    if !quiet {
        println!(
            "prebuild daemon starting (min={min}s max={max}s max_inflight={max_inflight}). Hot-reload watching {}. Ctrl-C to stop.",
            resolved_path.display()
        );
    }

    loop {
        // Attic-reachability gate. When probing is enabled and the
        // server is down, skip the build entirely and back off on the
        // separate unreachable-backoff ladder — there's no point
        // producing closures we can't push. When it comes back up we
        // reset the streak and fall through to a normal cycle.
        if reachability.enabled {
            let reachable = prebuild::probe_attic_reachable(
                &reachability.url,
                reachability.probe_timeout,
            )
            .await;
            if !reachable {
                unreachable_streak = unreachable_streak.saturating_add(1);
                let sleep_secs = reachability.unreachable_sleep(unreachable_streak);
                // Log the transition INTO the unreachable state once
                // (first strike), then stay quiet per re-probe.
                if unreachable_streak == 1 {
                    if !quiet {
                        println!(
                            "prebuild: attic unreachable ({}), backing off {sleep_secs}s",
                            reachability.url
                        );
                    }
                    audit.log(
                        "prebuild_attic_unreachable",
                        serde_json::json!({
                            "url": reachability.url,
                            "backoff_secs": sleep_secs,
                        }),
                    );
                }
                // Honour config-change wakes even while backing off, so
                // an operator edit doesn't get stuck behind a long
                // unreachable sleep.
                tokio::select! {
                    biased;
                    () = reload_signal.notified() => {
                        audit.log(
                            "prebuild_woken_by_config_change",
                            serde_json::json!({ "during": "attic_unreachable_backoff" }),
                        );
                        interval = min;
                    }
                    () = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {}
                }
                continue;
            } else if unreachable_streak > 0 {
                // Transition back to reachable — log once, reset both
                // the streak and the converged interval so we resume
                // at the floor.
                if !quiet {
                    println!("prebuild: attic reachable, resuming");
                }
                audit.log(
                    "prebuild_attic_reachable",
                    serde_json::json!({ "url": reachability.url }),
                );
                unreachable_streak = 0;
                interval = min;
            }
        }

        let cycle_start = std::time::Instant::now();
        audit.log(
            "prebuild_cycle_start",
            serde_json::json!({ "interval_secs": interval }),
        );

        // Re-load config + attic push spec each cycle so an operator
        // can edit either without restarting the daemon. The
        // ConfigWatcher above signals reload_signal on file change,
        // which wakes the sleep below — so config edits propagate
        // within milliseconds, not within one backoff interval.
        let cycle_result: Result<prebuild::PrebuildSummary> = async {
            let cfg = load_config(config_path.as_deref())?;
            let opts = prebuild::PrebuildOptions {
                quiet,
                max_inflight,
                attic: build_attic_push(
                    attic_cache.as_deref(),
                    &attic_server,
                    attic_url.as_deref(),
                    attic_token_file.as_deref(),
                )?,
                selector: prebuild_cache::PackageSelector::parse(&packages),
                repro: prebuild_cache::ReproPolicy::parse(&repro),
                caches: prebuild_cache::parse_caches_json(&caches_json).unwrap_or_else(|e| {
                    eprintln!("[prebuild] invalid --caches-json, using single cache: {e}");
                    Vec::new()
                }),
                ..Default::default()
            };
            prebuild::run_cycle(&cfg, ws_filter.as_deref(), &opts, &audit).await
        }
        .await;

        match cycle_result {
            Ok(summary) => {
                let duration_ms = cycle_start.elapsed().as_millis() as u64;
                audit.log(
                    "prebuild_cycle_complete",
                    serde_json::json!({
                        "duration_ms": duration_ms,
                        "built": summary.built,
                        "no_change": summary.no_change,
                        "skipped_no_default": summary.skipped_no_default,
                        "failed": summary.failed,
                        "pushed": summary.pushed,
                    }),
                );
                if summary.work() > 0 {
                    interval = min;
                } else {
                    interval = (interval.saturating_mul(2)).min(max);
                }
            }
            Err(e) => {
                eprintln!("prebuild daemon cycle failed: {e:#}");
                audit.log(
                    "prebuild_cycle_error",
                    serde_json::json!({ "error": format!("{e:#}") }),
                );
                interval = min;
            }
        }

        if !quiet {
            println!("prebuild daemon sleeping {interval}s");
        }

        // select! between the backoff timer and a config-change
        // notification. The notification is a Notify (not a channel)
        // so multiple file changes during one sleep collapse into a
        // single wake — exactly what we want.
        tokio::select! {
            biased;
            () = reload_signal.notified() => {
                audit.log(
                    "prebuild_woken_by_config_change",
                    serde_json::json!({ "interval_remaining_secs": interval }),
                );
                // Reset interval on a config change so the operator
                // gets a fresh cycle immediately at the floor.
                interval = min;
            }
            () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
        }
    }
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

/// Local stub renderer for the canonical rust-tool-public-release
/// workflow. Upstream source of truth is
/// `arch-synthesizer/src/rust_tool_release/render.rs`
/// (`RustToolPublicReleaseDecl::render()`). This stub produces a
/// structurally-compatible workflow — 3-target matrix derived from
/// repo + binary name — until we extract that renderer into a shared
/// crate or shell out to `pangea_render`.
fn render_rust_tool_release_workflow_yaml(
    repo_name: &str,
    repo_cfg: &release_swarm::RepoReleaseConfig,
) -> String {
    let binary_name = repo_cfg.binary_name.clone().unwrap_or_else(|| repo_name.to_string());
    let features = if repo_cfg.features.is_empty() {
        String::new()
    } else {
        format!(" --features {}", repo_cfg.features.join(","))
    };
    format!(
        "# AUTO-GENERATED by `tend release-swarm apply`.\n\
         # Source: arch-synthesizer RustToolPublicReleaseDecl (local stub).\n\
         name: Release\n\
         on:\n  \
           push:\n    \
             tags: ['v*.*.*']\n\
         jobs:\n  \
           release:\n    \
             runs-on: ${{{{ matrix.os }}}}\n    \
             strategy:\n      \
               matrix:\n        \
                 include:\n          \
                   - os: macos-14\n            \
                     target: aarch64-apple-darwin\n          \
                   - os: ubuntu-24.04\n            \
                     target: x86_64-unknown-linux-gnu\n          \
                   - os: ubuntu-24.04-arm\n            \
                     target: aarch64-unknown-linux-gnu\n    \
             steps:\n      \
               - uses: actions/checkout@v4\n      \
               - uses: dtolnay/rust-toolchain@stable\n        \
                 with:\n          \
                   targets: ${{{{ matrix.target }}}}\n      \
               - run: cargo build --release --target ${{{{ matrix.target }}}}{features}\n      \
               - uses: softprops/action-gh-release@v2\n        \
                 with:\n          \
                   files: target/${{{{ matrix.target }}}}/release/{binary_name}\n\
         # binary: {binary_name} • repo: {repo_name}\n",
    )
}

/// In-tend mock ReleaseSwarmApi for `--dry-run`. Open-PR call is
/// never reached in dry-run. Real `HttpReleaseSwarmApi` lands in
/// the next iteration.
struct MockReleaseSwarmApi;

#[async_trait::async_trait]
impl release_swarm::ReleaseSwarmApi for MockReleaseSwarmApi {
    async fn get_workflow_file_sha(
        &self,
        _org: &str,
        _repo: &str,
        _path: &str,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    async fn open_workflow_pr(
        &self,
        _org: &str,
        _repo: &str,
        _branch: &str,
        _path: &str,
        _content: &str,
        _commit_message: &str,
        _pr_title: &str,
        _pr_body: &str,
    ) -> Result<u64> {
        anyhow::bail!("MockReleaseSwarmApi: apply requires real HTTP API (not yet wired)")
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
