//! Release-swarm executor — the org-level fan-out of the canonical
//! `rust-tool-public-release` pattern across every enabled repo.
//!
//! **Deny-by-default, two-level opt-in.** Nothing is applied unless
//! BOTH the org-level and per-repo `enable` flags are explicitly
//! `true`. Mirrors the typed policy in
//! `arch-synthesizer/src/rust_tool_release/swarm.rs`.
//!
//! This module is the EXECUTOR side: reads the config, queries
//! GitHub, renders release.yml per eligible repo (via
//! arch-synthesizer, invoked as a library dep or shelled-out
//! binary), and opens PRs. Pure-policy logic is the `plan()` fn;
//! the real apply loop threads through a trait-backed API for
//! testability (same pattern as `ci_trim`).
//!
//! **Config shape mirrors `arch-synthesizer::rust_tool_release::swarm`.**
//! Kept in sync by convention + a shared YAML serialization contract
//! — both sides round-trip through identical field names + defaults.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Per-org swarm policy. Deny by default at both levels.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OrgReleaseSwarmConfig {
    /// GitHub org/owner (e.g. "pleme-io", "drzzln").
    #[serde(default)]
    pub org: String,
    /// Org-level enable. DENY by default.
    #[serde(default)]
    pub enable: bool,
    /// Target matrix label (passed through to arch-synthesizer):
    /// "three_target" (default) | "four_target_including_intel" | "custom".
    /// String-shaped here so tend stays loosely coupled to
    /// arch-synthesizer's enum representation.
    #[serde(default = "default_target_matrix")]
    pub default_target_matrix: String,
    /// Artefact retention in days.
    #[serde(default = "default_retention")]
    pub default_retention_days: u32,
    /// Per-repo configuration. Repos not listed OR listed with
    /// `enable: false` are both treated as disabled.
    #[serde(default)]
    pub repos: BTreeMap<String, RepoReleaseConfig>,
}

fn default_target_matrix() -> String {
    "three_target".to_string()
}

fn default_retention() -> u32 {
    90
}

/// Per-repo overrides. Every override optional.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RepoReleaseConfig {
    /// Per-repo enable. DENY by default.
    #[serde(default)]
    pub enable: bool,
    /// Binary name (falls back to repo name).
    #[serde(default)]
    pub binary_name: Option<String>,
    /// Cargo features enabled on every target.
    #[serde(default)]
    pub features: Vec<String>,
    /// Override target matrix for this repo.
    #[serde(default)]
    pub target_matrix: Option<String>,
    /// Override retention for this repo.
    #[serde(default)]
    pub retention_days: Option<u32>,
}

impl OrgReleaseSwarmConfig {
    /// Substring patterns that are STRUCTURALLY forbidden from the
    /// public-release swarm. Mirror of
    /// `arch_synthesizer::rust_tool_release::swarm::OrgReleaseSwarmConfig::FORBIDDEN_PATTERNS`
    /// — kept in lockstep by convention (cargo test asserts the
    /// contents). Any repo name containing any of these is ineligible
    /// regardless of config; the two-flag opt-in CANNOT override.
    pub const FORBIDDEN_PATTERNS: &'static [&'static str] = &["akeyless"];

    pub fn enabled(org: impl Into<String>) -> Self {
        Self { org: org.into(), enable: true, ..Self::default() }
    }

    #[must_use]
    pub fn with_enabled_repo(
        mut self,
        repo: impl Into<String>,
        cfg: RepoReleaseConfig,
    ) -> Self {
        let mut c = cfg;
        c.enable = true;
        self.repos.insert(repo.into(), c);
        self
    }

    pub fn is_eligible(&self, repo: &str) -> bool {
        for pat in Self::FORBIDDEN_PATTERNS {
            if repo.contains(pat) {
                return false;
            }
        }
        if !self.enable {
            return false;
        }
        self.repos.get(repo).map(|r| r.enable).unwrap_or(false)
    }

    /// Deterministic sorted list of eligible repos. Goes through
    /// `is_eligible()` so `FORBIDDEN_PATTERNS` + both-flags gates
    /// apply uniformly.
    pub fn eligible_repos(&self) -> Vec<String> {
        self.repos
            .keys()
            .filter(|n| self.is_eligible(n))
            .cloned()
            .collect()
    }

    /// Does the repo name match `FORBIDDEN_PATTERNS`? Structural deny
    /// — independent of the opt-in flags. Use to distinguish
    /// "user chose not to ship" from "structurally forbidden"
    /// (misconfiguration).
    pub fn is_forbidden(&self, repo: &str) -> bool {
        Self::FORBIDDEN_PATTERNS.iter().any(|p| repo.contains(p))
    }

    pub fn plan(&self) -> SwarmPlan {
        let eligible = self.eligible_repos();
        let declared_but_forbidden: Vec<String> = self
            .repos
            .keys()
            .filter(|n| self.is_forbidden(n))
            .cloned()
            .collect();
        let declared_but_disabled: Vec<String> = self
            .repos
            .iter()
            .filter(|(n, r)| !r.enable && !self.is_forbidden(n))
            .map(|(n, _)| n.clone())
            .collect();
        SwarmPlan {
            org: self.org.clone(),
            org_enabled: self.enable,
            eligible_count: eligible.len(),
            eligible_repos: eligible,
            declared_but_disabled,
            declared_but_forbidden,
        }
    }
}

/// Dry-run snapshot suitable for stdout display + audit-log emission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmPlan {
    pub org: String,
    pub org_enabled: bool,
    pub eligible_count: usize,
    pub eligible_repos: Vec<String>,
    pub declared_but_disabled: Vec<String>,
    /// Repos listed in config whose names match `FORBIDDEN_PATTERNS`.
    /// These are MISCONFIGURATIONS — surface loudly.
    #[serde(default)]
    pub declared_but_forbidden: Vec<String>,
}

/// GitHub API surface the executor needs. Separate trait (not merged
/// into `GitHubClient`) so mock tests don't drag the broader tend
/// GitHub surface.
#[async_trait::async_trait]
pub trait ReleaseSwarmApi: Send + Sync {
    /// Is there an existing `.github/workflows/release.yml` in this repo?
    /// Returns None if the file doesn't exist (404); returns Some(sha)
    /// to allow PUT-with-if-match for safe updates.
    async fn get_workflow_file_sha(
        &self,
        org: &str,
        repo: &str,
        path: &str,
    ) -> Result<Option<String>>;

    /// Open a PR that writes `path` on a new branch `branch` with the
    /// given `content`. Returns the PR number.
    async fn open_workflow_pr(
        &self,
        org: &str,
        repo: &str,
        branch: &str,
        path: &str,
        content: &str,
        commit_message: &str,
        pr_title: &str,
        pr_body: &str,
    ) -> Result<u64>;
}

/// Per-repo apply outcome — the unit of audit-log emission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoApplyReport {
    pub org: String,
    pub repo: String,
    pub outcome: ApplyOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyOutcome {
    /// Dry-run only; no PR opened. Content that would be written
    /// included for operator visibility.
    DryRun { rendered_bytes: usize },
    /// PR opened with the workflow file.
    PrOpened { pr_number: u64 },
    /// Workflow file already present + matches the rendered content;
    /// nothing to do.
    AlreadyInSync,
    /// Repo skipped because it wasn't eligible (debug aid — in
    /// practice the caller filters before this fires).
    IneligibleSkipped,
}

/// Plan the swarm against a config. Pure function — no I/O. Caller
/// presents the plan to the user; apply() is the mutating sibling.
pub fn plan_swarm(cfg: &OrgReleaseSwarmConfig) -> SwarmPlan {
    cfg.plan()
}

/// Apply the swarm — iterate eligible repos, render workflow.yml,
/// optionally open PRs. `dry_run: true` skips the PR open call and
/// returns `DryRun` outcomes; `dry_run: false` opens PRs via the
/// trait. `render_workflow_yaml_fn` produces the workflow content
/// for a given repo — caller injects this so the renderer (arch-
/// synthesizer's RustToolPublicReleaseDecl::render) stays out of
/// tend's dep tree.
pub async fn apply_swarm<F>(
    api: &dyn ReleaseSwarmApi,
    cfg: &OrgReleaseSwarmConfig,
    dry_run: bool,
    render_workflow_yaml_fn: F,
) -> Result<Vec<RepoApplyReport>>
where
    F: Fn(&str, &RepoReleaseConfig) -> String,
{
    let mut reports = Vec::new();
    if !cfg.enable {
        return Ok(reports);
    }

    for (repo_name, repo_cfg) in cfg.repos.iter().filter(|(n, _)| cfg.is_eligible(n)) {
        let rendered = render_workflow_yaml_fn(repo_name, repo_cfg);
        let outcome = if dry_run {
            ApplyOutcome::DryRun { rendered_bytes: rendered.len() }
        } else {
            // Check for existing file
            let path = ".github/workflows/release.yml";
            let existing = api.get_workflow_file_sha(&cfg.org, repo_name, path).await?;
            // If exists and caller wants to compare, compare bytes —
            // but the GitHub contents API returns base64-encoded
            // content; matching the exact shipped bytes vs the newly
            // rendered bytes is fiddly (line endings, trailing newline).
            // For now: if it exists, still open a PR — the PR diff
            // makes the change visible and reviewable.
            let _ = existing; // acknowledged but not used for comparison yet
            let branch = format!("release-swarm/{}", chrono::Utc::now().format("%Y%m%d%H%M%S"));
            let pr_number = api
                .open_workflow_pr(
                    &cfg.org,
                    repo_name,
                    &branch,
                    path,
                    &rendered,
                    "ci(release): adopt canonical rust-tool-public-release workflow",
                    "Adopt canonical rust-tool-public-release workflow",
                    "Auto-opened by `tend release-swarm apply`. Renders the \
                     typed `RustToolPublicReleaseDecl` from the org swarm \
                     config. See `project_canonical_oss_release_pattern.md` \
                     in user memory for the pattern's empirical proof.",
                )
                .await?;
            ApplyOutcome::PrOpened { pr_number }
        };
        reports.push(RepoApplyReport {
            org: cfg.org.clone(),
            repo: repo_name.clone(),
            outcome,
        });
    }

    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_disabled() {
        let c = OrgReleaseSwarmConfig::default();
        assert!(!c.enable);
        assert!(c.eligible_repos().is_empty());
    }

    #[test]
    fn both_levels_required_for_eligibility() {
        let c = OrgReleaseSwarmConfig::enabled("pleme-io").with_enabled_repo(
            "dq",
            RepoReleaseConfig::default(),
        );
        assert!(c.is_eligible("dq"));
        assert_eq!(c.eligible_repos(), vec!["dq".to_string()]);
    }

    #[test]
    fn akeyless_is_structurally_forbidden_regardless_of_config() {
        // Even when BOTH flags are explicitly true, any repo name
        // matching FORBIDDEN_PATTERNS is denied. No config override.
        let mut c = OrgReleaseSwarmConfig::enabled("pleme-io");
        for name in [
            "akeyless-nix",
            "akeyless-matrix",
            "pangea-akeyless",
            "blackmatter-akeyless",
            "akeyless-terraform-resources",
            "inspec-akeyless",
        ] {
            c = c.with_enabled_repo(name, RepoReleaseConfig::default());
            assert!(
                !c.is_eligible(name),
                "{name} must be structurally forbidden"
            );
        }
        assert!(c.eligible_repos().is_empty());
    }

    #[test]
    fn forbidden_patterns_list_mirrors_arch_synthesizer() {
        // Guard: this list must stay in lockstep with the
        // arch-synthesizer source of truth at
        // `rust_tool_release::swarm::OrgReleaseSwarmConfig::FORBIDDEN_PATTERNS`.
        assert_eq!(
            OrgReleaseSwarmConfig::FORBIDDEN_PATTERNS,
            &["akeyless"]
        );
    }

    #[test]
    fn missing_repo_enable_defaults_to_disabled() {
        let yaml = "org: pleme-io\n\
                    enable: true\n\
                    repos:\n  \
                      dq:\n    \
                        binary_name: dq\n    \
                        features: [lisp]\n";
        let c: OrgReleaseSwarmConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(c.enable);
        assert!(!c.is_eligible("dq"));
    }

    #[test]
    fn explicit_opt_in_yaml_enables_repo() {
        let yaml = "org: pleme-io\n\
                    enable: true\n\
                    repos:\n  \
                      dq:\n    \
                        enable: true\n    \
                        features: [lisp]\n";
        let c: OrgReleaseSwarmConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(c.is_eligible("dq"));
        assert_eq!(c.repos["dq"].features, vec!["lisp".to_string()]);
    }

    #[test]
    fn plan_summary() {
        let c = OrgReleaseSwarmConfig::enabled("pleme-io")
            .with_enabled_repo("dq", RepoReleaseConfig::default())
            .with_enabled_repo("tend", RepoReleaseConfig::default());
        let plan = plan_swarm(&c);
        assert_eq!(plan.eligible_count, 2);
        assert_eq!(plan.eligible_repos, vec!["dq".to_string(), "tend".to_string()]);
    }

    // ── Mock API + apply tests ────────────────────────────────

    use std::sync::Mutex;

    struct MockApi {
        next_pr: Mutex<u64>,
        prs: Mutex<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            Self { next_pr: Mutex::new(100), prs: Mutex::new(Vec::new()) }
        }
    }

    #[async_trait::async_trait]
    impl ReleaseSwarmApi for MockApi {
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
            org: &str,
            repo: &str,
            _branch: &str,
            _path: &str,
            _content: &str,
            _commit_message: &str,
            _pr_title: &str,
            _pr_body: &str,
        ) -> Result<u64> {
            let mut n = self.next_pr.lock().unwrap();
            let pr = *n;
            *n += 1;
            self.prs.lock().unwrap().push(format!("{org}/{repo}#{pr}"));
            Ok(pr)
        }
    }

    #[tokio::test]
    async fn dry_run_skips_pr_open() {
        let c = OrgReleaseSwarmConfig::enabled("pleme-io")
            .with_enabled_repo("dq", RepoReleaseConfig::default());
        let api = MockApi::new();
        let reports =
            apply_swarm(&api, &c, true, |_, _| "fake yaml content".to_string()).await.unwrap();
        assert_eq!(reports.len(), 1);
        match &reports[0].outcome {
            ApplyOutcome::DryRun { rendered_bytes } => {
                assert_eq!(*rendered_bytes, "fake yaml content".len());
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
        assert!(api.prs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn apply_opens_pr_for_each_eligible_repo() {
        let c = OrgReleaseSwarmConfig::enabled("pleme-io")
            .with_enabled_repo("dq", RepoReleaseConfig::default())
            .with_enabled_repo("tend", RepoReleaseConfig::default());
        let api = MockApi::new();
        let reports =
            apply_swarm(&api, &c, false, |name, _| format!("yaml for {name}")).await.unwrap();
        assert_eq!(reports.len(), 2);
        for r in &reports {
            assert!(matches!(r.outcome, ApplyOutcome::PrOpened { .. }));
        }
        let prs = api.prs.lock().unwrap();
        assert_eq!(prs.len(), 2);
    }

    #[tokio::test]
    async fn disabled_org_emits_nothing() {
        let mut c = OrgReleaseSwarmConfig::enabled("pleme-io")
            .with_enabled_repo("dq", RepoReleaseConfig::default());
        c.enable = false; // org-level DENY
        let api = MockApi::new();
        let reports =
            apply_swarm(&api, &c, false, |_, _| "x".into()).await.unwrap();
        assert!(reports.is_empty());
        assert!(api.prs.lock().unwrap().is_empty());
    }
}
