//! The ONE config load path: shikumi's operator fold, assembled for tend.
//!
//! Every subcommand, the daemon's per-cycle reload, the MCP server and the
//! K8s operator resolve [`Config`] here and nowhere else. The fold is
//! shikumi's ([`shikumi::cli::ConfigArgs`] semantics), lowest first:
//!
//! ```text
//! bare → discovered → prescribed_default        computed tiers (config.rs)
//!   → discovered file                           $TEND_CONFIG, tend/tend.yaml,
//!                                               legacy tend/config.yaml
//!   → --config FILE … (in order)                merge-overrides
//!   → TEND_* env (`__` nests)                   config keys only, see below
//!   → the subcommand's typed flags              crate::flags
//!   → --set PATH=VALUE … (in order)
//! ```
//!
//! Maps merge per key; scalars and lists replace. Extraction is strict: an
//! unknown or ill-typed key is refused naming the layer that wrote it.
//!
//! # Why tend assembles the layers instead of calling `ConfigArgs::resolve`
//!
//! Three tend facts shikumi's one-call path cannot know:
//!
//! 1. **The `TEND_` env namespace is shared.** `TEND_STATE_DIR`,
//!    `TEND_PRUNE_DIRENV`, `TEND_SESSION_ID`, `TEND_GITHUB_TOKEN` and a dozen
//!    operator knobs are runtime env, not config paths. shikumi's env layer
//!    takes every `TEND_*` variable, and its strict extraction then refuses
//!    the whole config over `state_dir` — every launchd/systemd unit sets
//!    that one. Here the env layer admits only variables whose first path
//!    segment is a top-level [`Config`] field ([`env_layer`]).
//! 2. **The discovered file has a legacy name.** Nix renders
//!    `~/.config/tend/config.yaml`; shikumi discovers `tend/tend.yaml`.
//!    Discovery is [`Config::default_path`], which tries shikumi first and
//!    falls back to the legacy name.
//! 3. **A legacy per-workspace `prebuild:` block** is hoisted to the
//!    top-level section of the same file ([`hoist_legacy_prebuild`]).
//!
//! Everything else — the slot order, merge, provenance, strict extraction —
//! is shikumi's, unmodified.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use figment::value::{Dict, Tag, Value};
use shikumi::cli::ConfigArgs;
use shikumi::{ProgressiveLayer, ProgressiveResolution, TieredConfig};

use crate::config::Config;

/// The env prefix (`TEND_`).
pub(crate) const ENV_PREFIX: &str = "TEND_";
/// `TEND_*` names shikumi reserves for itself (`TEND_CONFIG`, `TEND_TIER`).
const RESERVED_ENV_KEYS: &[&str] = shikumi::overlay::RESERVED_ENV_KEYS;

/// Where the discovered file comes from.
#[derive(Debug, Clone)]
enum Discovery {
    /// [`Config::default_path`], when it exists.
    Auto,
    /// Exactly this (tests, and callers that name their file).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Option<PathBuf>),
}

/// Where the env layer comes from.
#[derive(Debug, Clone)]
enum EnvSource {
    Process,
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Vec<(String, String)>),
}

/// A reusable resolution request: the operator's `--config`/`--set`, the
/// subcommand's flag overlay, and where to discover. Long-running callers
/// (the daemons) hold one and re-[`Self::load`] it every cycle, so a file
/// edit propagates while a flag keeps beating it.
#[derive(Debug, Clone)]
pub(crate) struct ConfigLoader {
    args: ConfigArgs,
    overlay: serde_json::Value,
    discovery: Discovery,
    env: EnvSource,
}

impl ConfigLoader {
    /// The production loader: discovered file, process env.
    pub(crate) fn new(args: ConfigArgs, overlay: serde_json::Value) -> Self {
        Self {
            args,
            overlay,
            discovery: Discovery::Auto,
            env: EnvSource::Process,
        }
    }

    /// A loader whose discovered file is exactly `file` (or none), with an
    /// explicit env — no ambient state. What tests and file-addressed
    /// callers use.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn hermetic(
        file: Option<PathBuf>,
        args: ConfigArgs,
        env: Vec<(String, String)>,
        overlay: serde_json::Value,
    ) -> Self {
        Self {
            args,
            overlay,
            discovery: Discovery::Fixed(file),
            env: EnvSource::Fixed(env),
        }
    }

    /// The discovered config file, if one exists.
    pub(crate) fn discovered_file(&self) -> Option<PathBuf> {
        match &self.discovery {
            Discovery::Auto => Some(Config::default_path()).filter(|p| p.is_file()),
            Discovery::Fixed(p) => p.clone(),
        }
    }

    /// The file an in-place edit (`tend adopt`) should write: the last
    /// `--config`, else the discovered file, else where `tend init` would
    /// create one.
    pub(crate) fn edit_target(&self) -> PathBuf {
        self.args
            .config
            .last()
            .cloned()
            .or_else(|| self.discovered_file())
            .unwrap_or_else(Config::default_path)
    }

    /// Every file this resolution reads — what a hot-reload watcher watches.
    pub(crate) fn files(&self) -> Vec<PathBuf> {
        self.discovered_file()
            .into_iter()
            .chain(self.args.config.iter().cloned())
            .collect()
    }

    /// The layers above the computed tiers, in shikumi slot order.
    fn layers(&self) -> Result<Vec<ProgressiveLayer>> {
        let mut layers = Vec::with_capacity(self.args.config.len() + 4);
        if let Some(path) = self.discovered_file() {
            let mut dict = ProgressiveLayer::try_from_file(&path)
                .with_context(|| format!("reading config {}", path.display()))?
                .into_dict();
            hoist_legacy_prebuild(&mut dict);
            layers.push(ProgressiveLayer::file(path, dict));
        }
        for path in &self.args.config {
            let mut dict = ProgressiveLayer::try_from_config_override(path)
                .with_context(|| format!("reading --config {}", path.display()))?
                .into_dict();
            hoist_legacy_prebuild(&mut dict);
            layers.push(ProgressiveLayer::config_override(path.clone(), dict));
        }
        let env: Vec<(String, String)> = match &self.env {
            EnvSource::Process => std::env::vars().collect(),
            EnvSource::Fixed(v) => v.clone(),
        };
        layers.push(env_layer(env));
        // No flags at all is an empty map, not null (shikumi wants a map).
        let overlay = if self.overlay.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            self.overlay.clone()
        };
        layers.push(ProgressiveLayer::cli(&overlay).context("CLI flag overlay")?);
        if !self.args.set.is_empty() {
            layers.push(ProgressiveLayer::set(&self.args.set));
        }
        Ok(layers)
    }

    /// Fold and extract, with per-leaf provenance. Does not install anything.
    pub(crate) fn resolve(&self) -> Result<ProgressiveResolution<Config>> {
        let layers = self.layers()?;
        Config::try_resolve_progressive_with(&layers).map_err(|e| anyhow::anyhow!("config: {e}"))
    }

    /// Resolve and install the process-wide state the config owns (the
    /// GitHub credential chain). The entry point every subcommand uses.
    pub(crate) fn load(&self) -> Result<Config> {
        self.load_resolved().map(ProgressiveResolution::into_value)
    }

    /// [`Self::load`], keeping the per-leaf provenance (the daemon publishes
    /// it).
    pub(crate) fn load_resolved(&self) -> Result<ProgressiveResolution<Config>> {
        let resolved = self.resolve()?;
        crate::gh_auth::install(&resolved.value().github_auth.0);
        Ok(resolved)
    }
}

/// The top-level [`Config`] field names — the only first segments the env
/// layer admits.
fn config_keys() -> Vec<String> {
    let value = serde_json::to_value(Config::prescribed_default()).unwrap_or_default();
    value
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// The `TEND_*` env layer, restricted to config paths.
///
/// `TEND_DAEMON__INTERVAL=60` → `daemon.interval: 60` (values parse as
/// figment values: numbers, booleans, `[a, b]` arrays, else strings).
/// `TEND_STATE_DIR`, `TEND_GITHUB_TOKEN`, … name no config field and are
/// left to the code that reads them — they are runtime env, and refusing
/// the whole config over them would take down every unit that sets one.
pub(crate) fn env_layer(vars: impl IntoIterator<Item = (String, String)>) -> ProgressiveLayer {
    let keys = config_keys();
    let mut root = Dict::new();
    for (name, raw) in vars {
        let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let lower = rest.to_ascii_lowercase();
        if RESERVED_ENV_KEYS.contains(&lower.as_str()) {
            continue;
        }
        let path: Vec<&str> = lower.split("__").collect();
        if path.iter().any(|s| s.is_empty()) || !keys.iter().any(|k| k == path[0]) {
            continue;
        }
        let value: Value = raw.parse().unwrap_or_else(|never| match never {});
        insert_path(&mut root, &path, value);
    }
    ProgressiveLayer::env(ENV_PREFIX, root)
}

fn insert_path(root: &mut Dict, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cursor = root;
    for seg in parents {
        let slot = cursor
            .entry((*seg).to_owned())
            .or_insert_with(|| Value::Dict(Tag::Default, Dict::new()));
        if !matches!(slot, Value::Dict(..)) {
            *slot = Value::Dict(Tag::Default, Dict::new());
        }
        let Value::Dict(_, next) = slot else {
            unreachable!("slot was just made a dict")
        };
        cursor = next;
    }
    cursor.insert((*last).to_owned(), value);
}

/// Hoist the first `workspaces[].prebuild` block to the top-level
/// `prebuild:` of the same layer, unless that layer already has one.
///
/// This is exactly the runtime meaning the block always had (the first
/// workspace carrying one configured the whole cycle — see
/// [`crate::config::PrebuildConfig`]), now expressed in the slot the file
/// occupies, so a CLI flag beats it instead of the reverse.
pub(crate) fn hoist_legacy_prebuild(dict: &mut Dict) {
    if dict.contains_key("prebuild") {
        return;
    }
    let first = match dict.get("workspaces") {
        Some(Value::Array(_, items)) => items.iter().find_map(|w| match w {
            Value::Dict(_, ws) => match ws.get("prebuild") {
                Some(p @ Value::Dict(..)) => Some(p.clone()),
                _ => None,
            },
            _ => None,
        }),
        _ => None,
    };
    if let Some(block) = first {
        dict.insert("prebuild".to_owned(), block);
    }
}

/// A resolution as one JSON value: `{value, provenance: {path: source}}`.
pub(crate) fn effective_json(resolved: &ProgressiveResolution<Config>) -> serde_json::Value {
    let provenance: serde_json::Map<String, serde_json::Value> = resolved
        .provenance()
        .iter()
        .map(|(path, prov)| (path.join("."), serde_json::Value::String(prov.to_string())))
        .collect();
    serde_json::json!({
        "value": serde_json::to_value(resolved.value()).unwrap_or_default(),
        "provenance": provenance,
    })
}

/// Resolve the config from one file over the defaults — no discovery, no
/// env. The `Config::load(path)` compatibility surface.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn load_file(path: &Path) -> Result<Config> {
    if !path.is_file() {
        anyhow::bail!("config file {} does not exist", path.display());
    }
    ConfigLoader::hermetic(
        Some(path.to_path_buf()),
        ConfigArgs::default(),
        Vec::new(),
        serde_json::Value::Object(serde_json::Map::new()),
    )
    .resolve()
    .map(ProgressiveResolution::into_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::{self, Overlay};
    use clap::Parser;

    fn temp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tend-config-layers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn args(config: &[PathBuf], set: &[&str]) -> ConfigArgs {
        ConfigArgs {
            config: config.to_vec(),
            set: set.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Parse the flags of `tend daemon …` the way main.rs does.
    fn daemon_overlay(argv: &[&str]) -> serde_json::Value {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            flags: flags::DaemonFlags,
        }
        let cli = Cli::try_parse_from(std::iter::once("tend").chain(argv.iter().copied())).unwrap();
        cli.flags.overlay()
    }

    fn source_of(r: &ProgressiveResolution<Config>, path: &[&str]) -> String {
        r.provenance()
            .provenance_of(path)
            .map(|p| p.to_string())
            .unwrap_or_default()
    }

    /// ★ flag > env > --config > file > default, for one daemon field, one
    /// layer added at a time — and each step names the layer that won.
    #[test]
    fn daemon_interval_precedence_is_flag_env_config_file_default() {
        let file = temp("prec-file.yaml", "daemon:\n  interval: 100\n");
        let over = temp("prec-over.yaml", "daemon:\n  interval: 200\n");
        let none = serde_json::Value::Null;

        let r = ConfigLoader::hermetic(None, args(&[], &[]), vec![], none.clone())
            .resolve()
            .unwrap();
        assert_eq!(r.value().daemon.interval, 300, "the prescribed default");

        let r = ConfigLoader::hermetic(Some(file.clone()), args(&[], &[]), vec![], none.clone())
            .resolve()
            .unwrap();
        assert_eq!(r.value().daemon.interval, 100);
        assert!(source_of(&r, &["daemon", "interval"]).contains("prec-file.yaml"));

        let r = ConfigLoader::hermetic(
            Some(file.clone()),
            args(std::slice::from_ref(&over), &[]),
            vec![],
            none.clone(),
        )
        .resolve()
        .unwrap();
        assert_eq!(r.value().daemon.interval, 200);
        assert!(source_of(&r, &["daemon", "interval"]).contains("prec-over.yaml"));

        let e = env(&[("TEND_DAEMON__INTERVAL", "250")]);
        let r = ConfigLoader::hermetic(
            Some(file.clone()),
            args(std::slice::from_ref(&over), &[]),
            e.clone(),
            none,
        )
        .resolve()
        .unwrap();
        assert_eq!(r.value().daemon.interval, 250);

        let flag = daemon_overlay(&["--interval", "260"]);
        let r = ConfigLoader::hermetic(
            Some(file.clone()),
            args(std::slice::from_ref(&over), &[]),
            e.clone(),
            flag.clone(),
        )
        .resolve()
        .unwrap();
        assert_eq!(r.value().daemon.interval, 260);

        let r =
            ConfigLoader::hermetic(Some(file), args(&[over], &["daemon.interval=270"]), e, flag)
                .resolve()
                .unwrap();
        assert_eq!(r.value().daemon.interval, 270, "--set is the top");
    }

    /// An absent flag contributes nothing: the file value stands. (The old
    /// clap `default_value = "300"` made every unflagged run say 300.)
    #[test]
    fn an_absent_flag_leaves_the_file_value_standing() {
        let file = temp("absent.yaml", "daemon:\n  interval: 42\n  pull: false\n");
        let flag = daemon_overlay(&["--max-inflight", "3"]);
        let cfg = ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], flag)
            .resolve()
            .unwrap()
            .into_value();
        assert_eq!(cfg.daemon.interval, 42);
        assert!(!cfg.daemon.pull);
        assert_eq!(
            cfg.reconcile.max_inflight, 3,
            "--max-inflight → reconcile.max_inflight"
        );
    }

    /// The launchd/systemd units' exact argv still parses, including the
    /// space-separated `--pull true` form and bare `--quiet`.
    #[test]
    fn the_nix_module_argv_still_parses() {
        let o = daemon_overlay(&[
            "--interval",
            "300",
            "--quiet",
            "--pull",
            "false",
            "--fetch",
            "true",
            "--max-inflight",
            "16",
        ]);
        assert_eq!(
            o,
            serde_json::json!({
                "daemon": {"interval": 300, "quiet": true, "pull": false, "fetch": true},
                "reconcile": {"max_inflight": 16}
            })
        );
    }

    /// `TEND_*` runtime env is not config: it must neither feed the fold
    /// nor make the strict extraction refuse the whole config.
    #[test]
    fn runtime_tend_env_vars_do_not_break_loading() {
        let e = env(&[
            ("TEND_STATE_DIR", "/var/lib/tend"),
            ("TEND_GITHUB_TOKEN", "ghp_x"),
            ("TEND_SESSION_ID", "abc"),
            ("TEND_PRUNE_DIRENV", "1"),
            ("TEND_CONFIG", "/nowhere"),
            ("TEND_RECONCILE__MAX_INFLIGHT", "5"),
        ]);
        let cfg = ConfigLoader::hermetic(None, args(&[], &[]), e, serde_json::Value::Null)
            .resolve()
            .unwrap()
            .into_value();
        assert_eq!(
            cfg.reconcile.max_inflight, 5,
            "a config-shaped var still lands"
        );
    }

    /// A typo in a FILE is still refused, naming the file.
    #[test]
    fn an_unknown_file_key_is_refused_naming_its_layer() {
        let file = temp("typo.yaml", "daemon:\n  intervall: 5\n");
        let err =
            ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], serde_json::Value::Null)
                .resolve()
                .unwrap_err()
                .to_string();
        assert!(err.contains("daemon.intervall"), "{err}");
        assert!(err.contains("typo.yaml"), "{err}");
    }

    /// ★ The config Nix renders today (a live `~/.config/tend/config.yaml`
    /// of 2026-10-09, re-serialized, only repo names pruned from
    /// `flake_deps`) keeps loading through the strict fold —
    /// including `flake_input_watches[].auto_propagate: false`, a bool the
    /// old serde_yaml_ng parse read as the string "false".
    #[test]
    fn the_nix_rendered_config_keeps_parsing() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nix-rendered-config.yaml");
        let cfg =
            ConfigLoader::hermetic(Some(path), args(&[], &[]), vec![], serde_json::Value::Null)
                .resolve()
                .unwrap()
                .into_value();
        assert_eq!(cfg.workspaces[0].name, "pleme-io");
        let w = &cfg.workspaces[0]
            .watch
            .as_ref()
            .unwrap()
            .flake_input_watches[0];
        assert_eq!(
            w.auto_propagate, None,
            "false means no propagation, not a repo named false"
        );
        assert_eq!(
            cfg.daemon.interval, 300,
            "keys the file omits take the defaults"
        );
    }

    #[test]
    fn github_auth_chain_from_yaml() {
        let file = temp(
            "auth.yaml",
            "github_auth:\n  - token: {env: MY_TOKEN}\n  - gh_cli: {host: github.example}\n  \
             - app: {app_id: 123, owner: pleme-io, private_key: {file: /run/key.pem}}\n",
        );
        let cfg =
            ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], serde_json::Value::Null)
                .resolve()
                .unwrap()
                .into_value();
        let kinds: Vec<&str> = cfg
            .github_auth
            .0
            .iter()
            .map(|a| a.kind().as_str())
            .collect();
        assert_eq!(
            kinds,
            ["token", "gh_cli", "app"],
            "the file REPLACES the default chain"
        );

        // One source map (and the `chain:` form) is a one-element list.
        let file = temp(
            "auth-map.yaml",
            "github_auth:\n  chain:\n    - token: {env: A}\n    - gh_cli: {}\n",
        );
        let cfg =
            ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], serde_json::Value::Null)
                .resolve()
                .unwrap()
                .into_value();
        assert_eq!(cfg.github_auth.0.len(), 1);
        assert_eq!(cfg.github_auth.0[0].kind().as_str(), "chain");
    }

    /// `--github-token-file` replaces whatever `github_auth` the file holds
    /// (a partial over the live config — never a merge into a two-key map).
    #[test]
    fn github_token_file_flag_replaces_github_auth() {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            github: flags::GithubFlags,
        }
        let cli = Cli::try_parse_from(["tend", "--github-token-file", "/run/secrets/gh"]).unwrap();
        let file = temp("auth-file.yaml", "github_auth:\n  - gh_cli: {}\n");
        let r = ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], cli.github.overlay())
            .resolve()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&r.value().github_auth).unwrap(),
            serde_json::json!([{ "token": { "file": "/run/secrets/gh" } }])
        );
        let src = source_of(&r, &["github_auth"]);
        assert!(src.contains("flags"), "attributed to the flag layer: {src}");

        let cli = Cli::try_parse_from([
            "tend",
            "--github-app-id",
            "42",
            "--github-app-key-file",
            "/run/app.pem",
            "--github-app-owner",
            "pleme-io",
        ])
        .unwrap();
        let cfg = ConfigLoader::hermetic(None, args(&[], &[]), vec![], cli.github.overlay())
            .resolve()
            .unwrap()
            .into_value();
        assert_eq!(cfg.github_auth.0.len(), 1);
        assert_eq!(
            cfg.github_auth.0[0].describe(),
            "GitHub App 42 installed on pleme-io"
        );

        assert!(
            Cli::try_parse_from(["tend", "--github-app-id", "42"]).is_err(),
            "an App id without its key is refused at parse time"
        );
    }

    /// ★ Prebuild precedence: CLI beats YAML — including the legacy
    /// per-workspace block, which used to override `--repro`/`--packages`.
    #[test]
    fn prebuild_flags_beat_the_legacy_workspace_block() {
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            flags: flags::PrebuildDaemonFlags,
        }
        let file = temp(
            "prebuild.yaml",
            "workspaces:\n  - name: a\n    base_dir: /a\n    prebuild:\n      repro: trusting\n      \
             packages: default\n      max_inflight: 4\n",
        );
        let none = ConfigLoader::hermetic(
            Some(file.clone()),
            args(&[], &[]),
            vec![],
            serde_json::Value::Null,
        )
        .resolve()
        .unwrap()
        .into_value();
        assert_eq!(
            none.prebuild.packages, "default",
            "the legacy block is hoisted"
        );
        assert_eq!(none.prebuild.max_inflight, 4);

        let cli = Cli::try_parse_from([
            "tend",
            "--repro",
            "verify",
            "--packages",
            "all",
            "--min-interval",
            "7",
            "--attic-probe",
            "false",
        ])
        .unwrap();
        let cfg = ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], cli.flags.overlay())
            .resolve()
            .unwrap()
            .into_value();
        assert_eq!(cfg.prebuild.repro, "verify");
        assert_eq!(cfg.prebuild.packages, "all");
        assert_eq!(
            cfg.prebuild.max_inflight, 4,
            "unflagged keys keep the file's value"
        );
        assert_eq!(cfg.prebuild.min_interval, 7);
        assert!(!cfg.prebuild.probe.enable);
    }

    /// ★ `config-show --effective` shows the REAL workspaces (the old
    /// `config-show` printed a tier whose `workspaces` was always `[]`), and
    /// `--provenance` attributes them to the file.
    #[test]
    fn config_show_effective_shows_the_real_workspaces() {
        let file = temp(
            "show.yaml",
            "workspaces:\n  - name: real-ws\n    base_dir: /x\n",
        );
        let r = ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], serde_json::Value::Null)
            .resolve()
            .unwrap();
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            show: shikumi::cli::ConfigShowCommand,
        }
        let plain = Cli::try_parse_from(["tend", "--effective"]).unwrap();
        let out = plain.show.render_effective(&r).unwrap();
        assert!(out.contains("real-ws"), "{out}");
        assert!(
            out.contains("TEND_GITHUB_TOKEN"),
            "defaults are real: {out}"
        );

        let prov = Cli::try_parse_from(["tend", "--effective", "--provenance"]).unwrap();
        let out = prov.show.render_effective(&r).unwrap();
        assert!(out.contains("show.yaml"), "{out}");
    }

    #[test]
    fn hoist_keeps_an_explicit_top_level_section() {
        let file = temp(
            "hoist-both.yaml",
            "prebuild:\n  packages: mado\nworkspaces:\n  - name: a\n    base_dir: /a\n    prebuild:\n      packages: tear\n",
        );
        let cfg =
            ConfigLoader::hermetic(Some(file), args(&[], &[]), vec![], serde_json::Value::Null)
                .resolve()
                .unwrap()
                .into_value();
        assert_eq!(cfg.prebuild.packages, "mado");
    }
}
