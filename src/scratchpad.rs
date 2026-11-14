use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::cargo_target::{discover, walk_tree, Admission};

pub(crate) const POLICY_SOURCE: &str = include_str!("../blue/scratchpad.b");
const SHOWN: &str = "scratchpad.b";
const SCRATCHPAD: &str = "scratchpad";
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ScratchpadConfig {
    pub enabled: bool,
    pub threshold_gib: u64,
    pub duplicate_targets: bool,
    pub root: String,
    pub search_depth: u32,
    pub interval_minutes: u64,
    pub hook_session_start: bool,
}

impl Default for ScratchpadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_gib: 20,
            duplicate_targets: true,
            root: String::new(),
            search_depth: 3,
            interval_minutes: 10,
            hook_session_start: true,
        }
    }
}

impl ScratchpadConfig {
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_minutes.saturating_mul(60))
    }

    #[must_use]
    pub fn root_path(&self) -> Option<PathBuf> {
        let named = self.root.trim();
        if !named.is_empty() {
            return Some(PathBuf::from(shellexpand::tilde(named).into_owned()));
        }
        derived_root(
            std::env::var_os("CLAUDE_CODE_TMPDIR").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )
    }

    fn settings(&self) -> Json {
        json!({
            "threshold": crate::cargo_target::int(self.threshold_gib.saturating_mul(GIB)),
            "duplicates": self.duplicate_targets,
        })
    }
}

fn derived_root(tmp: Option<PathBuf>, home: Option<&Path>) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let uid = std::fs::metadata(home?).ok()?.uid();
    Some(
        tmp.unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(format!("claude-{uid}")),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub path: PathBuf,
    pub bytes: u64,
    pub checkout: Option<PathBuf>,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pad {
    pub path: PathBuf,
    pub bytes: u64,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub at_unix: u64,
    pub root: PathBuf,
    pub pads: Vec<Pad>,
    pub verdicts: Vec<Json>,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Sight {
    Off,
    NoRoot,
    Seen(Snapshot),
}

pub(crate) fn pads_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for project in dirs_in(root) {
        for session in dirs_in(&project) {
            let pad = session.join(SCRATCHPAD);
            if std::fs::symlink_metadata(&pad).is_ok_and(|m| m.is_dir()) {
                out.push(pad);
            }
        }
    }
    out.sort();
    out
}

fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn measure_pad(pad: &Path, depth: u32) -> Pad {
    let found = discover(&[pad.to_path_buf()], depth, Admission::default());
    let targets = found
        .dirs
        .into_iter()
        .map(|(_, dir)| {
            let path = dir.path().to_path_buf();
            let bytes = walk_tree(&path, &mut |_, _, _| {}).map_or(0, |f| f.bytes);
            let checkout = checkout_of(pad, &path);
            let repo = checkout.as_deref().and_then(repo_of);
            Target {
                path,
                bytes,
                checkout,
                repo,
            }
        })
        .collect();
    Pad {
        path: pad.to_path_buf(),
        bytes: walk_tree(pad, &mut |_, _, _| {}).map_or(0, |f| f.bytes),
        targets,
    }
}

fn checkout_of(pad: &Path, target: &Path) -> Option<PathBuf> {
    target
        .ancestors()
        .skip(1)
        .take_while(|a| a.starts_with(pad) && *a != pad)
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

fn common_dir(checkout: &Path) -> Option<PathBuf> {
    let dotgit = checkout.join(".git");
    let meta = std::fs::symlink_metadata(&dotgit).ok()?;
    if meta.is_dir() {
        return Some(dotgit);
    }
    let text = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir = checkout.join(text.trim().strip_prefix("gitdir:")?.trim());
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => Some(gitdir.join(common.trim())),
        Err(_) => Some(gitdir),
    }
}

fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
        } else if in_origin {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "url" {
                    return Some(value.trim().to_owned());
                }
            }
        }
    }
    None
}

pub(crate) fn repo_of(checkout: &Path) -> Option<String> {
    let common = common_dir(checkout)?;
    let slug = std::fs::read_to_string(common.join("config"))
        .ok()
        .as_deref()
        .and_then(origin_url)
        .and_then(|u| crate::remote_url::GitHubSlug::parse(&u))
        .map(|s| s.slug());
    Some(slug.unwrap_or_else(|| {
        std::fs::canonicalize(&common)
            .unwrap_or(common)
            .display()
            .to_string()
    }))
}

fn pad_json(pad: &Pad) -> Json {
    json!({
        "path": pad.path.display().to_string(),
        "bytes": crate::cargo_target::int(pad.bytes),
        "targets": pad.targets.iter().map(|t| json!({
            "path": t.path.display().to_string(),
            "bytes": crate::cargo_target::int(t.bytes),
            "checkout": t.checkout.as_ref().map(|c| c.display().to_string()),
            "repo": t.repo,
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn judge(pads: &[Pad], config: &ScratchpadConfig) -> Result<(Vec<Json>, Vec<String>)> {
    let mut program = crate::blue_policy::load_pure(None, POLICY_SOURCE, SHOWN)?;
    let facts = Json::Array(pads.iter().map(pad_json).collect());
    let verdicts = crate::blue_policy::call_on(
        &mut program,
        SHOWN,
        "scratchpad_verdicts",
        &[facts, config.settings()],
    )?;
    let lines = crate::blue_policy::call_on(
        &mut program,
        SHOWN,
        "lines",
        std::slice::from_ref(&verdicts),
    )?;
    let verdicts = verdicts
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("{SHOWN}: scratchpad_verdicts() answered {verdicts}"))?;
    let lines = lines
        .as_array()
        .map(|ls| {
            ls.iter()
                .filter_map(Json::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .ok_or_else(|| anyhow!("{SHOWN}: lines() answered {lines}"))?;
    Ok((verdicts, lines))
}

pub(crate) fn observe(config: &ScratchpadConfig, root: &Path, now: SystemTime) -> Result<Snapshot> {
    let pads: Vec<Pad> = pads_under(root)
        .iter()
        .map(|p| measure_pad(p, config.search_depth))
        .collect();
    let (verdicts, lines) = judge(&pads, config)?;
    Ok(Snapshot {
        at_unix: crate::heartbeat::unix(now),
        root: root.to_path_buf(),
        pads,
        verdicts,
        lines,
    })
}

pub(crate) fn sense(config: &ScratchpadConfig, now: SystemTime) -> Result<Sight> {
    if !config.enabled {
        return Ok(Sight::Off);
    }
    match config.root_path().filter(|r| r.is_dir()) {
        Some(root) => Ok(Sight::Seen(observe(config, &root, now)?)),
        None => Ok(Sight::NoRoot),
    }
}

pub(crate) fn snapshot_path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("daemon")
        .join("scratchpad.json")
}

pub(crate) fn read_snapshot(path: &Path) -> Option<Snapshot> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub(crate) fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(snapshot)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn refresh(config: &ScratchpadConfig, path: &Path, now: SystemTime) -> Result<Sight> {
    let sight = sense(config, now)?;
    if let Sight::Seen(snapshot) = &sight {
        write_snapshot(path, snapshot)?;
    }
    Ok(sight)
}

pub(crate) fn session_lines(
    config: &ScratchpadConfig,
    path: &Path,
    now: SystemTime,
) -> Vec<String> {
    if !config.enabled || !config.hook_session_start {
        return Vec::new();
    }
    let Some(snapshot) = read_snapshot(path) else {
        return Vec::new();
    };
    let age = crate::heartbeat::unix(now).saturating_sub(snapshot.at_unix);
    snapshot
        .lines
        .into_iter()
        .map(|l| format!("{l} (measured {}m ago)", age / 60))
        .collect()
}

pub(crate) fn print(sight: &Sight) {
    match sight {
        Sight::Off => println!("scratchpad    off (scratchpad.enabled: false)"),
        Sight::NoRoot => println!("scratchpad    no Claude temp root on this host"),
        Sight::Seen(s) => {
            println!(
                "scratchpad    {} scratchpad(s) under {}",
                s.pads.len(),
                s.root.display()
            );
            for pad in &s.pads {
                println!(
                    "  {:>7.1} GiB  {} ({} target(s))",
                    crate::cargo_target::to_gib(pad.bytes),
                    pad.path.display(),
                    pad.targets.len()
                );
                for t in &pad.targets {
                    println!(
                        "  {:>7.1} GiB    {} [{}]",
                        crate::cargo_target::to_gib(t.bytes),
                        t.path.display(),
                        t.repo.as_deref().unwrap_or("shared")
                    );
                }
            }
            if s.lines.is_empty() {
                println!("verdict       ✅ no scratchpad needs attention");
            }
            for line in &s.lines {
                println!("{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TAG: &str =
        "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n";

    fn checkout(at: &Path, origin: &str, target_bytes: usize) {
        std::fs::create_dir_all(at.join(".git")).unwrap();
        std::fs::write(
            at.join(".git/config"),
            format!("[core]\n\tbare = false\n[remote \"origin\"]\n\turl = {origin}\n"),
        )
        .unwrap();
        std::fs::write(at.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(at.join("target/debug")).unwrap();
        std::fs::write(at.join("target/CACHEDIR.TAG"), TAG).unwrap();
        std::fs::write(at.join("target/debug/blob"), vec![7u8; target_bytes]).unwrap();
    }

    fn session(root: &Path) -> PathBuf {
        let pad = root.join("-Users-x-code-repo/0000-session/scratchpad");
        std::fs::create_dir_all(&pad).unwrap();
        pad
    }

    fn config(root: &Path) -> ScratchpadConfig {
        ScratchpadConfig {
            root: root.display().to_string(),
            ..ScratchpadConfig::default()
        }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn two_checkouts_of_one_repo_each_with_a_target_are_flagged_with_the_shared_target_fix() {
        let tmp = TempDir::new().unwrap();
        let pad = session(tmp.path());
        checkout(
            &pad.join("tend"),
            "git@github.com:pleme-io/tend.git",
            64 * 1024,
        );
        checkout(
            &pad.join("tend-scratch"),
            "https://github.com/pleme-io/tend",
            64 * 1024,
        );
        checkout(
            &pad.join("other"),
            "git@github.com:pleme-io/other.git",
            4096,
        );
        let Sight::Seen(s) = sense(&config(tmp.path()), at(1_000)).unwrap() else {
            panic!("the root exists, so the sensor sees it")
        };
        assert_eq!(s.pads.len(), 1);
        assert_eq!(s.pads[0].targets.len(), 3);
        let dup: Vec<&Json> = s
            .verdicts
            .iter()
            .filter(|v| v["verdict"] == "scratchpad-duplicate-targets")
            .collect();
        assert_eq!(dup.len(), 1, "{:#?}", s.verdicts);
        assert_eq!(dup[0]["repo"], "pleme-io/tend");
        assert_eq!(dup[0]["checkouts"].as_array().unwrap().len(), 2);
        assert!(
            s.lines
                .iter()
                .any(|l| l.contains("scratchpad-duplicate-targets")
                    && l.contains("CARGO_TARGET_DIR")),
            "{:#?}",
            s.lines
        );
        assert!(!s
            .verdicts
            .iter()
            .any(|v| v["verdict"] == "scratchpad-oversize"));
    }

    #[test]
    fn a_worktree_of_a_checkout_counts_as_the_same_repo() {
        let tmp = TempDir::new().unwrap();
        let pad = session(tmp.path());
        checkout(&pad.join("main"), "file:///nowhere/repo", 4096);
        let wt = pad.join("wt");
        let gitdir = pad.join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(wt.join("target/CACHEDIR.TAG"), TAG).unwrap();
        let measured = measure_pad(&pad, 3);
        let repos: Vec<_> = measured.targets.iter().map(|t| t.repo.clone()).collect();
        assert_eq!(repos.len(), 2);
        assert!(repos[0].is_some());
        assert_eq!(repos[0], repos[1]);
        let (verdicts, _) = judge(&[measured], &config(tmp.path())).unwrap();
        assert_eq!(verdicts.len(), 1);
    }

    #[test]
    fn a_shared_cargo_target_dir_is_the_fix_and_is_not_flagged() {
        let tmp = TempDir::new().unwrap();
        let pad = session(tmp.path());
        checkout(&pad.join("tend"), "git@github.com:pleme-io/tend.git", 4096);
        std::fs::remove_dir_all(pad.join("tend/target")).unwrap();
        std::fs::create_dir_all(pad.join("tend-target/debug")).unwrap();
        std::fs::write(pad.join("tend-target/CACHEDIR.TAG"), TAG).unwrap();
        std::fs::create_dir_all(pad.join("tend-target/debug/.fingerprint")).unwrap();
        std::fs::write(
            pad.join("tend-target/.rustc_info.json"),
            "{\"rustc_fingerprint\":1}",
        )
        .unwrap();
        let Sight::Seen(s) = sense(&config(tmp.path()), at(1_000)).unwrap() else {
            panic!("seen")
        };
        assert_eq!(s.pads[0].targets.len(), 1);
        assert_eq!(s.pads[0].targets[0].repo, None);
        assert!(s.verdicts.is_empty(), "{:#?}", s.verdicts);
    }

    #[test]
    fn a_pad_past_the_threshold_is_oversize() {
        let pad = Pad {
            path: PathBuf::from("/p"),
            bytes: 21 * GIB,
            targets: Vec::new(),
        };
        let (verdicts, lines) = judge(&[pad], &ScratchpadConfig::default()).unwrap();
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0]["verdict"], "scratchpad-oversize");
        assert!(lines[0].contains("21.0 GiB"), "{lines:?}");
    }

    #[test]
    fn the_off_switch_measures_nothing_and_says_nothing() {
        let tmp = TempDir::new().unwrap();
        let pad = session(tmp.path());
        checkout(&pad.join("a"), "git@github.com:pleme-io/tend.git", 4096);
        checkout(&pad.join("b"), "git@github.com:pleme-io/tend.git", 4096);
        let off = ScratchpadConfig {
            enabled: false,
            ..config(tmp.path())
        };
        let snap = tmp.path().join("cache/scratchpad.json");
        assert_eq!(refresh(&off, &snap, at(1_000)).unwrap(), Sight::Off);
        assert!(!snap.exists());
        refresh(&config(tmp.path()), &snap, at(1_000)).unwrap();
        assert_eq!(
            session_lines(&config(tmp.path()), &snap, at(1_120)).len(),
            1
        );
        assert!(session_lines(&off, &snap, at(1_120)).is_empty());
        let dups_off = ScratchpadConfig {
            duplicate_targets: false,
            ..config(tmp.path())
        };
        let Sight::Seen(s) = sense(&dups_off, at(1_000)).unwrap() else {
            panic!("seen")
        };
        assert!(s.verdicts.is_empty());
    }

    #[test]
    fn the_session_line_carries_the_measurement_age() {
        let tmp = TempDir::new().unwrap();
        let pad = session(tmp.path());
        checkout(&pad.join("a"), "git@github.com:pleme-io/tend.git", 4096);
        checkout(&pad.join("b"), "git@github.com:pleme-io/tend.git", 4096);
        let snap = tmp.path().join("cache/scratchpad.json");
        refresh(&config(tmp.path()), &snap, at(1_000)).unwrap();
        let lines = session_lines(&config(tmp.path()), &snap, at(1_000 + 600));
        assert!(lines[0].ends_with("(measured 10m ago)"), "{lines:?}");
    }

    #[test]
    fn the_root_is_the_claude_dir_of_the_home_owner() {
        let tmp = TempDir::new().unwrap();
        use std::os::unix::fs::MetadataExt as _;
        let uid = std::fs::metadata(tmp.path()).unwrap().uid();
        assert_eq!(
            derived_root(None, Some(tmp.path())),
            Some(PathBuf::from(format!("/tmp/claude-{uid}")))
        );
        assert_eq!(
            derived_root(Some(PathBuf::from("/x")), Some(tmp.path())),
            Some(PathBuf::from(format!("/x/claude-{uid}")))
        );
    }
}
