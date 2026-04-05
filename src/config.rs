use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub workspaces: Vec<Workspace>,
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
}

/// Configuration for nix-audit integration in the tend daemon.
///
/// When enabled, the daemon runs `nix-audit check --all` after the watch cycle,
/// optionally auto-fixes violations and propagates fixes across the flake graph.
/// Results are tracked in a convergence database for trend analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Commit and push after a successful update (default: true)
    #[serde(default = "default_true")]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FlakeInputMode {
    Commits,
    Tags,
}

fn default_flake_input_mode() -> FlakeInputMode {
    FlakeInputMode::Commits
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CloneMethod {
    Ssh,
    Https,
}

fn default_provider() -> String {
    "github".to_string()
}

fn default_clone_method() -> CloneMethod {
    CloneMethod::Ssh
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            serde_yaml_ng::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }

    /// Discover the default config file path using shikumi.
    ///
    /// Precedence:
    /// 1. `$TEND_CONFIG` environment variable
    /// 2. Standard shikumi paths: `$XDG_CONFIG_HOME/tend/tend.yaml`, `~/.config/tend/tend.yaml`, etc.
    /// 3. Legacy fallback: `~/.config/tend/config.yaml` (backward compat)
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
        let config_dir = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            });
        config_dir.join("tend").join("config.yaml")
    }
}

impl Workspace {
    /// Resolve base_dir with shell expansion (~ → home dir)
    pub fn resolved_base_dir(&self) -> Result<PathBuf> {
        let expanded = shellexpand::tilde(&self.base_dir);
        Ok(PathBuf::from(expanded.as_ref()))
    }

    /// Build the clone URL for a repo name
    pub fn clone_url(&self, repo_name: &str) -> String {
        let org = self.org.as_deref().unwrap_or(&self.name);
        match self.clone_method {
            CloneMethod::Ssh => format!("git@github.com:{org}/{repo_name}.git"),
            CloneMethod::Https => format!("https://github.com/{org}/{repo_name}.git"),
        }
    }
}

/// Generate a starter config file
pub fn generate_starter_config() -> String {
    let config = Config {
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
        }],
    };
    serde_yaml_ng::to_string(&config).unwrap()
}
