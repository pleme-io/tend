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

/// Generate a starter config file.
pub(crate) fn generate_starter_config() -> Result<String> {
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
    serde_yaml_ng::to_string(&config).context("serializing starter config")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let ws = Workspace {
            name: "my-org".to_string(),
            provider: "github".to_string(),
            base_dir: "/tmp".to_string(),
            clone_method: CloneMethod::Ssh,
            discover: false,
            org: Some("acme".to_string()),
            exclude: vec![],
            extra_repos: vec![],
            flake_deps: HashMap::new(),
            watch: None,
        };
        assert_eq!(ws.clone_url("my-repo"), "git@github.com:acme/my-repo.git");
    }

    #[test]
    fn test_clone_url_https() {
        let ws = Workspace {
            name: "my-org".to_string(),
            provider: "github".to_string(),
            base_dir: "/tmp".to_string(),
            clone_method: CloneMethod::Https,
            discover: false,
            org: Some("acme".to_string()),
            exclude: vec![],
            extra_repos: vec![],
            flake_deps: HashMap::new(),
            watch: None,
        };
        assert_eq!(
            ws.clone_url("my-repo"),
            "https://github.com/acme/my-repo.git"
        );
    }

    #[test]
    fn test_clone_url_falls_back_to_name_when_org_is_none() {
        let ws = Workspace {
            name: "fallback-org".to_string(),
            provider: "github".to_string(),
            base_dir: "/tmp".to_string(),
            clone_method: CloneMethod::Ssh,
            discover: false,
            org: None,
            exclude: vec![],
            extra_repos: vec![],
            flake_deps: HashMap::new(),
            watch: None,
        };
        assert_eq!(
            ws.clone_url("repo"),
            "git@github.com:fallback-org/repo.git"
        );
    }

    #[test]
    fn test_resolved_base_dir_tilde_expansion() {
        let ws = Workspace {
            name: "test".to_string(),
            provider: "github".to_string(),
            base_dir: "~/repos".to_string(),
            clone_method: CloneMethod::Ssh,
            discover: false,
            org: None,
            exclude: vec![],
            extra_repos: vec![],
            flake_deps: HashMap::new(),
            watch: None,
        };
        let resolved = ws.resolved_base_dir().unwrap();
        assert!(!resolved.to_string_lossy().contains('~'));
        assert!(resolved.to_string_lossy().ends_with("/repos"));
    }

    #[test]
    fn test_resolved_base_dir_absolute_path_unchanged() {
        let ws = Workspace {
            name: "test".to_string(),
            provider: "github".to_string(),
            base_dir: "/absolute/path".to_string(),
            clone_method: CloneMethod::Ssh,
            discover: false,
            org: None,
            exclude: vec![],
            extra_repos: vec![],
            flake_deps: HashMap::new(),
            watch: None,
        };
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
        let nix_audit = cfg.workspaces[0].watch.as_ref().unwrap().nix_audit.as_ref().unwrap();
        assert!(nix_audit.enable);
        assert!(!nix_audit.auto_fix);
        assert!(!nix_audit.auto_propagate);
        assert!(nix_audit.db_path.is_none());
        assert!(nix_audit.post_hooks.is_empty());
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
        let fr = cfg.workspaces[0].watch.as_ref().unwrap().flake_refresh.as_ref().unwrap();
        assert!(fr.enable);
        assert_eq!(fr.interval, 3600);
        assert_eq!(fr.max_interval, 86400);
        assert_eq!(fr.branch, "main");
        assert!(fr.pull_before_update);
        assert_eq!(fr.update_command, "nix flake update");
        assert_eq!(fr.update_timeout, 600);
        assert_eq!(fr.commit_message, "chore: update flake.lock");
        assert!(fr.auto_commit);
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
}
