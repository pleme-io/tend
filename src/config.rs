use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use shikumi::{ConfigDiscovery, Format};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiTaskConfig {
    pub name: String,
    pub schedule: String,
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_retries() -> u32 {
    3
}

fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub host_health: HostHealthConfig,
}

/// Host-level (not per-workspace) resource-hygiene knobs read by
/// `tend status` -- see `src/host_health.rs`. Absent from a config file
/// entirely, or with any field omitted, falls back to the defaults below.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostHealthConfig {
    /// Binary name fragments known to leave orphaned (PPID==1) processes
    /// behind under some failure mode -- extend as new patterns are found
    /// on the fleet. Default: `["frost"]` (the confirmed 2026-07-12 case).
    #[serde(default = "default_watched_commands")]
    pub watched_commands: Vec<String>,
    /// Fraction of `kern.maxfiles` (0.0-1.0) at which `tend status` warns
    /// about system-wide fd pressure, independent of any specific process.
    /// Default 0.5 -- the actual 2026-07-12 incident hit ~0.99997, so this
    /// gives a wide lead-time margin before the kernel table actually fills.
    #[serde(default = "default_fd_pressure_threshold")]
    pub fd_pressure_threshold: f64,
    /// Whether `tend status` SIGKILLs confirmed orphans it finds (PPID==1
    /// AND name-matched against `watched_commands`) rather than only
    /// reporting them. Default true: this match is narrow enough (no
    /// owning session exists any more) that killing it is a return to
    /// baseline, not an interruption of anything live. `--no-fix` at the
    /// CLI overrides this off for a single invocation.
    #[serde(default = "default_host_health_fix")]
    pub fix: bool,
    /// How old (seconds) an orphaned `.git/index.lock` must be before
    /// `tend status` will reap it. A real git operation holds its lock for
    /// milliseconds, so the default 120 is ~3 orders of magnitude of slack
    /// -- generous on purpose, since reaping a live lock corrupts an index
    /// while waiting one more pass costs nothing.
    #[serde(default = "default_stale_lock_min_age_secs")]
    pub stale_lock_min_age_secs: u64,
}

impl Default for HostHealthConfig {
    fn default() -> Self {
        Self {
            watched_commands: default_watched_commands(),
            fd_pressure_threshold: default_fd_pressure_threshold(),
            fix: true,
            stale_lock_min_age_secs: default_stale_lock_min_age_secs(),
        }
    }
}

fn default_watched_commands() -> Vec<String> {
    vec!["frost".to_string()]
}

fn default_fd_pressure_threshold() -> f64 {
    0.5
}

fn default_host_health_fix() -> bool {
    true
}

fn default_stale_lock_min_age_secs() -> u64 {
    120
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub name: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    pub base_dir: String,
    #[serde(default = "default_clone_method")]
    pub clone_method: CloneMethod,
    #[serde(default)]
    pub discover: bool,
    #[serde(default)]
    pub org: Option<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub extra_repos: Vec<String>,
    #[serde(default)]
    pub flake_deps: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub watch: Option<WatchConfig>,
    #[serde(default)]
    pub ai_tasks: Vec<AiTaskConfig>,
    /// Workspace-scoped prebuild daemon settings. When set, the
    /// `tend prebuild-daemon` reads these on every cycle (re-load
    /// is per-cycle, so dynamic edits take effect within one
    /// interval — no daemon restart needed). CLI flags still
    /// override; this is the typed-config layer.
    #[serde(default)]
    pub prebuild: Option<PrebuildConfig>,
}

/// Per-workspace prebuild knobs. See `src/prebuild.rs::PrebuildOptions`
/// for the runtime shape; this is the on-disk YAML projection that
/// shikumi's TieredConfig resolves (env > file > prescribed_default >
/// bare). The daemon reads this every cycle, so an operator can
/// edit `~/.config/tend/config.yaml` (or `/etc/tend/config.yaml`)
/// while the daemon runs and the next cycle picks up the change —
/// matches the "K8s controller, dynamic config propagates live"
/// shape called out in the project goals.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrebuildConfig {
    /// Minimum sleep between cycles (seconds). Resets after any work.
    #[serde(default = "default_prebuild_min_interval")]
    pub min_interval: u64,
    /// Maximum sleep when converged (seconds). Caps the exponential
    /// backoff growth.
    #[serde(default = "default_prebuild_max_interval")]
    pub max_interval: u64,
    /// Max concurrent `nix build` invocations.
    #[serde(default = "default_prebuild_max_inflight")]
    pub max_inflight: usize,
    /// Attic cache name to push closures to. `None` = build-only mode.
    #[serde(default)]
    pub attic_cache: Option<String>,
    /// Attic server alias for `attic login`.
    #[serde(default)]
    pub attic_server: Option<String>,
    /// Attic server URL.
    #[serde(default)]
    pub attic_url: Option<String>,
    /// SOPS-managed Attic JWT token file path.
    #[serde(default)]
    pub attic_token_file: Option<String>,

    // ── Cache-fill extensions (multi-cache, multi-package) ──────────
    /// Which flake outputs to build: `"all"` (every
    /// `packages.${system}.*` — the fill default), `"default"`, or a
    /// comma-separated allow-list (`"mado,tear"`). Parsed by
    /// [`crate::prebuild_cache::PackageSelector::parse`].
    #[serde(default = "default_prebuild_packages")]
    pub packages: String,
    /// Target systems to build for. Empty ⇒ this host's native system
    /// only (no remote-builder fan-out unless the operator opts in by
    /// listing extra triples like `x86_64-linux`).
    #[serde(default)]
    pub systems: Vec<String>,
    /// Reproducibility policy before pushing to a trusted cache:
    /// `"trusting"` (fast) or `"verify"` (build-and-compare; never
    /// pushes a non-reproducible artifact — the anti-poison gate).
    /// Parsed by [`crate::prebuild_cache::ReproPolicy::parse`].
    #[serde(default)]
    pub repro: String,
    /// Many caches. Each produced closure fans out to every enabled
    /// target. When empty, the legacy single `attic_*` quartet above is
    /// promoted to a one-element list, so existing configs keep working.
    #[serde(default)]
    pub caches: Vec<CacheTargetConfig>,
}

/// One push destination in the multi-cache `caches:` list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheTargetConfig {
    /// Which protocol this destination speaks (`attic` | `sui`).
    /// Absent ⇒ `attic`, so every existing `caches:` block keeps working.
    #[serde(default)]
    pub backend: crate::prebuild_cache::CacheBackend,
    /// atticd cache name (e.g. `nexus`). Attic-only — a `sui` target is
    /// addressed by `url` alone.
    #[serde(default)]
    pub cache: String,
    /// Server alias for `attic login`.
    pub server: String,
    /// Server root URL (e.g. `http://rio:8080/`).
    pub url: String,
    /// SOPS-managed JWT token file path.
    pub token_file: String,
    /// Disable without deleting. Defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl CacheTargetConfig {
    /// Lower to the runtime [`crate::prebuild_cache::CacheTarget`].
    #[must_use]
    pub fn to_target(&self) -> crate::prebuild_cache::CacheTarget {
        crate::prebuild_cache::CacheTarget {
            backend: self.backend,
            cache_name: self.cache.clone(),
            server_name: self.server.clone(),
            server_url: self.url.clone(),
            token_file: self.token_file.clone(),
            enabled: self.enabled,
        }
    }
}

fn default_prebuild_packages() -> String {
    "all".to_string()
}

fn default_prebuild_min_interval() -> u64 {
    120
}

fn default_prebuild_max_interval() -> u64 {
    3600
}

fn default_prebuild_max_inflight() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    /// Enable watch for this workspace
    #[serde(default)]
    pub enable: bool,
    /// Path to matrix.toml file to append entries to
    pub matrix_file: Option<String>,
    /// Run `akeyless-matrix certify` after appending pending entries
    #[serde(default)]
    pub auto_certify: bool,
    /// Auto commit+push all changes (matrix.toml + generated files)
    #[serde(default)]
    pub auto_commit: bool,
    /// Run `tend flake-update --changed <repo>` to propagate to dependent flakes
    #[serde(default)]
    pub auto_propagate: Option<String>,
    /// Post-hooks to run after watch cycle steps
    #[serde(default)]
    pub post_hooks: Vec<PostHook>,
    /// File watches: monitor specific files in GitHub repos for content changes
    #[serde(default)]
    pub file_watches: Vec<FileWatch>,
    /// Flake input watches: monitor flake.lock inputs against upstream for staleness
    #[serde(default)]
    pub flake_input_watches: Vec<FlakeInputWatch>,
    /// Flake refresh: periodically run `nix flake update` on all repos with flake.nix
    #[serde(default)]
    pub flake_refresh: Option<FlakeRefreshConfig>,
    /// Nix audit: run nix-audit convergence loop in daemon cycle
    #[serde(default)]
    pub nix_audit: Option<NixAuditConfig>,
    /// CI hygiene — tend's little tree trimmer for GitHub Actions queues.
    /// When enabled, the daemon (or the one-shot `tend ci-trim` command)
    /// scans workflow runs across the workspace's repos and cancels
    /// duplicate queued runs on the same tag/branch. Opt-in per workspace.
    #[serde(default)]
    pub ci_hygiene: Option<CiHygieneConfig>,
    /// Release swarm — apply the canonical `rust-tool-public-release`
    /// workflow across eligible Rust repos in this workspace's org.
    /// DENY by default at both the org level AND per-repo level —
    /// nothing is applied unless BOTH flags are explicitly `true`.
    /// See `src/release_swarm.rs` for policy + apply logic.
    #[serde(default)]
    pub release_swarm: Option<crate::release_swarm::OrgReleaseSwarmConfig>,
}

/// Per-workspace CI hygiene configuration. See `src/ci_trim.rs` for the
/// pure trim policy. When `enable: true`, the runner lists recent
/// workflow runs for each repo in the workspace + applies the policy.
///
/// Safe default: `auto_cancel_duplicate_queued: true` but `enable:
/// false` at the top level — the feature ships off-by-default and
/// each workspace opts in.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CiHygieneConfig {
    /// Enable CI hygiene for this workspace.
    #[serde(default)]
    pub enable: bool,
    /// Auto-cancel duplicate queued runs (older ones, keeping the
    /// newest). Default true when `enable: true`. Set false to
    /// observe + log only (the dry-run equivalent at config level).
    #[serde(default = "default_true")]
    pub auto_cancel_duplicate_queued: bool,
    /// Max number of recent workflow runs per repo to inspect each
    /// pass. Higher = more thorough but more API calls. Default 50.
    #[serde(default = "default_recent_run_limit")]
    pub recent_run_limit: u32,
    /// Optional allowlist of repo names. When set, only these repos
    /// (within the workspace's org) are scanned. When unset, every
    /// discovered repo in the org is scanned.
    #[serde(default)]
    pub repo_filter: Option<Vec<String>>,
    /// Alert after a queued run has been stuck for N minutes without
    /// allocation (observability; does not auto-cancel). Default 30.
    #[serde(default = "default_stale_queue_minutes")]
    pub stale_queue_alert_minutes: u32,
}

fn default_recent_run_limit() -> u32 {
    50
}

fn default_stale_queue_minutes() -> u32 {
    30
}

/// Configuration for nix-audit integration in the tend daemon.
///
/// When enabled, the daemon runs `nix-audit check --all` after the watch cycle,
/// optionally auto-fixes violations and propagates fixes across the flake graph.
/// Results are tracked in a convergence database for trend analysis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NixAuditConfig {
    /// Enable nix-audit integration in daemon cycle
    #[serde(default)]
    pub enable: bool,
    /// Path to convergence.db (default: ~/.local/share/nix-audit/convergence.db)
    #[serde(default)]
    pub db_path: Option<String>,
    /// Run `nix-audit fix --all --commit` when violations found
    #[serde(default)]
    pub auto_fix: bool,
    /// Trigger `tend flake-update` propagation after fixes
    #[serde(default)]
    pub auto_propagate: bool,
    /// Post-hooks with new triggers: "after_audit", "on_violation", "on_convergence"
    #[serde(default)]
    pub post_hooks: Vec<PostHook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileWatch {
    /// Human-readable name for this watch
    pub name: String,
    /// GitHub org/owner
    pub org: String,
    /// GitHub repo name
    pub repo: String,
    /// File path within the repo
    pub path: String,
    /// Local directory to download the file to (versioned: {download_to}/{sha[..12]}.{ext})
    #[serde(default)]
    pub download_to: Option<String>,
    /// Hooks to run when the file content changes
    #[serde(default)]
    pub post_hooks: Vec<PostHook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostHook {
    /// When to trigger: "after_certify", "after_commit", "after_propagate", "after_all"
    pub trigger: String,
    /// Shell command to run
    pub command: String,
    /// Arguments (supports $VERSION, $REPO, $REV, $MATRIX_FILE placeholders)
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory (supports ~ expansion)
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Continue if this hook fails
    #[serde(default)]
    pub continue_on_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeInputWatch {
    /// Human-readable name for this watch
    pub name: String,
    /// Local repo (relative to workspace base_dir) whose flake.lock to check
    pub repo: String,
    /// Flake input name in flake.lock
    pub input: String,
    /// "owner/repo" on GitHub — derived from flake.lock if omitted
    #[serde(default)]
    pub upstream: Option<String>,
    /// Watch mode: compare HEAD SHA (commits) or latest tag (tags)
    #[serde(default = "default_flake_input_mode")]
    pub mode: FlakeInputMode,
    /// Run `nix flake update <input>` when stale
    #[serde(default)]
    pub auto_update: bool,
    /// Commit + push flake.lock after update
    #[serde(default)]
    pub auto_commit: bool,
    /// Run `tend flake-update --changed <repo>` to propagate
    #[serde(default)]
    pub auto_propagate: Option<String>,
    /// Hooks to run when staleness is detected
    #[serde(default)]
    pub post_hooks: Vec<PostHook>,
}

impl Default for FlakeRefreshConfig {
    fn default() -> Self {
        Self {
            enable: false,
            interval: default_refresh_interval(),
            max_interval: default_max_interval(),
            branch: default_branch(),
            pull_before_update: true,
            update_command: default_update_command(),
            update_timeout: default_update_timeout(),
            commit_message: default_commit_message(),
            // Opt-in, matching the serde default above. Both had to change:
            // serde governs a config file that omits the key, this governs
            // a programmatically-constructed config, and leaving either at
            // `true` keeps an unverified writer on by default.
            auto_commit: false,
            auto_propagate: false,
            include: Vec::new(),
            exclude: Vec::new(),
            post_hooks: Vec::new(),
            staleness_check: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakeRefreshConfig {
    /// Enable flake refresh for this workspace
    #[serde(default)]
    pub enable: bool,
    /// Base cooldown per repo in seconds (default: 3600 = 1 hour).
    /// Actual interval grows via adaptive backoff when no changes are found.
    #[serde(default = "default_refresh_interval")]
    pub interval: u64,
    /// Maximum cooldown per repo in seconds after adaptive backoff (default: 86400 = 24 hours)
    #[serde(default = "default_max_interval")]
    pub max_interval: u64,
    /// Required branch — repos not on this branch are skipped (default: "main")
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Run `git pull origin <branch>` before updating (default: true)
    #[serde(default = "default_true")]
    pub pull_before_update: bool,
    /// Shell command to run for updating the flake lock (default: "nix flake update")
    #[serde(default = "default_update_command")]
    pub update_command: String,
    /// Timeout in seconds for the update command (default: 600 = 10 minutes)
    #[serde(default = "default_update_timeout")]
    pub update_timeout: u64,
    /// Commit message template — supports $REPO placeholder (default: "chore: update flake.lock")
    #[serde(default = "default_commit_message")]
    pub commit_message: String,
    /// Commit and push after a successful update.
    ///
    /// ── ★ OPT-IN. A WRITE FLAG THAT DEFAULTS ON IS A WRITE NOBODY CHOSE ──
    /// This defaulted to true, so the minimal config anyone would write —
    /// `flake_refresh: {enable: true}` — silently enabled commit+push to
    /// main, through the one refresh path that had no verification gate at
    /// all. Every other write flag in WatchConfig (`auto_certify`,
    /// WatchConfig's own `auto_commit`) is `#[serde(default)]`; this was
    /// the outlier, and the asymmetry is what kept it invisible.
    #[serde(default)]
    pub auto_commit: bool,
    /// Trigger `tend flake-update --changed <repo>` after each committed repo
    #[serde(default)]
    pub auto_propagate: bool,
    /// Only refresh these repos (empty = all repos with flake.nix)
    #[serde(default)]
    pub include: Vec<String>,
    /// Skip these repos (applied after include, on top of workspace exclude)
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Post-hooks to run after each repo refresh (trigger: "after_refresh")
    #[serde(default)]
    pub post_hooks: Vec<PostHook>,
    /// Skip `nix flake update` when local git refs show no inputs are stale (default: true).
    /// Uses zero GitHub API calls — relies on `git fetch` already done by the daemon.
    #[serde(default = "default_true")]
    pub staleness_check: bool,
}

fn default_refresh_interval() -> u64 {
    3600
}

fn default_max_interval() -> u64 {
    86400
}

fn default_branch() -> String {
    "main".to_string()
}

fn default_true() -> bool {
    true
}

fn default_update_command() -> String {
    "nix flake update".to_string()
}

fn default_update_timeout() -> u64 {
    600
}

fn default_commit_message() -> String {
    "chore: update flake.lock".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FlakeInputMode {
    #[default]
    Commits,
    Tags,
}

impl std::fmt::Display for FlakeInputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Commits => f.write_str("commits"),
            Self::Tags => f.write_str("tags"),
        }
    }
}

fn default_flake_input_mode() -> FlakeInputMode {
    FlakeInputMode::Commits
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum CloneMethod {
    #[default]
    Ssh,
    Https,
}

impl std::fmt::Display for CloneMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ssh => f.write_str("ssh"),
            Self::Https => f.write_str("https"),
        }
    }
}

fn default_provider() -> String {
    "github".to_string()
}

fn default_clone_method() -> CloneMethod {
    CloneMethod::Ssh
}

// ── shikumi::TieredConfig — prime directive ────────────────
//
// Every public tend Config struct impls TieredConfig so operators
// can request `tend config-show <tier>` + override via `TEND_TIER`
// env var. bare() = zero-opinion floor (empty strings, 0 numbers,
// false flags, empty collections); prescribed_default() = the
// curated tend defaults that ship (e.g. provider="github",
// clone_method=Ssh, interval=3600s).
//
// Sub-configs that derive Default already produce zero-opinion
// shapes (the curated defaults are applied via serde, not
// Default::default), so bare() delegates to Default for them.

impl shikumi::TieredConfig for Config {
    fn bare() -> Self {
        Self {
            workspaces: Vec::new(),
            host_health: HostHealthConfig::default(),
        }
    }
    fn prescribed_default() -> Self {
        Self {
            workspaces: Vec::new(),
            host_health: HostHealthConfig::default(),
        }
    }
}

impl shikumi::TieredConfig for Workspace {
    fn bare() -> Self {
        Self {
            name: String::new(),
            provider: String::new(),
            base_dir: String::new(),
            clone_method: CloneMethod::default(),
            discover: false,
            org: None,
            exclude: Vec::new(),
            extra_repos: Vec::new(),
            flake_deps: HashMap::new(),
            watch: None,
            ai_tasks: Vec::new(),
            prebuild: None,
        }
    }
    fn prescribed_default() -> Self {
        Self {
            name: String::new(),
            provider: default_provider(),
            base_dir: String::new(),
            clone_method: default_clone_method(),
            discover: false,
            org: None,
            exclude: Vec::new(),
            extra_repos: Vec::new(),
            flake_deps: HashMap::new(),
            watch: None,
            ai_tasks: Vec::new(),
            prebuild: None,
        }
    }
}

impl shikumi::TieredConfig for PrebuildConfig {
    fn bare() -> Self {
        // Floor: no work happens, no attic interaction. Operators
        // who declare a `prebuild:` block opt into the prescribed
        // defaults below for any field they omit.
        Self {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: None,
            attic_server: None,
            attic_url: None,
            attic_token_file: None,
            packages: String::new(),
            systems: Vec::new(),
            repro: String::new(),
            caches: Vec::new(),
        }
    }
    fn prescribed_default() -> Self {
        Self {
            min_interval: default_prebuild_min_interval(),
            max_interval: default_prebuild_max_interval(),
            max_inflight: default_prebuild_max_inflight(),
            attic_cache: None,
            attic_server: None,
            attic_url: None,
            attic_token_file: None,
            packages: default_prebuild_packages(),
            systems: Vec::new(),
            repro: String::new(),
            caches: Vec::new(),
        }
    }
}

impl shikumi::TieredConfig for AiTaskConfig {
    fn bare() -> Self {
        Self {
            name: String::new(),
            schedule: String::new(),
            model: String::new(),
            prompt: String::new(),
            output: None,
            env: HashMap::new(),
            retries: 0,
            timeout_secs: 0,
        }
    }
    fn prescribed_default() -> Self {
        Self {
            name: String::new(),
            schedule: String::new(),
            model: String::new(),
            prompt: String::new(),
            output: None,
            env: HashMap::new(),
            retries: default_retries(),
            timeout_secs: default_timeout(),
        }
    }
}

impl shikumi::TieredConfig for WatchConfig {
    fn bare() -> Self {
        Self {
            enable: false,
            matrix_file: None,
            auto_certify: false,
            auto_commit: false,
            auto_propagate: None,
            post_hooks: Vec::new(),
            file_watches: Vec::new(),
            flake_input_watches: Vec::new(),
            flake_refresh: None,
            nix_audit: None,
            ci_hygiene: None,
            release_swarm: None,
        }
    }
    fn prescribed_default() -> Self {
        // Watch off by default — every watcher is opt-in per workspace.
        Self::bare()
    }
}

impl shikumi::TieredConfig for CiHygieneConfig {
    fn bare() -> Self {
        Self::default()
    }
    fn prescribed_default() -> Self {
        Self {
            enable: false,
            auto_cancel_duplicate_queued: default_true(),
            recent_run_limit: default_recent_run_limit(),
            repo_filter: None,
            stale_queue_alert_minutes: default_stale_queue_minutes(),
        }
    }
}

impl shikumi::TieredConfig for NixAuditConfig {
    fn bare() -> Self {
        Self::default()
    }
    fn prescribed_default() -> Self {
        Self::default()
    }
}

impl shikumi::TieredConfig for FlakeRefreshConfig {
    fn bare() -> Self {
        Self {
            enable: false,
            interval: 0,
            max_interval: 0,
            branch: String::new(),
            pull_before_update: false,
            update_command: String::new(),
            update_timeout: 0,
            commit_message: String::new(),
            auto_commit: false,
            auto_propagate: false,
            include: Vec::new(),
            exclude: Vec::new(),
            post_hooks: Vec::new(),
            staleness_check: false,
        }
    }
    fn prescribed_default() -> Self {
        Self::default()
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config = serde_yaml_ng::from_str(&contents)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }

    /// Discover the default config file path using shikumi.
    ///
    /// Precedence:
    /// 1. `$TEND_CONFIG` environment variable
    /// 2. Standard shikumi paths: `$XDG_CONFIG_HOME/tend/tend.yaml`, `~/.config/tend/tend.yaml`, etc.
    /// 3. Legacy fallback: `~/.config/tend/config.yaml` (backward compat)
    #[must_use]
    pub fn default_path() -> PathBuf {
        use shikumi::{ConfigDiscovery, Format};

        // Try shikumi discovery first (TEND_CONFIG env, then tend/tend.yaml, etc.)
        if let Ok(path) = ConfigDiscovery::new("tend")
            .env_override("TEND_CONFIG")
            .formats(&[Format::Yaml])
            .discover()
        {
            return path;
        }

        // Legacy fallback: tend/config.yaml (pre-shikumi convention)
        Self::legacy_default_path(&crate::xdg::resolver())
    }

    /// The pre-shikumi `<config>/tend/config.yaml` location, resolved through
    /// an explicit okiba so the invariant below is testable without mutating
    /// `std::env` (which races under parallel test execution).
    ///
    /// **Every arm of the old chain could yield a RELATIVE path** — the
    /// masked-branch class of `theory/MASKED-BRANCH.md`. `$XDG_CONFIG_HOME`
    /// was taken verbatim by `PathBuf::from`, so `XDG_CONFIG_HOME=""`
    /// resolved to a bare `tend/config.yaml` and `XDG_CONFIG_HOME=rel` to
    /// `rel/tend/config.yaml`; the home arm's own
    /// `unwrap_or_else(|| PathBuf::from("."))` produced
    /// `./.config/tend/config.yaml` for the same reason. That is not cosmetic
    /// here: `tend init` calls `create_dir_all(path.parent())` and writes, so
    /// a relative resolution scatters a config into whatever directory the
    /// operator happened to be standing in — a different one each time.
    ///
    /// okiba resolves this tier to the identical place for every valid
    /// configuration (`$XDG_CONFIG_HOME/tend` or `~/.config/tend`) and
    /// ignores a relative override instead of joining it.
    fn legacy_default_path(x: &okiba::Okiba) -> PathBuf {
        x.try_path(okiba::Tier::Config, "config.yaml")
            // No absolute `$XDG_CONFIG_HOME` and no usable `$HOME` at all:
            // fall back to the spec's SYSTEM-wide config location, which is
            // at least absolute and stable. A `tend init` there fails loudly
            // on permissions rather than silently seeding a per-cwd config.
            .unwrap_or_else(|_| PathBuf::from("/etc/xdg/tend/config.yaml"))
    }

    /// Generate a starter config file.
    ///
    /// # Errors
    ///
    /// Returns an error if YAML serialization fails (should not happen with
    /// the well-known starter config, but callers get a typed error instead
    /// of a panic).
    pub fn generate_starter() -> Result<String> {
        let config = Config {
            host_health: HostHealthConfig::default(),
            workspaces: vec![Workspace {
                name: "my-org".to_string(),
                provider: "github".to_string(),
                base_dir: "~/code/github/my-org".to_string(),
                clone_method: CloneMethod::Ssh,
                discover: true,
                org: Some("my-org".to_string()),
                exclude: vec![".github".to_string()],
                extra_repos: vec![],
                flake_deps: HashMap::new(),
                watch: None,
                ai_tasks: vec![],
                prebuild: None,
            }],
        };
        serde_yaml_ng::to_string(&config).context("serializing starter config")
    }
}

impl Workspace {
    /// Create a minimal workspace for testing.
    #[cfg(test)]
    pub(crate) fn test_default(name: &str) -> Self {
        Self {
            name: name.to_string(),
            provider: "github".to_string(),
            base_dir: "/tmp".to_string(),
            clone_method: CloneMethod::Ssh,
            discover: false,
            org: None,
            exclude: vec![],
            extra_repos: vec![],
            flake_deps: HashMap::new(),
            watch: None,
            ai_tasks: vec![],
            prebuild: None,
        }
    }

    /// Resolve base_dir with shell expansion (~ → home dir)
    pub fn resolved_base_dir(&self) -> Result<PathBuf> {
        let expanded = shellexpand::tilde(&self.base_dir);
        Ok(PathBuf::from(expanded.as_ref()))
    }

    /// Build the clone URL for a repo name
    #[must_use]
    pub fn clone_url(&self, repo_name: &str) -> String {
        let org = self.org.as_deref().unwrap_or(&self.name);
        match self.clone_method {
            CloneMethod::Ssh => format!("git@github.com:{org}/{repo_name}.git"),
            CloneMethod::Https => format!("https://github.com/{org}/{repo_name}.git"),
        }
    }
}

/// Convenience wrapper — calls `Config::generate_starter()`.
pub(crate) fn generate_starter_config() -> Result<String> {
    Config::generate_starter()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an okiba over an explicit environment — no `std::env` mutation,
    /// so this runs safely alongside every other test.
    fn okiba_with(xdg_config_home: Option<&str>, home: Option<&str>) -> okiba::Okiba {
        let x = xdg_config_home.map(str::to_string);
        let h = home.map(str::to_string);
        okiba::Okiba::from_env("tend", move |k| match k {
            "XDG_CONFIG_HOME" => x.clone(),
            "HOME" => h.clone(),
            _ => None,
        })
    }

    /// ── ★ THE MASKED BRANCH (theory/MASKED-BRANCH.md) ───────────────────
    /// The `$XDG_CONFIG_HOME` arm was taken verbatim while the `$HOME` arm
    /// went through `dirs::home_dir()`, so *unsetting* the variable behaved
    /// correctly and *emptying* it did not — and unsetting is what anyone
    /// testing this would do. `tend init` writes to this path, so a relative
    /// result seeds a config into the current working directory.
    ///
    /// Red run: restore `std::env::var("XDG_CONFIG_HOME").map(PathBuf::from)`
    /// and the empty/relative rows below yield `tend/config.yaml` and
    /// `rel/x/tend/config.yaml`.
    #[test]
    fn no_environment_yields_a_relative_config_path() {
        for bogus in ["", "rel/x", "./x", "..", "x"] {
            let p = Config::legacy_default_path(&okiba_with(Some(bogus), Some("/home/u")));
            assert!(
                p.is_absolute(),
                "XDG_CONFIG_HOME={bogus:?} resolved to the relative {p:?}"
            );
            assert_eq!(
                p,
                PathBuf::from("/home/u/.config/tend/config.yaml"),
                "a rejected override must fall through to $HOME, not be joined"
            );
        }

        // The home arm's own `unwrap_or_else(|| PathBuf::from("."))` was the
        // second unguarded arm: no usable $HOME used to give
        // `./.config/tend/config.yaml`.
        for home in [None, Some(""), Some("rel/home")] {
            let p = Config::legacy_default_path(&okiba_with(None, home));
            assert!(
                p.is_absolute(),
                "HOME={home:?} resolved to the relative {p:?}"
            );
        }
    }

    /// The other half of the rule: a *valid* configuration must land exactly
    /// where it landed before, or the fix has quietly orphaned every existing
    /// config file.
    #[test]
    fn a_valid_configuration_resolves_where_it_always_did() {
        assert_eq!(
            Config::legacy_default_path(&okiba_with(Some("/x"), Some("/home/u"))),
            PathBuf::from("/x/tend/config.yaml"),
        );
        assert_eq!(
            Config::legacy_default_path(&okiba_with(None, Some("/home/u"))),
            PathBuf::from("/home/u/.config/tend/config.yaml"),
        );
    }

    #[test]
    fn test_config_load_valid_yaml() {
        let dir = std::env::temp_dir().join("tend-config-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-config.yaml");
        let yaml = r#"
workspaces:
  - name: test-ws
    base_dir: /tmp/repos
    discover: false
"#;
        std::fs::write(&path, yaml).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.workspaces[0].name, "test-ws");
        assert_eq!(cfg.workspaces[0].base_dir, "/tmp/repos");
        assert!(!cfg.workspaces[0].discover);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_config_load_nonexistent_file() {
        let result = Config::load(std::path::Path::new("/nonexistent/path/config.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_config_load_invalid_yaml() {
        let dir = std::env::temp_dir().join("tend-config-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad-config.yaml");
        std::fs::write(&path, "{{{{ not valid yaml").unwrap();
        let result = Config::load(&path);
        assert!(result.is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_serde_defaults_provider() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.workspaces[0].provider, "github");
    }

    #[test]
    fn test_serde_defaults_clone_method() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.workspaces[0].clone_method, CloneMethod::Ssh);
    }

    #[test]
    fn test_serde_explicit_clone_method_https() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    clone_method: https
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.workspaces[0].clone_method, CloneMethod::Https);
    }

    #[test]
    fn test_serde_defaults_discover_false() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!cfg.workspaces[0].discover);
    }

    #[test]
    fn test_serde_defaults_empty_collections() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &cfg.workspaces[0];
        assert!(ws.exclude.is_empty());
        assert!(ws.extra_repos.is_empty());
        assert!(ws.flake_deps.is_empty());
        assert!(ws.watch.is_none());
        assert!(ws.org.is_none());
    }

    #[test]
    fn test_clone_url_ssh() {
        let mut ws = Workspace::test_default("my-org");
        ws.org = Some("acme".to_string());
        assert_eq!(ws.clone_url("my-repo"), "git@github.com:acme/my-repo.git");
    }

    #[test]
    fn test_clone_url_https() {
        let mut ws = Workspace::test_default("my-org");
        ws.clone_method = CloneMethod::Https;
        ws.org = Some("acme".to_string());
        assert_eq!(
            ws.clone_url("my-repo"),
            "https://github.com/acme/my-repo.git"
        );
    }

    #[test]
    fn test_clone_url_falls_back_to_name_when_org_is_none() {
        let ws = Workspace::test_default("fallback-org");
        assert_eq!(ws.clone_url("repo"), "git@github.com:fallback-org/repo.git");
    }

    #[test]
    fn test_resolved_base_dir_tilde_expansion() {
        let mut ws = Workspace::test_default("test");
        ws.base_dir = "~/repos".to_string();
        let resolved = ws.resolved_base_dir().unwrap();
        assert!(!resolved.to_string_lossy().contains('~'));
        assert!(resolved.to_string_lossy().ends_with("/repos"));
    }

    #[test]
    fn test_resolved_base_dir_absolute_path_unchanged() {
        let mut ws = Workspace::test_default("test");
        ws.base_dir = "/absolute/path".to_string();
        let resolved = ws.resolved_base_dir().unwrap();
        assert_eq!(resolved, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_generate_starter_config_is_valid_yaml() {
        let yaml = generate_starter_config().unwrap();
        let parsed: Config = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.workspaces[0].name, "my-org");
        assert!(parsed.workspaces[0].discover);
        assert_eq!(parsed.workspaces[0].exclude, vec![".github".to_string()]);
    }

    #[test]
    fn test_watch_config_defaults() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    watch:
      enable: true
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let watch = cfg.workspaces[0].watch.as_ref().unwrap();
        assert!(watch.enable);
        assert!(!watch.auto_certify);
        assert!(!watch.auto_commit);
        assert!(watch.auto_propagate.is_none());
        assert!(watch.matrix_file.is_none());
        assert!(watch.post_hooks.is_empty());
        assert!(watch.file_watches.is_empty());
        assert!(watch.flake_input_watches.is_empty());
    }

    #[test]
    fn test_flake_input_mode_default_is_commits() {
        assert_eq!(default_flake_input_mode(), FlakeInputMode::Commits);
    }

    #[test]
    fn test_flake_input_mode_serde_roundtrip() {
        let yaml = "commits";
        let mode: FlakeInputMode = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(mode, FlakeInputMode::Commits);

        let yaml = "tags";
        let mode: FlakeInputMode = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(mode, FlakeInputMode::Tags);
    }

    #[test]
    fn test_clone_method_default_is_ssh() {
        assert_eq!(CloneMethod::default(), CloneMethod::Ssh);
    }

    #[test]
    fn test_flake_input_mode_default_is_commits_via_default() {
        assert_eq!(FlakeInputMode::default(), FlakeInputMode::Commits);
    }

    #[test]
    fn test_clone_method_display() {
        assert_eq!(CloneMethod::Ssh.to_string(), "ssh");
        assert_eq!(CloneMethod::Https.to_string(), "https");
    }

    #[test]
    fn test_flake_input_mode_display() {
        assert_eq!(FlakeInputMode::Commits.to_string(), "commits");
        assert_eq!(FlakeInputMode::Tags.to_string(), "tags");
    }

    #[test]
    fn test_clone_method_serde_roundtrip() {
        let yaml = "ssh";
        let method: CloneMethod = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(method, CloneMethod::Ssh);

        let yaml = "https";
        let method: CloneMethod = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(method, CloneMethod::Https);
    }

    #[test]
    fn test_nix_audit_config_defaults() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    watch:
      enable: true
      nix_audit:
        enable: true
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nix_audit = cfg.workspaces[0]
            .watch
            .as_ref()
            .unwrap()
            .nix_audit
            .as_ref()
            .unwrap();
        assert!(nix_audit.enable);
        assert!(!nix_audit.auto_fix);
        assert!(!nix_audit.auto_propagate);
        assert!(nix_audit.db_path.is_none());
        assert!(nix_audit.post_hooks.is_empty());
    }

    #[test]
    fn test_flake_refresh_config_default_impl_matches_serde() {
        let fr = FlakeRefreshConfig::default();
        assert!(!fr.enable);
        assert_eq!(fr.interval, 3600);
        assert_eq!(fr.max_interval, 86400);
        assert_eq!(fr.branch, "main");
        assert!(fr.pull_before_update);
        assert_eq!(fr.update_command, "nix flake update");
        assert_eq!(fr.update_timeout, 600);
        assert_eq!(fr.commit_message, "chore: update flake.lock");
        // Deliberately flipped 2026-08-03: `auto_commit` defaulting true
        // meant `flake_refresh: {enable: true}` silently enabled
        // commit+push to main through the one refresh path with no
        // verification gate. This assertion previously pinned that.
        assert!(!fr.auto_commit, "a write flag must be opt-in");
        assert!(!fr.auto_propagate);
        assert!(fr.staleness_check);
    }

    #[test]
    fn test_nix_audit_config_default_impl() {
        let cfg = NixAuditConfig::default();
        assert!(!cfg.enable);
        assert!(!cfg.auto_fix);
        assert!(!cfg.auto_propagate);
        assert!(cfg.db_path.is_none());
        assert!(cfg.post_hooks.is_empty());
    }

    #[test]
    fn test_flake_refresh_config_defaults() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    watch:
      enable: true
      flake_refresh:
        enable: true
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let fr = cfg.workspaces[0]
            .watch
            .as_ref()
            .unwrap()
            .flake_refresh
            .as_ref()
            .unwrap();
        assert!(fr.enable);
        assert_eq!(fr.interval, 3600);
        assert_eq!(fr.max_interval, 86400);
        assert_eq!(fr.branch, "main");
        assert!(fr.pull_before_update);
        assert_eq!(fr.update_command, "nix flake update");
        assert_eq!(fr.update_timeout, 600);
        assert_eq!(fr.commit_message, "chore: update flake.lock");
        // Flipped with the serde default and the Default impl — all three
        // pinned the same unsafe default, and fixing one at a time is how
        // two of them survived the first pass.
        assert!(!fr.auto_commit, "a write flag must be opt-in");
        assert!(!fr.auto_propagate);
        assert!(fr.include.is_empty());
        assert!(fr.exclude.is_empty());
        assert!(fr.post_hooks.is_empty());
        assert!(fr.staleness_check);
    }

    #[test]
    fn test_config_full_watch_config_with_all_fields() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    clone_method: https
    discover: true
    org: my-org
    exclude:
      - .github
    extra_repos:
      - special-repo
    watch:
      enable: true
      matrix_file: ~/matrix.toml
      auto_certify: true
      auto_commit: true
      auto_propagate: my-nix-repo
      post_hooks:
        - trigger: after_certify
          command: echo
          args: ["done"]
          continue_on_error: true
      file_watches:
        - name: openapi
          org: myorg
          repo: myrepo
          path: spec.yaml
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &cfg.workspaces[0];
        let watch = ws.watch.as_ref().unwrap();
        assert!(watch.auto_certify);
        assert!(watch.auto_commit);
        assert_eq!(watch.auto_propagate.as_deref(), Some("my-nix-repo"));
        assert_eq!(watch.post_hooks.len(), 1);
        assert_eq!(watch.post_hooks[0].trigger, "after_certify");
        assert!(watch.post_hooks[0].continue_on_error);
        assert_eq!(watch.file_watches.len(), 1);
        assert_eq!(watch.file_watches[0].name, "openapi");
    }

    #[test]
    fn test_config_multiple_workspaces() {
        let yaml = r#"
workspaces:
  - name: ws-a
    base_dir: /tmp/a
  - name: ws-b
    base_dir: /tmp/b
    clone_method: https
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        assert_eq!(cfg.workspaces[0].name, "ws-a");
        assert_eq!(cfg.workspaces[1].name, "ws-b");
        assert_eq!(cfg.workspaces[1].clone_method, CloneMethod::Https);
    }

    #[test]
    fn test_workspace_flake_deps_parsing() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    flake_deps:
      repo-a:
        - lib-x
        - lib-y
      repo-b:
        - repo-a
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &cfg.workspaces[0];
        assert_eq!(ws.flake_deps.len(), 2);
        assert_eq!(ws.flake_deps["repo-a"], vec!["lib-x", "lib-y"]);
        assert_eq!(ws.flake_deps["repo-b"], vec!["repo-a"]);
    }

    #[test]
    fn test_workspace_extra_repos_and_exclude() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    exclude:
      - .github
      - old-repo
    extra_repos:
      - special-repo
      - another-special
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let ws = &cfg.workspaces[0];
        assert_eq!(ws.exclude, vec![".github", "old-repo"]);
        assert_eq!(ws.extra_repos, vec!["special-repo", "another-special"]);
    }

    #[test]
    fn test_post_hook_all_fields() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    watch:
      enable: true
      post_hooks:
        - trigger: after_all
          command: ./my-script.sh
          args: ["$REPO", "$VERSION", "--flag"]
          working_dir: ~/scripts
          continue_on_error: false
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let hook = &cfg.workspaces[0].watch.as_ref().unwrap().post_hooks[0];
        assert_eq!(hook.trigger, "after_all");
        assert_eq!(hook.command, "./my-script.sh");
        assert_eq!(hook.args, vec!["$REPO", "$VERSION", "--flag"]);
        assert_eq!(hook.working_dir.as_deref(), Some("~/scripts"));
        assert!(!hook.continue_on_error);
    }

    #[test]
    fn test_file_watch_all_fields() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    watch:
      enable: true
      file_watches:
        - name: my-spec
          org: myorg
          repo: myrepo
          path: api/openapi.yaml
          download_to: ~/specs
          post_hooks:
            - trigger: on_change
              command: regenerate
              args: ["$CURRENT_FILE"]
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let fw = &cfg.workspaces[0].watch.as_ref().unwrap().file_watches[0];
        assert_eq!(fw.name, "my-spec");
        assert_eq!(fw.org, "myorg");
        assert_eq!(fw.repo, "myrepo");
        assert_eq!(fw.path, "api/openapi.yaml");
        assert_eq!(fw.download_to.as_deref(), Some("~/specs"));
        assert_eq!(fw.post_hooks.len(), 1);
        assert_eq!(fw.post_hooks[0].trigger, "on_change");
    }

    #[test]
    fn test_flake_input_watch_all_fields() {
        let yaml = r#"
workspaces:
  - name: test
    base_dir: /tmp
    watch:
      enable: true
      flake_input_watches:
        - name: claude
          repo: my-repo
          input: claude-code
          upstream: pleme-io/claude-code
          mode: tags
          auto_update: true
          auto_commit: true
          auto_propagate: nix-repo
"#;
        let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let fiw = &cfg.workspaces[0]
            .watch
            .as_ref()
            .unwrap()
            .flake_input_watches[0];
        assert_eq!(fiw.name, "claude");
        assert_eq!(fiw.repo, "my-repo");
        assert_eq!(fiw.input, "claude-code");
        assert_eq!(fiw.upstream.as_deref(), Some("pleme-io/claude-code"));
        assert_eq!(fiw.mode, FlakeInputMode::Tags);
        assert!(fiw.auto_update);
        assert!(fiw.auto_commit);
        assert_eq!(fiw.auto_propagate.as_deref(), Some("nix-repo"));
    }

    #[test]
    fn test_config_load_returns_error_context_on_invalid_yaml() {
        let dir = std::env::temp_dir().join("tend-config-test-ctx");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad-ctx.yaml");
        std::fs::write(&path, "{{{{ not valid yaml").unwrap();
        let result = Config::load(&path);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("parsing"),
            "error should mention parsing context: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tiered_tests {
    use super::*;
    use shikumi::{ConfigTier, TieredConfig};

    #[test]
    fn bare_is_zero_opinion() {
        let b = <Config as TieredConfig>::bare();
        assert!(b.workspaces.is_empty());

        let bw = <Workspace as TieredConfig>::bare();
        assert_eq!(bw.name, "");
        assert_eq!(bw.provider, "");
        assert_eq!(bw.base_dir, "");
        assert!(!bw.discover);
        assert!(bw.watch.is_none());
        assert!(bw.exclude.is_empty());
        assert!(bw.flake_deps.is_empty());

        let ba = <AiTaskConfig as TieredConfig>::bare();
        assert_eq!(ba.name, "");
        assert_eq!(ba.retries, 0);
        assert_eq!(ba.timeout_secs, 0);

        let bwatch = <WatchConfig as TieredConfig>::bare();
        assert!(!bwatch.enable);
        assert!(bwatch.flake_refresh.is_none());
        assert!(bwatch.ci_hygiene.is_none());

        let bch = <CiHygieneConfig as TieredConfig>::bare();
        assert!(!bch.enable);
        assert!(!bch.auto_cancel_duplicate_queued);
        assert_eq!(bch.recent_run_limit, 0);
        assert_eq!(bch.stale_queue_alert_minutes, 0);

        let bna = <NixAuditConfig as TieredConfig>::bare();
        assert!(!bna.enable);
        assert!(!bna.auto_fix);

        let bfr = <FlakeRefreshConfig as TieredConfig>::bare();
        assert!(!bfr.enable);
        assert_eq!(bfr.interval, 0);
        assert_eq!(bfr.branch, "");
        assert!(!bfr.pull_before_update);
        assert_eq!(bfr.update_command, "");
        assert!(!bfr.staleness_check);
    }

    #[test]
    fn prescribed_matches_default() {
        // Workspace prescribed_default applies the serde-default-fn values.
        let pw = <Workspace as TieredConfig>::prescribed_default();
        assert_eq!(pw.provider, default_provider());
        assert_eq!(pw.clone_method, default_clone_method());

        // AiTaskConfig prescribed_default carries default_retries/default_timeout.
        let pa = <AiTaskConfig as TieredConfig>::prescribed_default();
        assert_eq!(pa.retries, default_retries());
        assert_eq!(pa.timeout_secs, default_timeout());

        // CiHygieneConfig prescribed_default carries the curated auto-cancel + limits.
        let pch = <CiHygieneConfig as TieredConfig>::prescribed_default();
        assert!(pch.auto_cancel_duplicate_queued);
        assert_eq!(pch.recent_run_limit, default_recent_run_limit());
        assert_eq!(pch.stale_queue_alert_minutes, default_stale_queue_minutes());

        // FlakeRefreshConfig prescribed_default = the existing Default::default().
        let pfr = <FlakeRefreshConfig as TieredConfig>::prescribed_default();
        let dfr = FlakeRefreshConfig::default();
        assert_eq!(pfr.interval, dfr.interval);
        assert_eq!(pfr.branch, dfr.branch);
        assert_eq!(pfr.update_command, dfr.update_command);
        assert_eq!(pfr.staleness_check, dfr.staleness_check);
    }

    #[test]
    fn diff_bare_vs_default_is_non_empty() {
        // Workspace has serde-default-fn-driven differences.
        let b = <Workspace as TieredConfig>::bare();
        let d = <Workspace as TieredConfig>::prescribed_default();
        let diff = d.diff_against(&b);
        assert!(
            !diff.is_empty_diff(),
            "Workspace bare and prescribed_default must differ"
        );

        // AiTaskConfig retries/timeout differ.
        let ab = <AiTaskConfig as TieredConfig>::bare();
        let ad = <AiTaskConfig as TieredConfig>::prescribed_default();
        assert!(
            !ad.diff_against(&ab).is_empty_diff(),
            "AiTaskConfig bare and prescribed_default must differ"
        );

        // CiHygieneConfig has 3 curated defaults.
        let cb = <CiHygieneConfig as TieredConfig>::bare();
        let cd = <CiHygieneConfig as TieredConfig>::prescribed_default();
        assert!(
            !cd.diff_against(&cb).is_empty_diff(),
            "CiHygieneConfig bare and prescribed_default must differ"
        );

        // FlakeRefreshConfig has multiple curated defaults.
        let fb = <FlakeRefreshConfig as TieredConfig>::bare();
        let fd = <FlakeRefreshConfig as TieredConfig>::prescribed_default();
        assert!(
            !fd.diff_against(&fb).is_empty_diff(),
            "FlakeRefreshConfig bare and prescribed_default must differ"
        );
    }

    #[test]
    fn resolve_tier_dispatches() {
        assert_eq!(
            <Workspace as TieredConfig>::resolve_tier(ConfigTier::Bare).provider,
            ""
        );
        assert_eq!(
            <Workspace as TieredConfig>::resolve_tier(ConfigTier::Default).provider,
            "github"
        );
        assert_eq!(
            <AiTaskConfig as TieredConfig>::resolve_tier(ConfigTier::Bare).retries,
            0
        );
        assert_eq!(
            <AiTaskConfig as TieredConfig>::resolve_tier(ConfigTier::Default).retries,
            3
        );
        assert_eq!(
            <CiHygieneConfig as TieredConfig>::resolve_tier(ConfigTier::Default).recent_run_limit,
            50
        );
        assert_eq!(
            <FlakeRefreshConfig as TieredConfig>::resolve_tier(ConfigTier::Default).interval,
            3600
        );
        assert!(<Config as TieredConfig>::resolve_tier(ConfigTier::Bare)
            .workspaces
            .is_empty());
    }
}
