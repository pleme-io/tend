use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::cargo_target::{
    guard, int, to_gib, walk_tree, Candidate, Mode, Policy, ProcessTable, ProcessUse, ProcessView,
    Sight, Verdict,
};

pub(crate) const POLICY_SOURCE: &str = include_str!("../blue/agent_scratch.b");
const SHOWN: &str = "agent_scratch.b";
pub(crate) const MARKER: &str = ".tend-scratch";
const MARKER_MAX_BYTES: u64 = 4096;
const SETTINGS_FILE: &str = "agent-scratch.b";
const SETTINGS_ENV: &str = "TEND_AGENT_SCRATCH_SETTINGS";
const HOUR: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub root: String,
    pub default_ttl_hours: u64,
    pub max_idle_hours: u64,
    pub active_minutes: u64,
    pub pressure_ttl_pct: u64,
    pub floor_hours: u64,
    pub watch_paths: Vec<String>,
    pub patterns: Vec<String>,
    pub debris_alert_gib: u64,
    pub interval_minutes: u64,
}

impl Settings {
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_minutes.saturating_mul(60))
    }

    fn policy(&self) -> Policy {
        Policy {
            max_idle: Duration::from_secs(self.max_idle_hours.saturating_mul(HOUR)),
            budget_bytes: u64::MAX,
            pressure_budget_bytes: u64::MAX,
            active_window: Duration::from_secs(self.active_minutes.saturating_mul(60)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Declared {
    pub settings: Settings,
    pub source: Option<PathBuf>,
    pub refusals: Vec<String>,
}

pub(crate) fn settings_path(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.or_else(|| std::env::var_os(SETTINGS_ENV).map(PathBuf::from))
        .or_else(|| {
            crate::xdg::resolver()
                .base(okiba::Tier::Config)
                .ok()
                .map(|b| b.join("tend").join(SETTINGS_FILE).into_path_buf())
                .filter(|p| p.is_file())
        })
}

pub(crate) fn declared(source: Option<&Path>) -> Result<Declared> {
    let overrides = match source {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
            overrides_of(Some(path), &text)?
        }
        None => json!({}),
    };
    declared_from(overrides, source)
}

fn overrides_of(path: Option<&Path>, text: &str) -> Result<Json> {
    let shown = path.map_or_else(|| SETTINGS_FILE.to_owned(), |p| p.display().to_string());
    let mut program = crate::blue_policy::load_data(text, &shown)?;
    if !program.defines("agent_scratch") {
        return Err(anyhow!(
            "{shown}: defines no `agent_scratch()`; it returns the fields laid over the defaults"
        ));
    }
    let value = crate::blue_policy::call_on(&mut program, &shown, "agent_scratch", &[])?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(anyhow!(
            "{shown}: agent_scratch() answered {value}, not a map"
        ))
    }
}

fn declared_from(overrides: Json, source: Option<&Path>) -> Result<Declared> {
    let out = crate::blue_policy::call(POLICY_SOURCE, SHOWN, "settings", &[overrides])?;
    let settings: Settings = serde_json::from_value(out["settings"].clone())
        .map_err(|e| anyhow!("{SHOWN}: settings() answered {}: {e}", out["settings"]))?;
    let refusals = out["refusals"]
        .as_array()
        .map(|rs| {
            rs.iter()
                .filter_map(Json::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(Declared {
        settings,
        source: source.map(Path::to_path_buf),
        refusals,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub owner: String,
    pub created_unix: u64,
    pub ttl_hours: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerRead {
    Absent,
    Bad(String),
    Valid(Marker),
}

fn plain(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " -_.:/@+,=".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[must_use]
pub fn render_marker(m: &Marker) -> String {
    let purpose = m
        .purpose
        .as_deref()
        .map(|p| format!(",\n    \"purpose\" => \"{}\"", plain(p)))
        .unwrap_or_default();
    format!(
        "def scratch()\n  {{\n    \"owner\" => \"{}\",\n    \"created_unix\" => {},\n    \"ttl_hours\" => {}{purpose}\n  }}\nend\n",
        plain(&m.owner),
        m.created_unix,
        m.ttl_hours
    )
}

pub(crate) fn read_marker(dir: &Path) -> MarkerRead {
    let path = dir.join(MARKER);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return MarkerRead::Absent;
    };
    if !meta.file_type().is_file() {
        return MarkerRead::Bad("the marker is not a regular file".into());
    }
    if meta.len() > MARKER_MAX_BYTES {
        return MarkerRead::Bad(format!("the marker is over {MARKER_MAX_BYTES} bytes"));
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return MarkerRead::Bad("the marker cannot be read".into());
    };
    match marker_of(&text) {
        Ok(m) => MarkerRead::Valid(m),
        Err(e) => MarkerRead::Bad(format!("{e:#}")),
    }
}

fn marker_of(text: &str) -> Result<Marker> {
    let mut program = crate::blue_policy::load_data(text, MARKER)?;
    if !program.defines("scratch") {
        return Err(anyhow!("defines no `scratch()`"));
    }
    let value = crate::blue_policy::call_on(&mut program, MARKER, "scratch", &[])?;
    let whys = crate::blue_policy::call(POLICY_SOURCE, SHOWN, "marker_whys", &[value.clone()])?;
    let whys: Vec<&str> = whys
        .as_array()
        .map(|w| w.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    if !whys.is_empty() {
        return Err(anyhow!("{}", whys.join("; ")));
    }
    serde_json::from_value(value).map_err(|e| anyhow!("{e}"))
}

fn own_uid() -> Option<u32> {
    std::fs::metadata(std::env::var_os("HOME")?)
        .ok()
        .map(|m| m.uid())
}

fn expand(text: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(text.trim()).into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum RootState {
    Seen,
    Missing,
    Refused { why: String },
}

pub(crate) fn managed_root(text: &str) -> Result<PathBuf, RootState> {
    let refused = |why: &str| RootState::Refused {
        why: why.to_owned(),
    };
    let path = expand(text);
    if !path.is_absolute() {
        return Err(refused("the root is not absolute after expanding ~"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(refused("the root has a .. component"));
    }
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return Err(RootState::Missing);
    };
    if meta.file_type().is_symlink() {
        return Err(refused("the root is a symlink"));
    }
    if !meta.is_dir() {
        return Err(refused("the root is not a directory"));
    }
    let canon = std::fs::canonicalize(&path).map_err(|_| RootState::Missing)?;
    if canon.parent().is_none() {
        return Err(refused("the root is /"));
    }
    if let Some(home) = dirs::home_dir().and_then(|h| std::fs::canonicalize(h).ok()) {
        if canon == home || home.starts_with(&canon) {
            return Err(refused("the root is the home or holds it"));
        }
        if let Ok(inside) = canon.strip_prefix(&home) {
            if crate::disk::USER_DATA_TREES
                .iter()
                .any(|d| inside.starts_with(d))
            {
                return Err(refused("the root is inside a user-data tree"));
            }
        }
    }
    if canon
        .ancestors()
        .any(|a| std::fs::symlink_metadata(a.join(".git")).is_ok())
    {
        return Err(refused("the root or a parent holds a .git"));
    }
    Ok(canon)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenView {
    Seen(Vec<PathBuf>),
    Blind(String),
}

pub trait OpenFiles {
    fn open(&self) -> OpenView;
}

pub struct SystemOpenFiles;

impl OpenFiles for SystemOpenFiles {
    #[cfg(target_os = "linux")]
    fn open(&self) -> OpenView {
        let Some(uid) = own_uid() else {
            return OpenView::Blind("own user id unreadable".into());
        };
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return OpenView::Blind("/proc unreadable".into());
        };
        let mut out = Vec::new();
        for entry in procs.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let pid = entry.path();
            match std::fs::metadata(&pid) {
                Ok(m) if m.uid() == uid => {}
                _ => continue,
            }
            out.extend(std::fs::read_link(pid.join("cwd")));
            let Ok(fds) = std::fs::read_dir(pid.join("fd")) else {
                if pid.exists() {
                    return OpenView::Blind(format!("{}: open files unreadable", pid.display()));
                }
                continue;
            };
            out.extend(
                fds.flatten()
                    .filter_map(|fd| std::fs::read_link(fd.path()).ok()),
            );
        }
        OpenView::Seen(out)
    }

    #[cfg(not(target_os = "linux"))]
    fn open(&self) -> OpenView {
        let Some(uid) = own_uid() else {
            return OpenView::Blind("own user id unreadable".into());
        };
        let out = std::process::Command::new("lsof")
            .args(["-w", "-n", "-P", "-Fn", "-u", &uid.to_string()])
            .output();
        match out {
            Ok(o) if !o.stdout.is_empty() => OpenView::Seen(
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| l.strip_prefix('n'))
                    .filter(|p| p.starts_with('/'))
                    .map(PathBuf::from)
                    .collect(),
            ),
            Ok(o) => OpenView::Blind(format!("lsof answered nothing ({})", o.status)),
            Err(e) => OpenView::Blind(format!("lsof: {e}")),
        }
    }
}

#[must_use]
pub fn holder_use(view: &ProcessView, open: &OpenView, dir: &Path) -> ProcessUse {
    let (ProcessView::Seen(procs), OpenView::Seen(files)) = (view, open) else {
        return ProcessUse::Unseen;
    };
    let Ok(dir) = std::fs::canonicalize(dir) else {
        return ProcessUse::Unseen;
    };
    let inside = |p: &PathBuf| p.starts_with(&dir);
    let held = files.iter().any(inside)
        || procs.iter().any(|p| {
            p.cwd.iter().any(inside)
                || p.target_dirs.iter().any(inside)
                || p.paths.iter().any(inside)
                || p.other_paths.iter().any(inside)
        });
    if held {
        ProcessUse::Used
    } else {
        ProcessUse::Free
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DebrisKind {
    Unmarked,
    BadMarker,
    MarkedOutsideRoot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Debris {
    pub path: PathBuf,
    pub kind: DebrisKind,
    pub why: String,
    pub bytes: u64,
    pub idle_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub path: PathBuf,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scratch {
    pub path: PathBuf,
    pub marker: Marker,
    pub bytes: u64,
    pub idle_secs: u64,
    pub age_secs: u64,
    pub process: ProcessUse,
    #[serde(flatten)]
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub scratch_count: u64,
    pub scratch_bytes: u64,
    pub oldest_age_secs: u64,
    pub reclaimed_count: u64,
    pub reclaimed_bytes: u64,
    pub failed: u64,
    pub debris_count: u64,
    pub debris_bytes: u64,
    pub oldest_debris_idle_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beat {
    pub verdict: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub at_unix: u64,
    pub mode: Mode,
    pub root: PathBuf,
    pub root_state: RootState,
    pub pressured: bool,
    pub processes: Sight,
    pub scratch: Vec<Scratch>,
    pub debris: Vec<Debris>,
    pub refused: Vec<Refused>,
    pub summary: Summary,
    pub beat: Beat,
    pub lines: Vec<String>,
    pub settings_source: Option<PathBuf>,
    pub settings_refusals: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Outcome {
    Off,
    Ran(Box<Run>),
}

struct Found {
    path: PathBuf,
    marker: Marker,
    candidate: Candidate,
}

fn secs_between(earlier: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(earlier).map_or(0, |d| d.as_secs())
}

fn measure(dir: &Path) -> Option<(u64, SystemTime)> {
    walk_tree(dir, &mut |_, _, _| {})
        .ok()
        .filter(|f| !f.unread)
        .map(|f| (f.bytes, f.newest))
}

fn debris_of(path: PathBuf, kind: DebrisKind, why: String, now: SystemTime) -> Debris {
    let (bytes, newest) =
        walk_tree(&path, &mut |_, _, _| {}).map_or((0, now), |f| (f.bytes, f.newest));
    Debris {
        path,
        kind,
        why,
        bytes,
        idle_secs: secs_between(newest, now),
    }
}

fn scan_root(
    root: &Path,
    uid: Option<u32>,
    now: SystemTime,
    found: &mut Vec<Found>,
    debris: &mut Vec<Debris>,
    refused: &mut Vec<Refused>,
) {
    let Ok(entries) = std::fs::read_dir(root) else {
        refused.push(Refused {
            path: root.to_path_buf(),
            why: "the root cannot be read".into(),
        });
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let refuse = |why: &str| Refused {
            path: path.clone(),
            why: why.to_owned(),
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            refused.push(refuse("a symlink; never followed out of the root"));
            continue;
        }
        if !meta.is_dir() {
            refused.push(refuse("not a directory"));
            continue;
        }
        if uid.is_some_and(|u| u != meta.uid()) {
            refused.push(refuse(&format!(
                "owned by uid {}, not this user",
                meta.uid()
            )));
            continue;
        }
        match read_marker(&path) {
            MarkerRead::Absent => {
                debris.push(debris_of(path, DebrisKind::Unmarked, String::new(), now));
            }
            MarkerRead::Bad(why) => {
                debris.push(debris_of(path, DebrisKind::BadMarker, why, now));
            }
            MarkerRead::Valid(marker) => match measure(&path) {
                Some((bytes, newest)) => found.push(Found {
                    path,
                    marker,
                    candidate: Candidate {
                        bytes,
                        newest,
                        lock_held: false,
                        process: ProcessUse::Unseen,
                    },
                }),
                None => refused.push(refuse("part of the tree cannot be read")),
            },
        }
    }
}

fn scan_watch(
    settings: &Settings,
    root: Option<&Path>,
    now: SystemTime,
    debris: &mut Vec<Debris>,
) -> Result<()> {
    for watch in &settings.watch_paths {
        let dir = expand(watch);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().to_str().map(ToOwned::to_owned))
            .collect();
        names.sort();
        let matched = crate::blue_policy::call(
            POLICY_SOURCE,
            SHOWN,
            "debris_names",
            &[json!(names), json!({"patterns": settings.patterns})],
        )?;
        for name in matched
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
        {
            let path = dir.join(name);
            if root.is_some_and(|r| std::fs::canonicalize(&path).is_ok_and(|c| c == r)) {
                continue;
            }
            let (kind, why) = match read_marker(&path) {
                MarkerRead::Absent => (DebrisKind::Unmarked, String::new()),
                MarkerRead::Bad(why) => (DebrisKind::BadMarker, why),
                MarkerRead::Valid(_) => (DebrisKind::MarkedOutsideRoot, String::new()),
            };
            debris.push(debris_of(path, kind, why, now));
        }
    }
    Ok(())
}

fn candidate_json(f: &Found, now: SystemTime) -> Json {
    json!({
        "bytes": int(f.candidate.bytes),
        "idle_secs": int(secs_between(f.candidate.newest, now)),
        "age_secs": int(now_unix(now).saturating_sub(f.marker.created_unix)),
        "ttl_secs": int(f.marker.ttl_hours.saturating_mul(HOUR)),
        "process": match f.candidate.process {
            ProcessUse::Free => "free",
            ProcessUse::Used => "used",
            ProcessUse::Unseen => "unseen",
        },
    })
}

fn now_unix(now: SystemTime) -> u64 {
    crate::heartbeat::unix(now)
}

fn settings_json(s: &Settings) -> Json {
    serde_json::to_value(s).unwrap_or_default()
}

fn decide(found: &[Found], s: &Settings, pressured: bool, now: SystemTime) -> Result<Vec<Verdict>> {
    let rows: Vec<Json> = found.iter().map(|f| candidate_json(f, now)).collect();
    let out = crate::blue_policy::call(
        POLICY_SOURCE,
        SHOWN,
        "verdicts",
        &[Json::Array(rows), settings_json(s), json!(pressured)],
    )?;
    let verdicts = out
        .as_array()
        .filter(|v| v.len() == found.len())
        .ok_or_else(|| anyhow!("{SHOWN}: verdicts() answered {out}"))?
        .iter()
        .map(|w| {
            w.as_str()
                .and_then(Verdict::from_word)
                .filter(|v| !matches!(v, Verdict::RemoveOverBudget))
                .ok_or_else(|| anyhow!("{SHOWN}: unknown verdict {w}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let cands: Vec<Candidate> = found.iter().map(|f| f.candidate).collect();
    guard(&cands, &verdicts, s.policy(), now)?;
    Ok(verdicts)
}

#[derive(Debug)]
pub enum RemoveError {
    Moved,
    Symlink,
    Foreign,
    MarkerChanged,
    Io(std::io::Error),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Moved => f.write_str("no longer directly inside the managed root"),
            Self::Symlink => f.write_str("became a symlink after the scan"),
            Self::Foreign => f.write_str("owned by another user"),
            Self::MarkerChanged => f.write_str("its marker changed after the scan"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

fn remove(root: &Path, dir: &Path, marker: &Marker, uid: Option<u32>) -> Result<(), RemoveError> {
    let meta = std::fs::symlink_metadata(dir).map_err(RemoveError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(RemoveError::Symlink);
    }
    let parent = dir
        .parent()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or(RemoveError::Moved)?;
    if parent != root || !meta.is_dir() {
        return Err(RemoveError::Moved);
    }
    if uid.is_some_and(|u| u != meta.uid()) {
        return Err(RemoveError::Foreign);
    }
    if read_marker(dir) != MarkerRead::Valid(marker.clone()) {
        return Err(RemoveError::MarkerChanged);
    }
    std::fs::remove_dir_all(dir).map_err(RemoveError::Io)
}

fn summarize(scratch: &[Scratch], debris: &[Debris], now: SystemTime) -> Summary {
    let kept: Vec<&Scratch> = scratch.iter().filter(|s| s.removed != Some(true)).collect();
    let gone: Vec<&Scratch> = scratch.iter().filter(|s| s.removed == Some(true)).collect();
    Summary {
        scratch_count: kept.len() as u64,
        scratch_bytes: kept.iter().map(|s| s.bytes).sum(),
        oldest_age_secs: kept
            .iter()
            .map(|s| now_unix(now).saturating_sub(s.marker.created_unix))
            .max()
            .unwrap_or(0),
        reclaimed_count: gone.len() as u64,
        reclaimed_bytes: gone.iter().map(|s| s.bytes).sum(),
        failed: scratch.iter().filter(|s| s.removed == Some(false)).count() as u64,
        debris_count: debris.len() as u64,
        debris_bytes: debris.iter().map(|d| d.bytes).sum(),
        oldest_debris_idle_secs: debris.iter().map(|d| d.idle_secs).max().unwrap_or(0),
    }
}

fn judged_lines(scratch: &[Scratch], debris: &[Debris]) -> Result<Vec<String>> {
    let reclaimed: Vec<Json> = scratch
        .iter()
        .filter(|s| s.removed == Some(true))
        .map(|s| {
            json!({
                "path": s.path.display().to_string(),
                "bytes": int(s.bytes),
                "verdict": s.verdict.label(),
                "owner": s.marker.owner,
            })
        })
        .collect();
    let failed: Vec<Json> = scratch
        .iter()
        .filter(|s| s.removed == Some(false))
        .map(|s| json!({"path": s.path.display().to_string(), "error": s.error}))
        .collect();
    let debris: Vec<Json> = debris
        .iter()
        .map(|d| {
            json!({
                "path": d.path.display().to_string(),
                "kind": d.kind,
                "why": d.why,
                "bytes": int(d.bytes),
                "idle_secs": int(d.idle_secs),
            })
        })
        .collect();
    let out = crate::blue_policy::call(
        POLICY_SOURCE,
        SHOWN,
        "lines",
        &[json!({"reclaimed": reclaimed, "failed": failed, "debris": debris})],
    )?;
    Ok(out
        .as_array()
        .map(|ls| {
            ls.iter()
                .filter_map(Json::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

fn judged_beat(summary: &Summary, s: &Settings) -> Result<Beat> {
    let facts = json!({
        "scratch_count": int(summary.scratch_count),
        "scratch_bytes": int(summary.scratch_bytes),
        "oldest_age_secs": int(summary.oldest_age_secs),
        "failed": int(summary.failed),
        "debris_count": int(summary.debris_count),
        "debris_bytes": int(summary.debris_bytes),
    });
    let out = crate::blue_policy::call(POLICY_SOURCE, SHOWN, "beat", &[facts, settings_json(s)])?;
    serde_json::from_value(out.clone()).map_err(|e| anyhow!("{SHOWN}: beat() answered {out}: {e}"))
}

pub struct Host<'a> {
    pub procs: &'a dyn ProcessTable,
    pub open: &'a dyn OpenFiles,
    pub pressured: &'a dyn Fn(&Path) -> bool,
}

pub(crate) fn system_pressured(root: &Path) -> bool {
    use crate::pressure::PressureReader as _;
    crate::pressure::SystemPressureReader {
        path: root.to_path_buf(),
    }
    .read()
    .is_ok_and(|r| crate::pressure::Thresholds::default().disk_pressured(&r, 1))
}

/// # Errors
/// The policy failed to load, answered in a shape tend does not read, or
/// planned to remove a directory in use; nothing is removed then.
pub fn run(declared: &Declared, mode: Mode, now: SystemTime, host: &Host<'_>) -> Result<Outcome> {
    let s = &declared.settings;
    if !s.enabled {
        return Ok(Outcome::Off);
    }
    let uid = own_uid();
    let (root, root_state) = match managed_root(&s.root) {
        Ok(r) => (Some(r), RootState::Seen),
        Err(state) => (None, state),
    };
    let mut found = Vec::new();
    let mut debris = Vec::new();
    let mut refused = Vec::new();
    if let Some(r) = &root {
        scan_root(r, uid, now, &mut found, &mut debris, &mut refused);
    }
    scan_watch(s, root.as_deref(), now, &mut debris)?;

    let (view, open) = if found.is_empty() {
        (ProcessView::Seen(Vec::new()), OpenView::Seen(Vec::new()))
    } else {
        (host.procs.view(), host.open.open())
    };
    for f in &mut found {
        f.candidate.process = holder_use(&view, &open, &f.path);
    }
    let pressured = root.as_deref().is_some_and(|r| (host.pressured)(r));
    let verdicts = decide(&found, s, pressured, now)?;

    let mut scratch = Vec::with_capacity(found.len());
    for (f, v) in found.into_iter().zip(verdicts) {
        let (removed, error) = match (&root, mode == Mode::Apply && v.removes()) {
            (Some(r), true) => match remove(r, &f.path, &f.marker, uid) {
                Ok(()) => (Some(true), None),
                Err(e) => (Some(false), Some(e.to_string())),
            },
            _ => (None, None),
        };
        scratch.push(Scratch {
            idle_secs: secs_between(f.candidate.newest, now),
            age_secs: now_unix(now).saturating_sub(f.marker.created_unix),
            path: f.path,
            marker: f.marker,
            bytes: f.candidate.bytes,
            process: f.candidate.process,
            verdict: v,
            removed,
            error,
        });
    }
    let summary = summarize(&scratch, &debris, now);
    let lines = judged_lines(&scratch, &debris)?;
    let beat = judged_beat(&summary, s)?;
    Ok(Outcome::Ran(Box::new(Run {
        at_unix: now_unix(now),
        mode,
        root: root.unwrap_or_else(|| expand(&s.root)),
        root_state,
        pressured,
        processes: match view {
            ProcessView::Seen(ps) => Sight::Seen {
                processes: ps.len(),
            },
            ProcessView::Blind(why) => Sight::Blind { why },
        },
        scratch,
        debris,
        refused,
        summary,
        beat,
        lines,
        settings_source: declared.source.clone(),
        settings_refusals: declared.refusals.clone(),
    })))
}

/// # Errors
/// The name is not one plain path segment, the root is not usable, or the
/// directory or its marker cannot be written.
pub fn create(settings: &Settings, name: &str, marker: &Marker) -> Result<PathBuf> {
    let plain_name = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b));
    if !plain_name {
        return Err(anyhow!(
            "{name:?} is not one plain segment of [A-Za-z0-9._-] that does not start with ."
        ));
    }
    let root = managed_root(&settings.root).map_err(|state| match state {
        RootState::Missing => anyhow!(
            "{} does not exist; on NixOS the tend agentScratch module declares it",
            settings.root
        ),
        RootState::Refused { why } => anyhow!("{}: {why}", settings.root),
        RootState::Seen => anyhow!("unreachable"),
    })?;
    let dir = root.join(name);
    std::fs::create_dir(&dir).map_err(|e| anyhow!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join(MARKER), render_marker(marker))?;
    Ok(dir)
}

pub(crate) fn snapshot_path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("daemon")
        .join("agent-scratch.json")
}

pub(crate) fn beat_path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("daemon")
        .join("agent-scratch-beat.json")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(OsStr::to_str).unwrap_or("out"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn beat_json(run: &Run, s: &Settings) -> Json {
    let m = &run.summary;
    json!({
        "at_unix_ms": int(run.at_unix.saturating_mul(1000)),
        "phase": "resolved",
        "outcome": match run.mode { Mode::Apply => "swept", Mode::DryRun => "reported" },
        "poll_seconds": int(s.interval_minutes.saturating_mul(60)),
        "verdict": run.beat.verdict,
        "detail": run.beat.detail,
        "scratch_count": int(m.scratch_count),
        "scratch_bytes": int(m.scratch_bytes),
        "oldest_age_secs": int(m.oldest_age_secs),
        "reclaimed_count": int(m.reclaimed_count),
        "reclaimed_bytes": int(m.reclaimed_bytes),
        "failed": int(m.failed),
        "debris_count": int(m.debris_count),
        "debris_bytes": int(m.debris_bytes),
        "oldest_debris_idle_secs": int(m.oldest_debris_idle_secs),
    })
}

#[must_use]
pub fn textfile(run: &Run) -> String {
    let m = &run.summary;
    let verdict = |v: &str| u8::from(run.beat.verdict == v);
    let rows: [(&str, &str, String); 11] = [
        (
            "tend_agent_scratch_bytes",
            "Bytes held by marked agent scratch dirs under the managed root.",
            m.scratch_bytes.to_string(),
        ),
        (
            "tend_agent_scratch_count",
            "Marked agent scratch dirs under the managed root.",
            m.scratch_count.to_string(),
        ),
        (
            "tend_agent_scratch_oldest_age_seconds",
            "Age of the oldest marked scratch dir, from its marker.",
            m.oldest_age_secs.to_string(),
        ),
        (
            "tend_agent_scratch_reclaimed_bytes",
            "Bytes reclaimed by the last run.",
            m.reclaimed_bytes.to_string(),
        ),
        (
            "tend_agent_scratch_reclaim_failures",
            "Reclaims that failed in the last run.",
            m.failed.to_string(),
        ),
        (
            "tend_agent_scratch_debris_bytes",
            "Bytes in undeclared agent scratch (reported, never deleted).",
            m.debris_bytes.to_string(),
        ),
        (
            "tend_agent_scratch_debris_count",
            "Undeclared agent scratch dirs.",
            m.debris_count.to_string(),
        ),
        (
            "tend_agent_scratch_debris_oldest_idle_seconds",
            "Idle time of the oldest undeclared scratch dir.",
            m.oldest_debris_idle_secs.to_string(),
        ),
        (
            "tend_agent_scratch_last_run_timestamp_seconds",
            "When the last run finished.",
            run.at_unix.to_string(),
        ),
        (
            "tend_agent_scratch_degraded",
            "1 when the run's verdict is degraded.",
            verdict("degraded").to_string(),
        ),
        (
            "tend_agent_scratch_failed",
            "1 when the run's verdict is failed.",
            verdict("failed").to_string(),
        ),
    ];
    rows.iter()
        .map(|(name, help, value)| {
            format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n")
        })
        .collect()
}

/// # Errors
/// A record could not be written.
pub fn record(run: &Run, s: &Settings, textfile_path: Option<&Path>) -> Result<()> {
    write_atomic(&snapshot_path(), &serde_json::to_vec(run)?)?;
    write_atomic(&beat_path(), &serde_json::to_vec(&beat_json(run, s))?)?;
    if let Some(p) = textfile_path {
        std::fs::write(p, textfile(run).as_bytes()).map_err(|e| anyhow!("{}: {e}", p.display()))?;
    }
    let log = crate::audit::AuditLog::default_path();
    for sc in run.scratch.iter().filter(|sc| sc.removed.is_some()) {
        log.log(
            "agent_scratch_reclaim",
            json!({
                "path": sc.path,
                "bytes": sc.bytes,
                "verdict": sc.verdict,
                "owner": sc.marker.owner,
                "removed": sc.removed,
                "error": sc.error,
            }),
        );
    }
    if !run.debris.is_empty() {
        log.log(
            "agent_scratch_debris",
            json!({"debris": run.debris, "bytes": run.summary.debris_bytes}),
        );
    }
    Ok(())
}

pub async fn watch(settings: Option<PathBuf>, textfile_path: Option<&Path>) {
    let source = settings_path(settings);
    loop {
        let wait = match declared(source.as_deref()) {
            Ok(d) => {
                let interval = d.settings.interval();
                let textfile_path = textfile_path.map(Path::to_path_buf);
                let swept = tokio::task::spawn_blocking(move || {
                    let host = Host {
                        procs: &crate::cargo_target::SystemProcessTable,
                        open: &SystemOpenFiles,
                        pressured: &system_pressured,
                    };
                    let outcome = run(&d, Mode::Apply, SystemTime::now(), &host)?;
                    if let Outcome::Ran(r) = &outcome {
                        record(r, &d.settings, textfile_path.as_deref())?;
                    }
                    anyhow::Ok(outcome)
                })
                .await;
                match swept {
                    Ok(Ok(Outcome::Ran(r))) => {
                        for line in &r.lines {
                            eprintln!("{line}");
                        }
                        eprintln!("agent-scratch {}: {}", r.beat.verdict, r.beat.detail);
                    }
                    Ok(Ok(Outcome::Off)) => eprintln!("agent-scratch off (enabled: false)"),
                    Ok(Err(e)) => eprintln!("agent-scratch run refused: {e:#}"),
                    Err(e) => eprintln!("agent-scratch run panicked: {e}"),
                }
                interval
            }
            Err(e) => {
                eprintln!("agent-scratch settings refused: {e:#}");
                Duration::from_secs(15 * 60)
            }
        };
        tokio::time::sleep(wait.max(Duration::from_secs(60))).await;
    }
}

pub fn print(outcome: &Outcome) {
    let Outcome::Ran(run) = outcome else {
        println!("agent-scratch off (enabled: false in agent-scratch.b)");
        return;
    };
    let root = match &run.root_state {
        RootState::Seen => format!("{}", run.root.display()),
        RootState::Missing => format!("{} (missing)", run.root.display()),
        RootState::Refused { why } => format!("{} (refused: {why})", run.root.display()),
    };
    println!(
        "agent-scratch root {root}{}",
        if run.pressured {
            "  [disk pressured]"
        } else {
            ""
        }
    );
    for why in &run.settings_refusals {
        println!("  ⚠️ setting refused: {why}");
    }
    for sc in &run.scratch {
        println!(
            "  {:>7.2} GiB  {}  owner {}  age {}h  idle {}h  {}",
            to_gib(sc.bytes),
            sc.path.display(),
            sc.marker.owner,
            sc.age_secs / HOUR,
            sc.idle_secs / HOUR,
            sc.verdict.label()
        );
    }
    for r in &run.refused {
        println!("  ➖ {}: {}", r.path.display(), r.why);
    }
    for line in &run.lines {
        println!("{line}");
    }
    let icon = match run.beat.verdict.as_str() {
        "ok" => "🟢",
        "degraded" => "🟡",
        _ => "🔴",
    };
    println!("verdict {icon} {}: {}", run.beat.verdict, run.beat.detail);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo_target::ProcessFacts;
    use tempfile::TempDir;

    struct Procs(ProcessView);
    impl ProcessTable for Procs {
        fn view(&self) -> ProcessView {
            self.0.clone()
        }
    }
    struct Open(OpenView);
    impl OpenFiles for Open {
        fn open(&self) -> OpenView {
            self.0.clone()
        }
    }

    fn now() -> SystemTime {
        static AT: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();
        *AT.get_or_init(SystemTime::now)
    }

    fn now_s() -> u64 {
        now_unix(now())
    }

    fn age_tree(dir: &Path, at: SystemTime) {
        let mut all = vec![dir.to_path_buf()];
        let mut i = 0;
        while i < all.len() {
            if all[i].is_dir() {
                for e in std::fs::read_dir(&all[i]).unwrap().flatten() {
                    all.push(e.path());
                }
            }
            i += 1;
        }
        for p in all.iter().rev() {
            std::fs::File::open(p).unwrap().set_modified(at).unwrap();
        }
    }

    fn settings(root: &Path, watch: &[&Path]) -> Settings {
        let mut s = declared(None).unwrap().settings;
        s.root = root.display().to_string();
        s.watch_paths = watch.iter().map(|p| p.display().to_string()).collect();
        s
    }

    fn decl(s: Settings) -> Declared {
        Declared {
            settings: s,
            source: None,
            refusals: Vec::new(),
        }
    }

    fn marked(root: &Path, name: &str, age_hours: u64, ttl_hours: u64) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("build")).unwrap();
        std::fs::write(dir.join("build/blob"), vec![7u8; 8192]).unwrap();
        let m = Marker {
            owner: "session-1".into(),
            created_unix: now_s() - age_hours * HOUR,
            ttl_hours,
            purpose: Some("repro".into()),
        };
        std::fs::write(dir.join(MARKER), render_marker(&m)).unwrap();
        age_tree(&dir, now() - Duration::from_secs(age_hours * HOUR));
        dir
    }

    fn quiet() -> (Procs, Open) {
        (
            Procs(ProcessView::Seen(Vec::new())),
            Open(OpenView::Seen(Vec::new())),
        )
    }

    fn apply(s: Settings, procs: &Procs, open: &Open, pressured: bool) -> Run {
        let p = move |_: &Path| pressured;
        let host = Host {
            procs,
            open,
            pressured: &p,
        };
        match run(&decl(s), Mode::Apply, now(), &host).unwrap() {
            Outcome::Ran(r) => *r,
            Outcome::Off => panic!("on"),
        }
    }

    #[test]
    fn an_expired_marked_dir_is_reclaimed() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let dir = marked(&root, "old", 10, 3);
        let (p, o) = quiet();
        let r = apply(settings(&root, &[]), &p, &o, false);
        assert!(!dir.exists(), "{r:#?}");
        assert_eq!(r.scratch[0].verdict, Verdict::RemoveExpired);
        assert_eq!(r.summary.reclaimed_count, 1);
        assert!(r.summary.reclaimed_bytes > 0);
        assert!(r.lines[0].contains("reclaimed"), "{:?}", r.lines);
        assert_eq!(r.beat.verdict, "ok");
    }

    #[test]
    fn a_dir_within_its_ttl_is_kept() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let dir = marked(&root, "fresh", 10, 24);
        let (p, o) = quiet();
        let r = apply(settings(&root, &[]), &p, &o, false);
        assert!(dir.exists());
        assert_eq!(r.scratch[0].verdict, Verdict::Keep);
        assert_eq!(r.summary.scratch_count, 1);
        assert_eq!(r.summary.oldest_age_secs, 10 * HOUR);
    }

    #[test]
    fn a_dir_a_process_holds_a_file_in_is_kept() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let dir = marked(&root, "held", 10, 3);
        let file = std::fs::canonicalize(dir.join("build/blob")).unwrap();
        let procs = Procs(ProcessView::Seen(Vec::new()));
        let open = Open(OpenView::Seen(vec![file]));
        let r = apply(settings(&root, &[]), &procs, &open, false);
        assert!(dir.exists());
        assert_eq!(
            r.scratch[0].verdict,
            Verdict::Active {
                signal: crate::cargo_target::ActiveSignal::ProcessUses
            }
        );
        let blind = Open(OpenView::Blind("unread".into()));
        let r = apply(settings(&root, &[]), &procs, &blind, false);
        assert!(dir.exists());
        assert_eq!(r.scratch[0].process, ProcessUse::Unseen);
    }

    #[test]
    fn a_live_open_file_in_the_dir_keeps_it() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let dir = marked(&root, "open", 10, 3);
        let held = std::fs::File::open(dir.join("build/blob")).unwrap();
        let p = |_: &Path| false;
        let host = Host {
            procs: &crate::cargo_target::SystemProcessTable,
            open: &SystemOpenFiles,
            pressured: &p,
        };
        let out = run(&decl(settings(&root, &[])), Mode::Apply, now(), &host).unwrap();
        drop(held);
        let Outcome::Ran(r) = out else { panic!("on") };
        assert!(dir.exists(), "{r:#?}");
        assert!(!r.scratch[0].verdict.removes());
        if matches!(r.processes, Sight::Seen { .. })
            && matches!(SystemOpenFiles.open(), OpenView::Seen(_))
        {
            assert_eq!(r.scratch[0].process, ProcessUse::Used, "{r:#?}");
        }
    }

    #[test]
    fn a_process_cwd_inside_the_dir_holds_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(dir.join("x")).unwrap();
        let canon = std::fs::canonicalize(dir.join("x")).unwrap();
        let view = ProcessView::Seen(vec![ProcessFacts {
            pid: 7,
            other_paths: vec![canon],
            ..ProcessFacts::default()
        }]);
        let open = OpenView::Seen(Vec::new());
        assert_eq!(holder_use(&view, &open, &dir), ProcessUse::Used);
        assert_eq!(
            holder_use(&ProcessView::Seen(Vec::new()), &open, &dir),
            ProcessUse::Free
        );
    }

    #[test]
    fn unmarked_or_outside_the_root_is_reported_never_deleted() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let watch = tmp.path().join("tmp");
        let bare = root.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        let unmarked = watch.join("cg-repro");
        std::fs::create_dir_all(&unmarked).unwrap();
        std::fs::write(unmarked.join("f"), vec![1u8; 4096]).unwrap();
        let outside = marked(&watch, "engenho-mountns-9", 99, 1);
        let unrelated = watch.join("notes");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::create_dir_all(root.join("bad")).unwrap();
        std::fs::write(
            root.join("bad").join(MARKER),
            "def scratch()\n  {\"owner\" => \"x\"}\nend\n",
        )
        .unwrap();
        let (p, o) = quiet();
        let r = apply(settings(&root, &[&watch]), &p, &o, true);
        assert!(bare.exists() && unmarked.exists() && outside.exists());
        assert!(root.join("bad").exists());
        let kinds: Vec<(String, DebrisKind)> = r
            .debris
            .iter()
            .map(|d| {
                (
                    d.path.file_name().unwrap().to_string_lossy().into_owned(),
                    d.kind,
                )
            })
            .collect();
        assert!(
            kinds.contains(&("bare".into(), DebrisKind::Unmarked)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("bad".into(), DebrisKind::BadMarker)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("cg-repro".into(), DebrisKind::Unmarked)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("engenho-mountns-9".into(), DebrisKind::MarkedOutsideRoot)),
            "{kinds:?}"
        );
        assert!(!kinds.iter().any(|(n, _)| n == "notes"));
        assert!(r.scratch.is_empty());
        assert!(r
            .lines
            .iter()
            .any(|l| l.contains("cg-repro") && l.contains("rm -rf --")));
    }

    #[test]
    fn a_symlink_out_of_the_root_is_refused_and_its_target_survives() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let elsewhere = marked(tmp.path(), "precious", 99, 1);
        std::os::unix::fs::symlink(&elsewhere, root.join("escape")).unwrap();
        let (p, o) = quiet();
        let r = apply(settings(&root, &[]), &p, &o, false);
        assert!(elsewhere.join("build/blob").exists());
        assert!(root.join("escape").exists());
        assert_eq!(r.refused.len(), 1, "{r:#?}");
        assert!(r.refused[0].why.contains("symlink"));
        assert!(r.scratch.is_empty());

        let linked_root = tmp.path().join("linked-root");
        std::os::unix::fs::symlink(&root, &linked_root).unwrap();
        assert_eq!(
            managed_root(&linked_root.display().to_string()),
            Err(RootState::Refused {
                why: "the root is a symlink".into()
            })
        );
    }

    #[test]
    fn pressure_shortens_the_ttl_but_never_below_the_floor() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let over = marked(&root, "seven-of-12", 7, 12);
        let under = marked(&root, "five-of-12", 5, 12);
        let (p, o) = quiet();
        let r = apply(settings(&root, &[]), &p, &o, true);
        assert!(r.pressured);
        assert!(
            !over.exists(),
            "12h cut to 3h, held at the 6h floor, so 7h old goes"
        );
        assert!(under.exists(), "the floor keeps a 5h old dir");
        let calm = marked(&root, "seven-calm", 7, 12);
        let r = apply(settings(&root, &[]), &p, &o, false);
        assert!(calm.exists());
        assert!(!r.pressured);
    }

    #[test]
    fn the_off_switch_measures_nothing_and_reclaims_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let dir = marked(&root, "old", 10, 3);
        let mut s = settings(&root, &[]);
        s.enabled = false;
        let (p, o) = quiet();
        let f = |_: &Path| false;
        let host = Host {
            procs: &p,
            open: &o,
            pressured: &f,
        };
        assert_eq!(
            run(&decl(s), Mode::Apply, now(), &host).unwrap(),
            Outcome::Off
        );
        assert!(dir.exists());
        let off = declared_from(json!({"enabled": false}), None).unwrap();
        assert!(!off.settings.enabled);
    }

    #[test]
    fn a_dry_run_reclaims_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let dir = marked(&root, "old", 10, 3);
        let (p, o) = quiet();
        let f = |_: &Path| false;
        let host = Host {
            procs: &p,
            open: &o,
            pressured: &f,
        };
        let Outcome::Ran(r) = run(&decl(settings(&root, &[])), Mode::DryRun, now(), &host).unwrap()
        else {
            panic!("on")
        };
        assert!(dir.exists());
        assert_eq!(r.scratch[0].verdict, Verdict::RemoveExpired);
        assert_eq!(r.scratch[0].removed, None);
    }

    #[test]
    fn the_node_settings_file_is_blue_and_a_bad_field_is_refused_alone() {
        let src = "def agent_scratch()\n  {\"floor_hours\" => 2, \"max_idle_hours\" => \"x\", \"nope\" => 1}\nend\n";
        let d = declared_from(overrides_of(None, src).unwrap(), None).unwrap();
        assert_eq!(d.settings.floor_hours, 2);
        assert_eq!(d.settings.max_idle_hours, 168);
        assert_eq!(d.refusals.len(), 2, "{:?}", d.refusals);
        let none = overrides_of(None, "def other()\n  1\nend\n").unwrap_err();
        assert!(none.to_string().contains("agent_scratch()"));
    }

    #[test]
    fn the_settings_file_the_nixos_module_renders_loads() {
        let rendered = "def agent_scratch()\n  {\n    \"enabled\" => true,\n    \"floor_hours\" => 6,\n    \"root\" => \"/var/tmp/agent-scratch\",\n    \"watch_paths\" => [\"/tmp\", \"/home/u\"]\n  }\nend\n";
        let d = declared_from(overrides_of(None, rendered).unwrap(), None).unwrap();
        assert!(d.refusals.is_empty(), "{:?}", d.refusals);
        assert_eq!(d.settings.watch_paths, vec!["/tmp", "/home/u"]);
    }

    #[test]
    fn the_marker_round_trips_and_a_hostile_one_is_bad_not_run() {
        let m = Marker {
            owner: "s\"#{x}\nend".into(),
            created_unix: 1,
            ttl_hours: 2,
            purpose: None,
        };
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(MARKER), render_marker(&m)).unwrap();
        let MarkerRead::Valid(read) = read_marker(tmp.path()) else {
            panic!("{:?}", read_marker(tmp.path()))
        };
        assert_eq!(read.owner, "s___x__end");
        std::fs::write(
            tmp.path().join(MARKER),
            "def scratch()\n  {\"owner\" => read_file(\"/etc/passwd\"), \"created_unix\" => 1, \"ttl_hours\" => 1}\nend\n",
        )
        .unwrap();
        assert!(matches!(read_marker(tmp.path()), MarkerRead::Bad(_)));
    }

    #[test]
    fn create_writes_a_marked_dir_the_reclaim_reads() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let s = settings(&root, &[]);
        let m = Marker {
            owner: "session-2".into(),
            created_unix: now_s(),
            ttl_hours: 4,
            purpose: Some("mount ns build".into()),
        };
        let dir = create(&s, "build-1", &m).unwrap();
        assert_eq!(read_marker(&dir), MarkerRead::Valid(m));
        assert!(create(
            &s,
            "../escape",
            &Marker {
                owner: "x".into(),
                created_unix: 1,
                ttl_hours: 1,
                purpose: None
            }
        )
        .is_err());
    }

    #[test]
    fn the_root_refuses_home_and_user_data() {
        let home = dirs::home_dir().unwrap();
        assert!(matches!(
            managed_root(&home.display().to_string()),
            Err(RootState::Refused { .. })
        ));
        assert_eq!(
            managed_root("relative/x"),
            Err(RootState::Refused {
                why: "the root is not absolute after expanding ~".into()
            })
        );
        assert_eq!(
            managed_root("/nonexistent-agent-scratch-root"),
            Err(RootState::Missing)
        );
    }

    #[test]
    fn the_textfile_and_beat_carry_bytes_and_oldest_age() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        marked(&root, "kept", 10, 24);
        let (p, o) = quiet();
        let r = apply(settings(&root, &[]), &p, &o, false);
        let s = settings(&root, &[]);
        let b = beat_json(&r, &s);
        assert_eq!(b["phase"], "resolved");
        assert_eq!(b["verdict"], "ok");
        assert_eq!(b["oldest_age_secs"], 10 * HOUR);
        assert_eq!(b["poll_seconds"], 900);
        let t = textfile(&r);
        assert!(t.contains(&format!(
            "tend_agent_scratch_oldest_age_seconds {}",
            10 * HOUR
        )));
        assert!(t.contains("tend_agent_scratch_count 1"));
    }
}
