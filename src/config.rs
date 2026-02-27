use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
            serde_yaml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }

    pub fn default_path() -> PathBuf {
        // Use XDG_CONFIG_HOME or ~/.config (not macOS ~/Library/Application Support)
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
        }],
    };
    serde_yaml::to_string(&config).unwrap()
}
