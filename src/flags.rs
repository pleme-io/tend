//! Every CLI flag that sets a config value, as a PARTIAL of [`Config`].
//!
//! A flag here is an `Option`, and the struct's serde shape is the config
//! path it overrides: `tend daemon --interval 60` serializes to
//! `{daemon: {interval: 60}}`, which shikumi folds above the file and env
//! layers. An absent flag serializes to null and contributes nothing, so
//! the file (or env, or `--config`) value stands. There is no second
//! default: the prescribed default lives once, in `config.rs`, and
//! `tend config-show --effective --provenance` names which layer set every
//! leaf.
//!
//! Flags that are not configuration — a `--workspace` filter, `--json`,
//! `--refresh`, a one-shot `--dry-run` — stay plain clap fields on the
//! subcommand and never enter the fold.
//!
//! [`Config`]: crate::config::Config

use std::path::PathBuf;

use clap::{ArgAction, Args};
use serde::Serialize;
use serde_json::{json, Value};

/// A subcommand's flags as an overlay value.
pub(crate) trait Overlay {
    fn overlay(&self) -> Value;
}

impl<T: Serialize> Overlay for T {
    fn overlay(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Deep-merge overlays; later wins. Each source's keys are disjoint in
/// practice (GitHub flags own `github_auth`, a subcommand its own section).
pub(crate) fn merge(overlays: impl IntoIterator<Item = Value>) -> Value {
    fn merge_into(base: &mut Value, next: Value) {
        match (base, next) {
            (Value::Object(b), Value::Object(n)) => {
                for (k, v) in n {
                    merge_into(b.entry(k).or_insert(Value::Null), v);
                }
            }
            (slot, Value::Null) if !slot.is_null() => {}
            (slot, v) => *slot = v,
        }
    }
    let mut out = Value::Object(serde_json::Map::new());
    for o in overlays {
        merge_into(&mut out, o);
    }
    out
}

// ── GitHub credential — global, on every subcommand ─────────────────────

/// Credential flags. Each is sugar for a `github_auth:` list, and ANY of
/// them REPLACES the configured `github_auth` for this invocation — it does
/// not prepend to it. A flag is the operator saying "use this credential",
/// and a silent fallback to the configured chain when the named file is
/// unreadable would hide exactly the failure the flag was passed to fix. To
/// combine a flag-given source with others, write the list:
/// `--set 'github_auth=[{token: {file: /run/t}}, {gh_cli: {}}]'`.
///
/// When both a token file and an App are given they form one list, App
/// first: `[{app: …}, {token: {file: …}}]`.
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct GithubFlags {
    /// GitHub token file. Sugar for `github_auth: [{token: {file: PATH}}]`,
    /// replacing the configured chain. Re-read on every resolution, so a
    /// rotated file takes effect within one cycle.
    #[arg(long, global = true, value_name = "PATH")]
    pub github_token_file: Option<PathBuf>,

    /// GitHub App id. With `--github-app-key-file`, sugar for
    /// `github_auth: [{app: {...}}]`, replacing the configured chain.
    #[arg(
        long,
        global = true,
        value_name = "ID",
        requires = "github_app_key_file"
    )]
    pub github_app_id: Option<u64>,

    /// PEM private key of the GitHub App.
    #[arg(long, global = true, value_name = "PATH", requires = "github_app_id")]
    pub github_app_key_file: Option<PathBuf>,

    /// Org or user the App installation belongs to (looked up when no
    /// installation id is given).
    #[arg(long, global = true, value_name = "OWNER", requires = "github_app_id")]
    pub github_app_owner: Option<String>,

    /// The App installation id (skips the lookup).
    #[arg(long, global = true, value_name = "ID", requires = "github_app_id")]
    pub github_app_installation_id: Option<u64>,
}

impl Overlay for GithubFlags {
    fn overlay(&self) -> Value {
        let mut sources = Vec::new();
        if let (Some(app_id), Some(key)) = (self.github_app_id, &self.github_app_key_file) {
            let mut app = json!({ "app_id": app_id, "private_key": { "file": key } });
            if let Some(owner) = &self.github_app_owner {
                app["owner"] = json!(owner);
            }
            if let Some(id) = self.github_app_installation_id {
                app["installation_id"] = json!(id);
            }
            sources.push(json!({ "app": app }));
        }
        if let Some(path) = &self.github_token_file {
            sources.push(json!({ "token": { "file": path } }));
        }
        if sources.is_empty() {
            Value::Null
        } else {
            json!({ "github_auth": sources })
        }
    }
}

// ── reconcile / daemon ──────────────────────────────────────────────────

/// `reconcile.*`
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct ReconcileSection {
    /// Maximum concurrent `git pull` processes per workspace
    /// (`reconcile.max_inflight`; default 16).
    #[arg(long, value_name = "N")]
    pub max_inflight: Option<u32>,
}

/// Flags of `tend reconcile` and `tend pressure`.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct ReconcileFlags {
    #[command(flatten)]
    pub reconcile: ReconcileSection,
}

/// `daemon.*`
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct DaemonSection {
    /// Seconds between cycles (`daemon.interval`; default 300).
    #[arg(long, value_name = "SECS")]
    pub interval: Option<u64>,

    /// Fast-forward clean repos every cycle (`daemon.pull`; default true).
    /// Bare `--pull` is true; `--pull false` / `--pull=false` turns it off.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = false,
        action = ArgAction::Set,
    )]
    pub pull: Option<bool>,

    /// Plain `git fetch --all --prune` each cycle when pull is off
    /// (`daemon.fetch`; default true). Same value forms as `--pull`.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = false,
        action = ArgAction::Set,
    )]
    pub fetch: Option<bool>,

    /// Suppress per-repo output (`daemon.quiet`). `--quiet=false` overrides
    /// a config that sets it.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub quiet: Option<bool>,
}

/// Flags of `tend daemon`.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct DaemonFlags {
    #[command(flatten)]
    pub daemon: DaemonSection,
    #[command(flatten)]
    pub reconcile: ReconcileSection,
}

/// `flake_update_daemon.*`
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct FlakeUpdateDaemonSection {
    /// Sleep after a cycle that did work, seconds (default 60).
    #[arg(long, value_name = "SECS")]
    pub min_interval: Option<u64>,
    /// Ceiling of the converged backoff, seconds (default 3600).
    #[arg(long, value_name = "SECS")]
    pub max_interval: Option<u64>,
    /// Suppress per-step output.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub quiet: Option<bool>,
}

/// Flags of `tend flake-update-daemon`.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct FlakeUpdateDaemonFlags {
    #[command(flatten)]
    pub flake_update_daemon: FlakeUpdateDaemonSection,
}

// ── prebuild ────────────────────────────────────────────────────────────

/// `prebuild.caches`, given as the JSON list the Nix module renders:
/// `[{name, server, url, token_file, enabled?, backend?}]`.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub(crate) struct CachesJson(pub Vec<crate::config::CacheTargetConfig>);

fn parse_caches_json(raw: &str) -> Result<CachesJson, String> {
    crate::prebuild_cache::parse_caches_json(raw)
        .map(|targets| {
            CachesJson(
                targets
                    .into_iter()
                    .map(|t| crate::config::CacheTargetConfig {
                        backend: t.backend,
                        cache: t.cache_name,
                        server: t.server_name,
                        url: t.server_url,
                        token_file: t.token_file,
                        enabled: t.enabled,
                    })
                    .collect(),
            )
        })
        .map_err(|e| format!("not a JSON cache list: {e}"))
}

/// `prebuild.*` — shared by `tend prebuild` and `tend prebuild-daemon`.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct PrebuildSection {
    /// Maximum concurrent `nix build` invocations (default 1).
    #[arg(long, value_name = "N")]
    pub max_inflight: Option<usize>,
    /// Suppress per-repo log lines (the audit log is still written).
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub quiet: Option<bool>,
    /// Attic cache name (e.g. `nexus`). Unset builds without pushing.
    #[arg(long, value_name = "NAME")]
    pub attic_cache: Option<String>,
    /// Attic server alias for `attic login` (default `nexus`).
    #[arg(long, value_name = "NAME")]
    pub attic_server: Option<String>,
    /// Attic server URL (e.g. `http://rio:8080/`).
    #[arg(long, value_name = "URL")]
    pub attic_url: Option<String>,
    /// File holding the Attic JWT (SOPS-managed).
    #[arg(long, value_name = "PATH")]
    pub attic_token_file: Option<String>,
    /// Flake outputs to build: "all" (default), "default", or a
    /// comma-separated allow-list ("mado,tear").
    #[arg(long, value_name = "SELECTOR")]
    pub packages: Option<String>,
    /// Reproducibility gate before pushing: "trusting" (default) or
    /// "verify" (never push a non-reproducible closure).
    #[arg(long, value_name = "POLICY")]
    pub repro: Option<String>,
    /// JSON list of cache targets every closure fans out to — rendered by
    /// the Nix module: `[{name, server, url, token_file, enabled?}]`.
    /// Replaces `prebuild.caches`.
    #[arg(long = "caches-json", value_name = "JSON", value_parser = parse_caches_json)]
    pub caches: Option<CachesJson>,
}

/// Flags of `tend prebuild`.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct PrebuildFlags {
    #[command(flatten)]
    pub prebuild: PrebuildSection,
}

/// `prebuild.probe.*`
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct ProbeSection {
    /// Probe the Attic server before each cycle and back off while it is
    /// down (`prebuild.probe.enable`; default true, effective only with an
    /// attic URL). `--attic-probe false` disables.
    #[arg(id = "attic_probe", long = "attic-probe", action = ArgAction::Set, value_name = "BOOL")]
    pub enable: Option<bool>,
    /// Floor of the unreachable backoff, seconds (default 60).
    #[arg(
        id = "attic_unreachable_min_interval",
        long = "attic-unreachable-min-interval",
        value_name = "SECS"
    )]
    pub min_interval: Option<u64>,
    /// Ceiling of the unreachable backoff, seconds (default 1800).
    #[arg(
        id = "attic_unreachable_max_interval",
        long = "attic-unreachable-max-interval",
        value_name = "SECS"
    )]
    pub max_interval: Option<u64>,
    /// Per-probe HTTP timeout, seconds (default 5).
    #[arg(
        id = "attic_probe_timeout",
        long = "attic-probe-timeout",
        value_name = "SECS"
    )]
    pub timeout: Option<u64>,
}

/// `prebuild.*` plus the daemon-only pacing keys.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct PrebuildDaemonSection {
    #[command(flatten)]
    #[serde(flatten)]
    pub base: PrebuildSection,
    /// Minimum sleep between cycles, seconds (default 120).
    #[arg(long, value_name = "SECS")]
    pub min_interval: Option<u64>,
    /// Maximum sleep when converged, seconds (default 3600).
    #[arg(long, value_name = "SECS")]
    pub max_interval: Option<u64>,
    #[command(flatten)]
    pub probe: ProbeSection,
}

/// Flags of `tend prebuild-daemon`.
#[derive(Debug, Clone, Default, Args, Serialize)]
pub(crate) struct PrebuildDaemonFlags {
    #[command(flatten)]
    pub prebuild: PrebuildDaemonSection,
}

// ── one-shot flags that are partials too ────────────────────────────────

/// `tend status --no-fix` is `host_health.fix: false` for one invocation.
pub(crate) fn status_overlay(no_fix: bool) -> Value {
    if no_fix {
        json!({ "host_health": { "fix": false } })
    } else {
        Value::Null
    }
}

/// `tend cargo-target apply --dry-run` is `cargo_target.dry_run: true`.
pub(crate) fn cargo_target_overlay(dry_run: bool) -> Value {
    if dry_run {
        json!({ "cargo_target": { "dry_run": true } })
    } else {
        Value::Null
    }
}
