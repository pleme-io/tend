mod ai_cron;
mod ai_executor;
mod ai_flow;
mod ai_lisp;
mod ai_models;
mod ai_planner;
mod audit;
mod cache;
mod cargo_target;
mod ci_trim;
mod config;
mod config_layers;
mod daemon;
mod flags;
mod gh_auth;
mod schema;
mod display;
mod hook;
mod kanshou_state;
mod mcp;
mod pressure;
mod push_ahead;
mod scan;
mod worktree;
mod xdg;
// Typed failure classification for a reconcile cycle. See src/failure.rs --
// it exists because a String reason produced three contradictory readings of
// one log on 2026-07-28, and because "the host was asleep" must not count as
// residue the way "the credential is gone" does.
mod anomaly;
mod drift;
mod failure;
mod flake;
mod flake_lock;
mod git;
mod github;
mod head_cache;
mod host_health;
mod jobs;
mod logrotate;
mod nixpkgs_align;
mod placeholder;
mod planner;
mod prebuild;
mod prebuild_cache;
mod provider;
mod reach;
mod reconcile;
mod release_swarm;
mod release_swarm_http;
mod remote_url;
mod report;
mod secret;
mod sync;
mod watch;
mod watch_cache;

#[cfg(feature = "operator")]
mod operator;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "tend",
    version,
    about = "Workspace repository manager",
    long_about = "Workspace repository manager.\n\n\
        Config is folded, lowest first: built-in defaults -> the config file \
        ($TEND_CONFIG, else ~/.config/tend/tend.yaml, else ~/.config/tend/config.yaml) \
        -> each --config FILE -> TEND_* env (`__` nests, e.g. TEND_DAEMON__INTERVAL=60) \
        -> this subcommand's flags -> each --set PATH=VALUE. \
        `tend config-show --effective --provenance` prints the result and who set each value."
)]
struct Cli {
    /// `--config FILE` (merge-override, repeatable) and `--set PATH=VALUE`.
    #[command(flatten, next_help_heading = "Config layers")]
    config: shikumi::cli::ConfigArgs,

    /// `--github-token-file`, `--github-app-*`: partials over `github_auth`.
    #[command(flatten, next_help_heading = "GitHub credential (replaces `github_auth`)")]
    github: flags::GithubFlags,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// MCP surface — what an agent can observe and do.
    ///
    /// `--list-tools` renders the catalog, including which tools this authority
    /// may actually call. Read-only by default: tend reconciles a whole
    /// workspace, so a mistaken write here is a fleet-wide action.
    Mcp {
        /// Print the tool catalog as JSON and exit.
        #[arg(long)]
        list_tools: bool,
        /// Permit state-changing tools (prune, land). Off by default.
        #[arg(long)]
        allow_mutate: bool,
    },

    /// Report host pressure and the verdict the daemon would act on.
    ///
    /// The gate runs unattended inside the daemon loop, so without this there is
    /// no way to see what it decided or why — and a guard nobody can observe is
    /// one nobody can trust. Exercises the REAL reader, not a re-derivation.
    Pressure {
        /// Filesystem to measure (default: current directory).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Concurrency to assess (`reconcile.max_inflight`; default: what
        /// the daemon is configured for).
        #[command(flatten)]
        flags: flags::ReconcileFlags,
    },

    /// Cargo `target/` directories in the workspace repos: size, idle days,
    /// and the verdict the configured `cargo_target:` policy reaches.
    ///
    /// `report` never deletes. `apply` removes what the policy says to, unless
    /// `--dry-run` or the config's `dry_run` holds it back. Only directories
    /// carrying cargo's `CACHEDIR.TAG` are ever candidates.
    CargoTarget {
        #[command(subcommand)]
        action: CargoTargetAction,
    },

    /// Per-session git worktrees — real isolation for concurrent agents.
    ///
    /// Concurrent sessions sharing one checkout share one INDEX, so a
    /// `git add -A` in one stages another's work. A worktree gives each session
    /// its own index and checkout, making that unconstructible rather than
    /// merely discouraged.
    Worktree {
        #[command(subcommand)]
        action: WorktreeAction,
    },

    #[command(
        about = "Claude Code hook handlers. stop and session-start read the hook payload as JSON on stdin and print the hook's JSON answer (or nothing), never exiting non-zero."
    )]
    Hook {
        #[command(subcommand)]
        event: HookEvent,
    },

    /// Clone missing repos into the workspace
    Sync {
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
    /// dirty repo. Plans by default: every repo is classified from its lock and
    /// reported, and nothing is written without `--apply`.
    NixpkgsAlign {
        /// Only align a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// Only these repos (repeatable). Default: every repo in the workspace.
        #[arg(long = "repo")]
        repos: Vec<String>,

        /// Write, commit and push. Without it the run is a plan.
        #[arg(long)]
        apply: bool,
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
        /// Only reconcile a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// Bypass discovery cache and always hit the GitHub API
        #[arg(long)]
        refresh: bool,

        /// `--max-inflight` (`reconcile.max_inflight`).
        #[command(flatten)]
        flags: flags::ReconcileFlags,
    },

    /// Show repo status (clean/dirty/ahead/no-upstream/behind/unborn/stuck/no-remote/missing/unknown)
    Status {
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

        #[arg(
            long,
            help = "Print only repos that need attention: everything except clean and unborn"
        )]
        problems: bool,

        /// Skip the host-health autocorrect: report orphaned watched
        /// processes (see host_health.rs) without SIGKILLing them.
        /// Detection + audit logging still run either way.
        #[arg(long)]
        no_fix: bool,
    },

    /// List configured repos
    List {
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
        /// Only sync a specific workspace by name
        #[arg(long)]
        workspace: Option<String>,

        /// `--interval`, `--pull`, `--fetch`, `--quiet`, `--max-inflight`:
        /// partials over `daemon.*` and `reconcile.max_inflight`.
        #[command(flatten)]
        flags: flags::DaemonFlags,
    },

    /// Run watch cycle once (detect new versions)
    Watch {
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
        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,
    },

    /// Apply the release-swarm — for each eligible repo, render the
    /// canonical release.yml and open a PR. Dry-run by default so
    /// nothing mutates without explicit opt-in.
    ReleaseSwarmApply {
        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Render but skip the PR-open call. Defaults TRUE — this
        /// command opens pull requests, so the safe posture is the default.
        ///
        /// ── ★ IT TAKES A VALUE, BECAUSE IT MUST BE TURNABLE OFF ────────
        /// This was a bare `#[arg(long, default_value_t = true)]`, which
        /// clap renders as SetTrue: `--dry-run` sets it true and there is
        /// no spelling that sets it false. So `release-swarm apply` could
        /// never actually apply — the subcommand's entire purpose was
        /// unreachable from the CLI, and every invocation reported a
        /// successful dry run.
        ///
        /// `ArgAction::Set` makes `--dry-run false` (or `--dry-run=false`)
        /// work while keeping the safe default.
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
        dry_run: bool,
    },

    /// Run flake-update --all continuously with exponential backoff.
    /// Idempotent: cycles where every workspace is converged do no work.
    FlakeUpdateDaemon {
        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// `--min-interval`, `--max-interval`, `--quiet`: partials over
        /// `flake_update_daemon.*`.
        #[command(flatten)]
        flags: flags::FlakeUpdateDaemonFlags,
    },

    /// Build every workspace flake repo whose HEAD has moved since
    /// last cycle, optionally pushing each closure to an Attic cache.
    /// One-shot; pair with `prebuild-daemon` for continuous fill.
    Prebuild {
        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Partials over the top-level `prebuild.*` config section.
        #[command(flatten)]
        flags: flags::PrebuildFlags,
    },

    /// Run `prebuild` continuously with exponential backoff. Idempotent:
    /// converged cycles (zero builds) double the sleep up to
    /// `max_interval`; any build resets sleep to `min_interval`.
    PrebuildDaemon {
        /// Only process a specific workspace
        #[arg(long)]
        workspace: Option<String>,

        /// Partials over the top-level `prebuild.*` config section,
        /// including the daemon pacing and `prebuild.probe.*`.
        #[command(flatten)]
        flags: flags::PrebuildDaemonFlags,
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
        /// Path to the samba worker config YAML. Defaults to
        /// /etc/pleme-worker/config.yaml (the pleme-lib.rate-limit-worker
        /// Helm template output). Named `--worker-config` because
        /// `--config` is tend's own config overlay on every subcommand.
        #[arg(long = "worker-config", value_name = "PATH")]
        worker_config: Option<PathBuf>,
    },

    /// Print the config at a tier (`bare`/`default`/`env`/...), or — with
    /// `--effective` — the config this exact invocation resolves: every
    /// file, `--config`, `TEND_*` env, flag and `--set` folded. Add
    /// `--provenance` to see which layer set each value.
    ConfigShow(shikumi::cli::ConfigShowCommand),

    /// Print the JSON Schema of tend's config (the committed
    /// `schema/tend-config.schema.json`).
    ConfigSchema,
}

#[derive(Subcommand)]
enum CargoTargetAction {
    /// Show every target dir with its size, idle days and verdict. Deletes nothing.
    Report {
        #[arg(long)]
        workspace: Option<String>,
        /// Emit the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove the directories the policy selects.
    Apply {
        #[arg(long)]
        workspace: Option<String>,
        /// Decide and report without deleting.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum HookEvent {
    #[command(
        about = "Stop: block once when repos touched this session are dirty or hold unpushed commits"
    )]
    Stop,
    #[command(about = "SessionStart: add the workspace's problem repos to the session's context")]
    SessionStart,
    #[command(
        about = "Refresh the status snapshot SessionStart reads (the daemon does this every cycle)"
    )]
    Snapshot,
}

#[derive(Subcommand)]
enum WorktreeAction {
    /// Create (or reuse) this session's worktree and print its path.
    ///
    /// Idempotent: every call in one session lands in the SAME worktree, so it
    /// is safe to invoke from a shell hook on each command.
    Enter {
        /// Repo to isolate (default: current directory).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Session id (default: $CLAUDE_CODE_SESSION_ID).
        #[arg(long)]
        session: Option<String>,
    },
    /// Run a command inside this session's worktree (default: claude).
    ///
    /// THE ENTRY POINT: `tend worktree session` is what makes isolation actually
    /// take effect, rather than being a capability nobody invokes. Creates or
    /// reuses the worktree, then EXECs — so the agent inherits the tty, signals
    /// and exit code directly.
    Session {
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        session: Option<String>,
        /// Command to run (default: claude). Everything after `--` is its args.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Rebase this session's branch onto the remote default branch and push it.
    ///
    /// The exit from an isolated worktree: work goes back to the shared branch
    /// deliberately, never by accident.
    Land {
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        session: Option<String>,
        /// Branch to land onto (default: main).
        #[arg(long, default_value = "main")]
        base: String,
    },

    /// Show every session worktree with its safety verdict.
    List {
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove worktrees that hold no work. Refuses on uncommitted or unpushed
    /// changes — never removes on a guess.
    Prune {
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Report what would be removed without touching anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Every subcommand resolves its config through this one constructor:
    // the operator's --config/--set, the global credential flags, and the
    // subcommand's own typed partial (`sub`).
    let config_args = cli.config.clone();
    let github_overlay = flags::Overlay::overlay(&cli.github);
    let loader = move |sub: serde_json::Value| {
        config_layers::ConfigLoader::new(
            config_args.clone(),
            flags::merge([github_overlay.clone(), sub]),
        )
    };
    let none = || serde_json::Value::Null;

    match cli.command {
        Commands::Mcp {
            list_tools,
            allow_mutate,
        } => {
            let authority = if allow_mutate {
                mcp::Authority::Mutate
            } else {
                mcp::Authority::Observe
            };
            if list_tools {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&mcp::catalog_json(authority))?
                );
            } else {
                // stdio transport: the server IS this process, so nothing may be
                // written to stdout except MCP frames. Diagnostics go to stderr.
                eprintln!(
                    "tend mcp: serving over stdio ({})",
                    if allow_mutate { "mutate" } else { "observe" }
                );
                mcp::run(authority, std::env::current_dir()?).await?;
            }
        }

        Commands::Pressure { path, flags } => {
            // The concurrency the daemon is configured for — the same
            // `reconcile.max_inflight` it resolves — unless overridden here.
            let max_inflight = loader(flags::Overlay::overlay(&flags))
                .load()?
                .reconcile
                .max_inflight;
            let path = path.unwrap_or(std::env::current_dir()?);
            let reader = pressure::SystemPressureReader { path: path.clone() };
            let reading = pressure::PressureReader::read(&reader)?;
            let verdict = pressure::assess(reading, pressure::Thresholds::default(), max_inflight);
            println!("path          {}", path.display());
            println!(
                "disk free     {:.1} GiB of {:.1} GiB ({:.1}%)",
                reading.disk_free_gib,
                reading.disk_total_gib,
                reading.disk_free_pct()
            );
            // "unavailable" rather than a fabricated 0.0 — this line was
            // printing "no descriptor pressure" on every host whose sysctl
            // it could not read.
            match reading.fd_ratio {
                Some(r) => println!("fd ratio      {r:.4}"),
                None => println!("fd ratio      unavailable (sysctl unreadable on this host)"),
            }
            match verdict.inflight(max_inflight) {
                Some(n) if n == max_inflight => println!("verdict       proceed at {n}"),
                Some(n) => println!("verdict       throttle to {n} — {}", verdict.why()),
                None => println!("verdict       run nothing — {}", verdict.why()),
            }
        }

        Commands::Hook { event } => {
            let event = match event {
                HookEvent::Stop => hook::Event::Stop,
                HookEvent::SessionStart => hook::Event::SessionStart,
                HookEvent::Snapshot => {
                    let cfg = loader(none()).load()?;
                    let workspaces: Vec<&config::Workspace> = cfg.workspaces.iter().collect();
                    let path = scan::snapshot_path();
                    let snapshot = scan::refresh_snapshot(
                        &workspaces,
                        cfg.status_snapshot.workers,
                        &path,
                        std::time::SystemTime::now(),
                    )?;
                    println!(
                        "{} of {} repos classified -> {}",
                        snapshot.rows.len(),
                        snapshot.total,
                        path.display()
                    );
                    return Ok(());
                }
            };
            let mut raw = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw);
            if let Some(answer) = hook::run(event, loader(none()).load(), &raw) {
                println!("{answer}");
            }
        }

        Commands::CargoTarget { action } => {
            let (ws_filter, mode, json, overlay) = match action {
                CargoTargetAction::Report { workspace, json } => {
                    (workspace, cargo_target::Mode::DryRun, json, none())
                }
                // `--dry-run` is `cargo_target.dry_run: true` for this run;
                // `sweep` honours the config's dry_run over the mode.
                CargoTargetAction::Apply {
                    workspace,
                    dry_run,
                    json,
                } => (
                    workspace,
                    cargo_target::Mode::Apply,
                    json,
                    flags::cargo_target_overlay(dry_run),
                ),
            };
            let cfg = loader(overlay).load()?;
            let workspaces = filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())?;
            let repos = daemon::daemon_repo_paths(&workspaces);
            let probe = workspaces
                .first()
                .and_then(|w| w.resolved_base_dir().ok())
                .unwrap_or(std::env::current_dir()?);
            let pressured = cargo_target::disk_pressured_at(&probe, 8);
            let report = cargo_target::sweep(
                &cfg.cargo_target,
                &repos,
                pressured,
                mode,
                std::time::SystemTime::now(),
            );
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                cargo_target::print_report(&report, &cfg.cargo_target);
            }
            if report.entries.iter().any(|e| e.removed == Some(false)) {
                std::process::exit(1);
            }
        }

        Commands::Worktree { action } => {
            let here = std::env::current_dir()?;
            match action {
                WorktreeAction::Enter { repo, session } => {
                    let repo = repo.unwrap_or(here);
                    // MINT when absent. CLAUDE_CODE_SESSION_ID exists only
                    // INSIDE a session, so a launch has none and erroring here
                    // makes the verb unusable as a launcher.
                    let session = session
                        .or_else(worktree::session_from_env)
                        .unwrap_or_else(worktree::mint_slug_now);
                    let path = worktree::enter(&repo, &session, &worktree::default_root(&repo))?;
                    println!("{}", path.display());
                }
                WorktreeAction::Session {
                    repo,
                    session,
                    command,
                } => {
                    // DELEGATES to `claude --worktree` rather than competing with
                    // it. Claude Code creates the worktree itself — same location
                    // (<repo>/.claude/worktrees/<name>), same branch prefix, and
                    // it locks the result. Two mechanisms creating worktrees in
                    // one repo is friction with no upside, so tend supplies only
                    // what it uniquely knows (a stable per-session name) and lets
                    // claude do the creating. tend's job is the LIFECYCLE the
                    // native flag has no answer for: list / land / prune.
                    //
                    // Non-claude commands still get a tend-made worktree, since
                    // `--worktree` is claude's flag and nothing else has it.
                    let repo = repo.unwrap_or(here);
                    // MINT when absent — same reason as Enter. This is THE
                    // launcher path, so it must work in a plain interactive
                    // shell, which never carries CLAUDE_CODE_SESSION_ID.
                    let session = session
                        .or_else(worktree::session_from_env)
                        .unwrap_or_else(worktree::mint_slug_now);
                    let slug = worktree::session_slug(&session);

                    // Empty command, or one that is only FLAGS, means "claude
                    // with these extra args" — delegate, forwarding them. Only
                    // the empty case delegated before, so the launcher the
                    // operator actually types
                    // (`claude --dangerously-skip-permissions`) had no isolated
                    // form and `session -- --dangerously-skip-permissions` tried
                    // to exec a flag as a program.
                    if worktree::is_claude_args(&command) {
                        // `--worktree` ONLY inside a work tree: claude cannot
                        // honour it elsewhere, and this verb is the operator's
                        // default launcher everywhere, so a non-repo cwd must
                        // degrade to a plain claude rather than fail.
                        let mut args = Vec::new();
                        if worktree::is_inside_work_tree(&repo) {
                            args.push(String::from("--worktree"));
                            args.push(slug);
                        } else {
                            eprintln!("tend: not a git work tree — no isolation available here");
                        }
                        args.extend(command.iter().cloned());
                        eprintln!("tend: delegating to `claude {}`", args.join(" "));
                        worktree::exec_in(&repo, "claude", &args)?;
                        unreachable!("exec_in only returns on failure");
                    }

                    let path = worktree::enter(&repo, &session, &worktree::default_root(&repo))?;
                    let (cmd, args) = command.split_first().map_or_else(
                        || (String::from("claude"), Vec::new()),
                        |(c, rest)| (c.clone(), rest.to_vec()),
                    );
                    eprintln!("tend: isolated worktree {}", path.display());
                    worktree::exec_in(&path, &cmd, &args)?;
                    unreachable!("exec_in only returns on failure");
                }

                WorktreeAction::Land {
                    repo,
                    session,
                    base,
                } => {
                    let repo = repo.unwrap_or(here);
                    let session = session.or_else(worktree::session_from_env).ok_or_else(|| {
                        anyhow::anyhow!(
                            "no session id: pass --session or set CLAUDE_CODE_SESSION_ID"
                        )
                    })?;
                    // RESOLVE, never create. This called `enter`, which creates
                    // on demand — so landing a session whose worktree directory
                    // was gone built a fresh one, and `enter`'s old `-B` reset
                    // the branch holding the only copy of the work. Landing acts
                    // ON existing work; nothing to find is an error, not a cue
                    // to invent one.
                    let path = worktree::resolve(&worktree::default_root(&repo), &session)?;
                    let sha = worktree::land(&path, &base)?;
                    println!("landed {sha} onto {base}");
                }

                WorktreeAction::List { repo } => {
                    let repo = repo.unwrap_or(here);
                    for e in worktree::list(&repo)? {
                        println!(
                            "{}  {}  {}",
                            e.branch,
                            e.path.display(),
                            e.verdict().reason()
                        );
                    }
                }
                WorktreeAction::Prune { repo, dry_run } => {
                    let repo = repo.unwrap_or(here);
                    for e in worktree::list(&repo)? {
                        let v = e.verdict();
                        if v.is_safe() {
                            if dry_run {
                                println!("would remove {}", e.path.display());
                            } else {
                                worktree::remove(&repo, &e)?;
                                println!("removed {}", e.path.display());
                            }
                        } else {
                            println!("kept {}  {}", e.path.display(), v.reason());
                        }
                    }
                }
            }
        }

        Commands::Sync {
            workspace: ws_filter,
            quiet,
            refresh,
        } => {
            let cfg = loader(none()).load()?;
            let mut degraded = reach::Degradations::default();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                let Some(repos) = sync::resolve_or_degrade(ws, refresh, &mut degraded).await
                else {
                    continue;
                };
                let (cloned, present, failed) = sync::sync_repos(ws, &repos, quiet).await?;
                if !quiet || cloned > 0 {
                    display::print_sync_summary(&ws.name, cloned, present, failed);
                }
            }
            // No in-band channel on this surface, so the exit code carries it.
            // Silence plus exit 0 would read as "nothing to do", which is the
            // confidently-wrong direction: the workspaces we could not see may
            // have held all the work. See reach::Degradations.
            degraded.report();
            if degraded.should_fail_exit() {
                std::process::exit(1);
            }
        }

        Commands::Pull {
            workspace: ws_filter,
            quiet,
            refresh,
        } => {
            let cfg = loader(none()).load()?;
            let mut degraded = reach::Degradations::default();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                let Some(repos) = sync::resolve_or_degrade(ws, refresh, &mut degraded).await
                else {
                    continue;
                };
                let summary = sync::pull_repos(ws, &repos, quiet).await?;
                display::print_pull_summary(&ws.name, &summary);
            }
            // No in-band channel on this surface, so the exit code carries it.
            // Silence plus exit 0 would read as "nothing to do", which is the
            // confidently-wrong direction: the workspaces we could not see may
            // have held all the work. See reach::Degradations.
            degraded.report();
            if degraded.should_fail_exit() {
                std::process::exit(1);
            }
        }

        Commands::NixpkgsAlign {
            workspace: ws_filter,
            repos: only,
            apply,
        } => {
            use crate::nixpkgs_align::{align_one_repo, substrate_canonical_rev, AlignReport};
            let cfg = loader(none()).load()?;
            let git = crate::git::SystemGitOps;
            let mut degraded = reach::Degradations::default();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                let base_dir = ws.resolved_base_dir()?;
                let Some(canonical) = substrate_canonical_rev(&base_dir.join("substrate")) else {
                    eprintln!(
                        "[{}] substrate not found at {} — can't read the canonical nixpkgs rev; skipping",
                        ws.name,
                        base_dir.join("substrate").display()
                    );
                    continue;
                };
                let Some(repos) = sync::resolve_or_degrade(ws, false, &mut degraded).await
                else {
                    continue;
                };
                let mut report = AlignReport::default();
                let mut absent = 0u32;
                for repo in repos.iter().filter(|r| only.is_empty() || only.contains(r)) {
                    let repo_path = base_dir.join(repo);
                    if !repo_path.join(".git").exists() {
                        absent += 1;
                        continue;
                    }
                    report.record(repo, align_one_repo(&repo_path, &canonical, &git, apply));
                }
                let mode = if apply { "apply" } else { "plan" };
                for repo in &report.aligned {
                    println!("  OK    {repo}");
                }
                for repo in &report.would_align {
                    println!("  PLAN  {repo}");
                }
                for (repo, rev) in &report.stale {
                    println!("  STALE {repo}: follows substrate/nixpkgs, locked to {rev}");
                }
                for repo in &report.dirty {
                    println!("  DIRTY {repo} (WIP untouched)");
                }
                for (repo, why) in &report.blind {
                    println!("  BLIND {repo}: {why}");
                }
                for (repo, why) in &report.failed {
                    println!("  FAIL  {repo}: {why}");
                }
                let by_design = report
                    .by_design
                    .iter()
                    .map(|(why, n)| [*why, "=", n.to_string().as_str()].concat())
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "[{}] nixpkgs-align ({mode}) -> {canonical}: aligned={} would-align={} converged={} stale={} by-design={} ({by_design}) dirty={} blind={} failed={} not-a-flake={} not-cloned={absent}",
                    ws.name,
                    report.aligned.len(),
                    report.would_align.len(),
                    report.converged,
                    report.stale.len(),
                    report.by_design.values().sum::<usize>(),
                    report.dirty.len(),
                    report.blind.len(),
                    report.failed.len(),
                    report.not_flakes,
                );
            }
            // No in-band channel on this surface, so the exit code carries it.
            // Silence plus exit 0 would read as "nothing to do", which is the
            // confidently-wrong direction: the workspaces we could not see may
            // have held all the work. See reach::Degradations.
            degraded.report();
            if degraded.should_fail_exit() {
                std::process::exit(1);
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
                        drift::DriftEvent::RemoteUrlEmbeddedCredential { .. } => {
                            "remote-url-embedded-credential"
                        }
                        drift::DriftEvent::RemoteProtocolMismatch { .. } => {
                            "remote-protocol-mismatch"
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
                        drift::DriftEvent::RemoteUrlEmbeddedCredential { repo_name, credential, .. } => Some(format!(
                            "  CREDENTIAL IN REMOTE [{repo_name}]: origin embeds {credential} — a token is stored in plaintext in .git/config and was live when the repo was cloned. \
                             `tend reconcile` rewrites the remote to the canonical URL automatically (the target repo does not change; only the credential is dropped). \
                             Rewriting does NOT undo the exposure: rotate the token if it is still valid"
                        )),
                        drift::DriftEvent::RemoteProtocolMismatch { repo_name, declared, actual, .. } => Some(format!(
                            "  remote protocol [{repo_name}]: origin uses {actual} but the workspace declares {declared} — `tend reconcile` converges it to the declared method"
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

        Commands::Adopt { workspace, repo } => {
            // Validate the workspace exists + the repo dir is on disk
            // BEFORE touching the config file, so a typo doesn't
            // corrupt the YAML.
            let adopt_loader = loader(none());
            let cfg = adopt_loader.load()?;
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
                // The last `--config`, else the discovered file: the file
                // the operator owns, never a merged view of several.
                let cfg_path = adopt_loader.edit_target();
                let content = std::fs::read_to_string(&cfg_path)
                    .with_context(|| format!("reading {}", cfg_path.display()))?;
                let mut doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&content)
                    .with_context(|| format!("parsing {}", cfg_path.display()))?;

                let workspaces = doc
                    .get_mut("workspaces")
                    .and_then(|v| v.as_sequence_mut())
                    .ok_or_else(|| anyhow::anyhow!("config has no `workspaces:` sequence"))?;

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
                    let map = ws_val
                        .as_mapping_mut()
                        .ok_or_else(|| anyhow::anyhow!("workspace entry is not a mapping"))?;
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

                let serialized =
                    serde_yaml_ng::to_string(&doc).with_context(|| "re-serializing config YAML")?;
                std::fs::write(&cfg_path, &serialized)
                    .with_context(|| format!("writing {}", cfg_path.display()))?;
                println!(
                    "adopted {}/{} into config at {} (extra_repos updated)",
                    workspace,
                    repo,
                    cfg_path.display()
                );
            }
        }

        Commands::Report { log, window_hours } => {
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
            workspace: ws_filter,
            refresh,
            flags,
        } => {
            let cfg = loader(flags::Overlay::overlay(&flags)).load()?;
            let max_inflight = cfg.reconcile.max_inflight;
            let mut any_failed = false;
            // Same transition log as the daemon path — operator can
            // grep one file across both `tend daemon` and `tend reconcile`.
            let transition_log_path = audit::AuditLog::default_path()
                .path()
                .parent()
                .map(|p| p.join("scheduler-transitions.jsonl"));
            let mut degraded = reach::Degradations::default();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                let Some(repos) = sync::resolve_or_degrade(ws, refresh, &mut degraded).await
                else {
                    continue;
                };
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
            // No in-band channel on this surface, so the exit code carries it.
            // Silence plus exit 0 would read as "nothing to do", which is the
            // confidently-wrong direction: the workspaces we could not see may
            // have held all the work. See reach::Degradations.
            degraded.report();
            if degraded.should_fail_exit() {
                std::process::exit(1);
            }
            if any_failed {
                std::process::exit(1);
            }
        }

        Commands::Status {
            workspace: ws_filter,
            refresh,
            json,
            problems,
            no_fix,
        } => {
            // `--no-fix` is `host_health.fix: false` for this invocation.
            let cfg = loader(flags::status_overlay(no_fix)).load()?;
            let mut rows: Vec<display::StatusJsonRow> = Vec::new();
            // Every worktree this pass actually saw -- the input to the
            // orphaned-index.lock reap below. Collected here rather than
            // rediscovered, so the reap can only ever touch a repo tend
            // already manages.
            let mut seen_repos: Vec<std::path::PathBuf> = Vec::new();
            let mut degraded = reach::Degradations::default();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                let Some(repos) = sync::resolve_or_degrade(ws, refresh, &mut degraded).await
                else {
                    continue;
                };
                let entries = sync::check_status(ws, &repos).await?;
                let base_dir = ws.resolved_base_dir()?;
                seen_repos.extend(
                    entries
                        .iter()
                        .filter(|e| e.status != sync::RepoStatus::Missing)
                        .map(|e| base_dir.join(&e.name)),
                );
                if json {
                    rows.extend(
                        entries
                            .iter()
                            .map(|e| display::StatusJsonRow::new(e, &base_dir))
                            .filter(|row| !(problems && row.is_quiet())),
                    );
                } else {
                    display::print_status(&ws.name, &entries, problems);
                }
            }
            if json {
                // ── ★ BLINDNESS GOES IN THE ARRAY, NOT IN THE EXIT CODE ──
                // This is the one surface with an in-band channel for it: one
                // synthetic row per workspace we could not see, so a short
                // array can never be mistaken for "those repos are all fine".
                // izumi ranks an unrecognised state High, so it surfaces at
                // the top of the board rather than sinking.
                //
                // Exit stays 0 deliberately. izumi treats a non-zero `tend`
                // as Unavailable and would DISCARD the partial data — which
                // is strictly worse than showing four workspaces plus a loud
                // row saying the fifth is unreadable.
                rows.extend(
                    degraded
                        .iter()
                        .map(|(ws, answer)| display::StatusJsonRow::unreachable(ws, answer)),
                );
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                // The human table has no row for a whole missing workspace,
                // so it gets the summary instead — with its denominator, so
                // "3 workspaces" is never read without "of 5".
                degraded.report();
                let host_audit = audit::AuditLog::default_path();
                let report = host_health::run_host_health_check(
                    &host_health::SystemProcessLister,
                    &host_health::SystemProcessKiller,
                    &host_health::SystemSysctlReader,
                    &host_health::SystemLockProbe,
                    &cfg.host_health.watched_commands,
                    cfg.host_health.fd_pressure_threshold,
                    cfg.host_health.fix,
                    &seen_repos,
                    cfg.host_health.stale_lock_min_age_secs,
                    &host_audit,
                );
                if !report.stale_locks.is_empty() {
                    println!();
                    println!(
                        "Host health: {} repo(s) wedged by an orphaned .git/index.lock. Logged to {}:",
                        report.stale_locks.len(),
                        host_audit.path().display()
                    );
                    for l in &report.stale_locks {
                        println!("  {} (lock {}s old)", l.repo.display(), l.age_secs);
                    }
                    if report.lock_reap_outcomes.is_empty() {
                        println!(
                            "  (not reaped -- pass without --no-fix, or set host_health.fix: true)"
                        );
                    } else {
                        for (l, outcome) in &report.lock_reap_outcomes {
                            let desc = match outcome {
                                host_health::LockReapOutcome::Removed => "removed".to_string(),
                                host_health::LockReapOutcome::AlreadyGone => {
                                    "already gone".to_string()
                                }
                                host_health::LockReapOutcome::Failed(e) => format!("FAILED: {e}"),
                            };
                            println!("  {} reap: {}", l.repo.display(), desc);
                        }
                    }
                }
                if !report.orphans.is_empty() {
                    println!();
                    println!(
                        "Host health: {} orphaned process(es) (PPID==1). Logged to {}:",
                        report.orphans.len(),
                        host_audit.path().display()
                    );
                    for o in &report.orphans {
                        println!(
                            "  pid {} etime {} tty {} -- {}",
                            o.pid, o.etime, o.tty, o.command
                        );
                    }
                    if report.reap_outcomes.is_empty() {
                        println!(
                            "  (not reaped -- pass without --no-fix, or set host_health.fix: true)"
                        );
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
            workspace: ws_filter,
            refresh,
        } => {
            let cfg = loader(none()).load()?;
            let mut degraded = reach::Degradations::default();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                let Some(repos) = sync::resolve_or_degrade(ws, refresh, &mut degraded).await
                else {
                    continue;
                };
                display::print_repo_list(&ws.name, &repos);
            }
            // No in-band channel on this surface, so the exit code carries it.
            // Silence plus exit 0 would read as "nothing to do", which is the
            // confidently-wrong direction: the workspaces we could not see may
            // have held all the work. See reach::Degradations.
            degraded.report();
            if degraded.should_fail_exit() {
                std::process::exit(1);
            }
        }

        Commands::Discover { org, provider: _ } => {
            let repos = provider::discover_github_repos(&org).await?;
            display::print_discover_results(&org, &repos);
        }

        Commands::FlakeUpdate {
            changed,
            all,
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

            let cfg = loader(none()).load()?;
            let opts = flake::ExecOptions {
                // Verification ON by default — an unverified lock is how a
                // withdrawn upstream reached main and stopped the fleet.
                skip_verify: false,
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

            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                if ws.flake_deps.is_empty() {
                    continue;
                }

                let (label, chain) = if all {
                    (
                        "(all)".to_string(),
                        flake::compute_update_chain_all(&ws.flake_deps)?,
                    )
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
                // ── ★ THE FOURTH FIELD. Dropping it made an all-blocked run
                // report success: `blocked` was counted in the chain and
                // discarded here, so the CLI printed "done", exited 0, and
                // the daemon doubled its backoff as if the fleet had
                // converged. Counting a thing and then not reading it is
                // the same defect as never counting it.
                summary.blocked += ws_summary.blocked;

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
                        println!(
                            "{}: running cargo updates for {} dirty Rust repos",
                            ws.name,
                            dirty_cargo_repos.len()
                        );
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
                        println!(
                            "{}: {} cargo updates committed and pushed",
                            ws.name, cargo_summary.updated
                        );
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
                    display::print_flake_chain_complete(ws_summary.updated, ws_summary.blocked);
                }
            }

            if all && !quiet {
                println!(
                    "\nsummary: {} updated, {} no-change, {} skipped, {} blocked",
                    summary.updated, summary.no_change, summary.skipped, summary.blocked
                );
                // ── ★ A BLOCKED RUN MUST NOT EXIT 0 ──────────────────────
                // `had_blocked`'s own doc promised "the caller reports a
                // non-zero exit so an automated run cannot look successful
                // while silently having skipped half the fleet" — and no
                // caller read it. CI, a cron wrapper, or `&&` in a shell
                // all treated a fully-blocked fleet as a clean run.
                if summary.had_blocked() {
                    anyhow::bail!(
                        "{} repo(s) blocked — see the BLOCKED lines above; nothing behind them was skipped",
                        summary.blocked
                    );
                }
            }
        }

        Commands::FlakeUpdateDaemon {
            workspace: ws_filter,
            flags,
        } => {
            run_flake_update_daemon(loader(flags::Overlay::overlay(&flags)), ws_filter).await?;
        }

        Commands::Prebuild {
            workspace: ws_filter,
            flags,
        } => {
            let cfg = loader(flags::Overlay::overlay(&flags)).load()?;
            let opts = prebuild::PrebuildOptions::from_config(&cfg.prebuild)?;
            let audit = audit::AuditLog::default_path();
            let summary = prebuild::run_cycle(&cfg, ws_filter.as_deref(), &opts, &audit).await?;
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
            workspace: ws_filter,
            flags,
        } => {
            run_prebuild_daemon(loader(flags::Overlay::overlay(&flags)), ws_filter).await?;
        }

        Commands::Watch {
            workspace: ws_filter,
            refresh: _refresh,
        } => {
            let cfg = loader(none()).load()?;
            let audit_log = audit::AuditLog::default_path();
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
                if let Some(ref watch_cfg) = ws.watch {
                    if watch_cfg.enable {
                        let gh = github::HttpGitHubClient::new()?;
                        let cache_store = watch_cache::FsWatchStateStore;
                        let matrix_appender = watch::TomlMatrixAppender;
                        let git_ops = git::SystemGitOps;

                        let summary = watch::run_watch_cycle(
                            ws,
                            false,
                            &gh,
                            &cache_store,
                            &matrix_appender,
                            &git_ops,
                            &audit_log,
                        )
                        .await?;
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
                    let evt = entry.get("event").and_then(|v| v.as_str()).unwrap_or("?");

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
            workspace: ws_filter,
            flags,
        } => {
            // No token plumbing here any more: `--github-token-file` (now on
            // every subcommand) is a partial over `github_auth`, resolved on
            // each use and re-read every cycle — see src/gh_auth.rs.
            // Open the kanshou introspection socket so operators can
            // query the live tend daemon — ticks completed, current
            // workspace/repo, pull/fetch counters — via
            // `gen kanshou query tend <field>`. Best-effort: bind
            // failure logs and continues so introspection-disabled
            // mode is graceful.
            let kanshou_state = std::sync::Arc::new(kanshou_state::TendDaemonState::new());
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
                    loader: loader(flags::Overlay::overlay(&flags)),
                    workspace: ws_filter,
                },
                kanshou_state,
            )
            .await?;
        }

        Commands::ReleaseSwarmPlan {
            workspace: ws_filter,
        } => {
            let cfg = loader(none()).load()?;
            let audit_log = audit::AuditLog::default_path();
            let mut total_eligible = 0usize;
            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
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
            workspace: ws_filter,
            dry_run,
        } => {
            let cfg = loader(none()).load()?;
            let audit_log = audit::AuditLog::default_path();
            // Render fn stub — produces the canonical 3-target workflow YAML
            // derived from repo_name + binary_name. Later swapped for a call
            // into arch-synthesizer's RustToolPublicReleaseDecl::render().
            let render = |repo_name: &str, repo_cfg: &release_swarm::RepoReleaseConfig| {
                render_rust_tool_release_workflow_yaml(repo_name, repo_cfg)
            };

            for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter.as_deref())? {
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
                    let token = gh_auth::token().ok_or_else(|| {
                        anyhow::anyhow!(
                            "no GitHub token from `github_auth` — release-swarm apply needs \
                             one with repo write scope (see `tend config-show --effective`)"
                        )
                    })?;
                    let api = release_swarm_http::HttpReleaseSwarmApi::new(
                        token.expose_for_header().to_string(),
                    )?;
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
        Commands::Throttle { worker_config } => {
            operator::throttle::run(worker_config.as_deref()).await?;
        }

        Commands::ConfigShow(cmd) => {
            if cmd.effective {
                // The same fold every subcommand runs, with this
                // invocation's --config/--set/credential flags applied.
                let resolved = loader(none()).resolve()?;
                print!(
                    "{}",
                    cmd.render_effective(&resolved)
                        .map_err(|e| anyhow::anyhow!("config-show failed: {e}"))?
                );
            } else {
                cmd.run::<config::Config>("TEND_TIER")
                    .map_err(|e| anyhow::anyhow!("config-show failed: {e}"))?;
            }
        }

        Commands::ConfigSchema => {
            println!("{}", serde_json::to_string_pretty(&schema::config_schema())?);
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

/// Resolve the config with no flag overlay — the K8s operator's and MCP's
/// entry: the discovered file (or `path` as a `--config` override), env.
pub(crate) fn load_config(path: Option<&std::path::Path>) -> Result<config::Config> {
    let args = shikumi::cli::ConfigArgs {
        config: path.map(std::path::Path::to_path_buf).into_iter().collect(),
        set: Vec::new(),
    };
    config_layers::ConfigLoader::new(args, serde_json::Value::Null).load()
}

async fn run_flake_update_daemon(
    loader: config_layers::ConfigLoader,
    ws_filter: Option<String>,
) -> Result<()> {
    // Pacing is config (`flake_update_daemon.*`), re-resolved every cycle
    // with the same flag overlay, so an edit takes effect without a restart
    // while a flag keeps beating the file.
    let pacing = |cfg: &config::FlakeUpdateDaemonConfig| {
        let min = cfg.min_interval.max(1);
        (min, cfg.max_interval.max(min), cfg.quiet)
    };
    let first = loader.load()?;
    let (min, max, quiet) = pacing(&first.flake_update_daemon);
    let mut interval = min;
    let audit_log = audit::AuditLog::default_path();

    if !quiet {
        println!("flake-update daemon starting (min={min}s max={max}s). Ctrl-C to stop.");
    }

    loop {
        let cycle_start = std::time::Instant::now();
        audit_log.log(
            "flake_update_cycle_start",
            serde_json::json!({ "interval_secs": interval }),
        );

        let cycle = match loader.load() {
            Ok(cfg) => {
                gh_auth::credentials().begin_cycle();
                let (min, max, quiet) = pacing(&cfg.flake_update_daemon);
                (
                    min,
                    max,
                    quiet,
                    run_flake_update_cycle(&cfg, ws_filter.as_deref(), quiet).await,
                )
            }
            Err(e) => (min, max, quiet, Err(e)),
        };
        let (min, max, quiet, outcome) = cycle;
        match outcome {
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
                if !summary.converged() {
                    interval = min;
                } else {
                    interval = (interval.saturating_mul(2)).clamp(min, max);
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
    cfg: &config::Config,
    ws_filter: Option<&str>,
    quiet: bool,
) -> Result<flake::ExecSummary> {
    let opts = flake::ExecOptions {
        // Verification ON by default — an unverified lock is how a
        // withdrawn upstream reached main and stopped the fleet.
        skip_verify: false,
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
    for ws in filter_workspaces_checked(&cfg.workspaces, ws_filter)? {
        if ws.flake_deps.is_empty() {
            continue;
        }
        let chain = flake::compute_update_chain_all(&ws.flake_deps)?;
        if chain.is_empty() {
            continue;
        }
        let (chain, _dropped) = flake::filter_to_divergent(ws, chain, Some(&upstream)).await?;
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
        // See the CLI site: dropping `blocked` here made the DAEMON treat a
        // fully-blocked fleet as converged and double its sleep interval.
        summary.blocked += ws_summary.blocked;
    }
    Ok(summary)
}

async fn run_prebuild_daemon(
    loader: config_layers::ConfigLoader,
    ws_filter: Option<String>,
) -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::Notify;

    // Pacing, probe and fill settings are the top-level `prebuild:` config,
    // re-resolved each cycle with the same flag overlay — so a flag beats
    // the file, and a file edit lands without a restart.
    let first = loader.load()?;
    let pacing = |p: &config::PrebuildConfig| {
        let min = p.min_interval.max(1);
        (min, p.max_interval.max(min))
    };
    let (mut min, mut max) = pacing(&first.prebuild);
    let mut reachability = prebuild::ReachabilityOptions::from_config(&first.prebuild);
    let quiet = first.prebuild.quiet;
    let max_inflight = first.prebuild.max_inflight;
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
    // Watch the first file the fold reads (the discovered file, else the
    // first --config); with none on disk, the path `tend init` would create.
    let resolved_path: PathBuf = loader
        .files()
        .into_iter()
        .next()
        .unwrap_or_else(config::Config::default_path);

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
            let reachable =
                prebuild::probe_attic_reachable(&reachability.url, reachability.probe_timeout)
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
            let cfg = loader.load()?;
            (min, max) = pacing(&cfg.prebuild);
            reachability = prebuild::ReachabilityOptions::from_config(&cfg.prebuild);
            let opts = prebuild::PrebuildOptions::from_config(&cfg.prebuild)?;
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

/// `filter_workspaces`, but a name that matches nothing is an ERROR.
///
/// ── ★ A TYPO MUST NOT BE A SUCCESSFUL NO-OP ─────────────────────────
/// `filter_workspaces` returns an empty Vec when an explicit `--workspace`
/// name matches no configured workspace, and all eleven subcommands then
/// iterate zero workspaces, print nothing, and exit 0. `tend sync
/// --workspace plemeio` (missing hyphen) reports success having done
/// nothing at all — and so does the daemon, forever, on a stale name.
///
/// The empty-filter case is untouched: no `--workspace` legitimately means
/// all of them.
pub(crate) fn filter_workspaces_checked<'a>(
    workspaces: &'a [config::Workspace],
    filter: Option<&str>,
) -> anyhow::Result<Vec<&'a config::Workspace>> {
    let selected = filter_workspaces(workspaces, filter);
    if let Some(name) = filter {
        if selected.is_empty() {
            let known: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
            anyhow::bail!(
                "no workspace named `{name}` — known workspaces: {}. \
                 (An unmatched filter used to be a silent, successful no-op.)",
                if known.is_empty() {
                    "<none configured>".to_owned()
                } else {
                    known.join(", ")
                }
            );
        }
    }
    Ok(selected)
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
    let binary_name = repo_cfg
        .binary_name
        .clone()
        .unwrap_or_else(|| repo_name.to_string());
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

    /// The RAW filter still returns empty for an unmatched name — that is
    /// its contract and other callers rely on it.
    #[test]
    fn test_filter_workspaces_with_nonexistent_name() {
        let workspaces = vec![
            config::Workspace::test_default("ws-a"),
            config::Workspace::test_default("ws-b"),
        ];
        let filtered = filter_workspaces(&workspaces, Some("ws-z"));
        assert!(filtered.is_empty());
    }

    /// ── ★ BUT AN UNMATCHED --workspace IS AN ERROR AT THE CLI ───────────
    /// Every subcommand used the raw filter, so `tend sync --workspace
    /// plemeio` (missing hyphen) iterated zero workspaces, printed nothing
    /// and exited 0 — a typo reported as success. The daemon did the same,
    /// forever, on a stale name.
    ///
    /// Red run: point the call sites back at `filter_workspaces` and a
    /// misspelled workspace is a clean exit again.
    #[test]
    fn an_unmatched_workspace_name_is_an_error_not_an_empty_success() {
        let workspaces = vec![
            config::Workspace::test_default("ws-a"),
            config::Workspace::test_default("ws-b"),
        ];
        let err = filter_workspaces_checked(&workspaces, Some("ws-z"))
            .expect_err("an unmatched explicit name must not succeed");
        let msg = err.to_string();
        assert!(msg.contains("ws-z"), "names the bad filter: {msg}");
        // Naming the alternatives is the difference between an error and a
        // useful one — the operator almost always mistyped a real name.
        assert!(
            msg.contains("ws-a") && msg.contains("ws-b"),
            "lists known: {msg}"
        );
    }

    /// No filter still means all of them — the fix must not turn "run
    /// everywhere" into an error.
    #[test]
    fn no_filter_is_still_every_workspace() {
        let workspaces = vec![
            config::Workspace::test_default("ws-a"),
            config::Workspace::test_default("ws-b"),
        ];
        assert_eq!(
            filter_workspaces_checked(&workspaces, None)
                .expect("no filter is valid")
                .len(),
            2
        );
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
        std::fs::write(
            &path,
            "workspaces:\n  - name: a\n    base_dir: /a\n  - name: b\n    base_dir: /b\n",
        )
        .unwrap();
        let cfg = load_config(Some(&path)).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
