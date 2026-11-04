use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::cargo_target::{
    self, age, candidate_row, guard, int, policy_row, to_gib, walk_tree, CargoTargetConfig,
    Candidate, Mode, Policy, ProcessTable, ProcessUse, ProcessView, Report, Verdict,
    DEFAULT_ACTIVE_MINUTES, DEFAULT_MAX_IDLE_DAYS, POLICY_SOURCE,
};

const GIB: u64 = 1024 * 1024 * 1024;
const SYSTEM_CACHE_ROOT: &str = "/private/var/cache";
const CACHEDIR_TAG: &str = "CACHEDIR.TAG";
const GIT_DIR: &str = ".git";
const USER_DATA_TREES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".config",
    ".claude",
    "Documents",
    "Desktop",
    "Downloads",
    "Pictures",
    "Movies",
    "Music",
    "Public",
    "code",
];
const AGGREGATE_ROOTS: &[&str] = &[
    ".cache",
    ".local",
    ".local/share",
    ".local/state",
    "Library",
    "Library/Caches",
    "Library/Application Support",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DiskConfig {
    pub enable: bool,
    pub free_goal_pct: u32,
    pub active_minutes: u64,
    pub dry_run: bool,
    pub caches: Vec<Json>,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            enable: false,
            free_goal_pct: 0,
            active_minutes: DEFAULT_ACTIVE_MINUTES,
            dry_run: false,
            caches: Vec::new(),
        }
    }
}

const fn default_max_idle_days() -> u32 {
    DEFAULT_MAX_IDLE_DAYS
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEntry {
    pub name: String,
    pub path: String,
    pub budget_gib: u64,
    #[serde(default = "default_max_idle_days")]
    pub max_idle_days: u32,
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub owners: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotCache {
    Malformed,
    EmptyName,
    DuplicateName,
    NotAbsolute,
    ParentComponent,
    Missing,
    Symlink,
    NotADirectory,
    OutsideAllowed,
    IsHome,
    UserData,
    HasGit,
}

impl NotCache {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Malformed => "entry does not match {name, path, budget_gib, max_idle_days, enable, owners}",
            Self::EmptyName => "name is empty",
            Self::DuplicateName => "another entry already has this name",
            Self::NotAbsolute => "path is not absolute after expanding ~",
            Self::ParentComponent => "path has a .. component",
            Self::Missing => "path does not exist",
            Self::Symlink => "path is a symlink",
            Self::NotADirectory => "path is not a directory",
            Self::OutsideAllowed => "path is outside the home and /private/var/cache",
            Self::IsHome => "path is the home or /private/var/cache itself",
            Self::UserData => "path holds user data or many tools' state, not one cache",
            Self::HasGit => "path or a parent below the home has a .git",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    home: Option<PathBuf>,
    system: PathBuf,
}

impl Allowed {
    #[must_use]
    pub fn host() -> Self {
        Self {
            home: dirs::home_dir().and_then(|h| std::fs::canonicalize(h).ok()),
            system: PathBuf::from(SYSTEM_CACHE_ROOT),
        }
    }

    fn root_of(&self, canon: &Path) -> Option<&Path> {
        self.home
            .as_deref()
            .into_iter()
            .chain(std::iter::once(self.system.as_path()))
            .find(|root| canon.starts_with(root))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDir {
    text: String,
    path: PathBuf,
    allowed: Allowed,
}

impl CacheDir {
    /// # Errors
    /// The first clause of the invariant the path fails.
    pub fn recognize(text: &str, allowed: &Allowed) -> Result<Self, NotCache> {
        let expanded = PathBuf::from(shellexpand::tilde(text.trim()).into_owned());
        if !expanded.is_absolute() {
            return Err(NotCache::NotAbsolute);
        }
        if expanded.components().any(|c| c == Component::ParentDir) {
            return Err(NotCache::ParentComponent);
        }
        let meta = std::fs::symlink_metadata(&expanded).map_err(|_| NotCache::Missing)?;
        if meta.file_type().is_symlink() {
            return Err(NotCache::Symlink);
        }
        if !meta.is_dir() {
            return Err(NotCache::NotADirectory);
        }
        let canon = std::fs::canonicalize(&expanded).map_err(|_| NotCache::Missing)?;
        let root = allowed.root_of(&canon).ok_or(NotCache::OutsideAllowed)?;
        if canon == root {
            return Err(NotCache::IsHome);
        }
        let inside = canon.strip_prefix(root).map_err(|_| NotCache::OutsideAllowed)?;
        let user_data = USER_DATA_TREES.iter().any(|d| inside.starts_with(d))
            || AGGREGATE_ROOTS.iter().any(|d| inside == Path::new(d));
        if user_data {
            return Err(NotCache::UserData);
        }
        let git_above = canon
            .ancestors()
            .take_while(|a| *a != root)
            .any(|a| std::fs::symlink_metadata(a.join(GIT_DIR)).is_ok());
        if git_above {
            return Err(NotCache::HasGit);
        }
        Ok(Self {
            text: text.to_owned(),
            path: canon,
            allowed: allowed.clone(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn children(&self) -> io::Result<Vec<CacheChild>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.path)?.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
                continue;
            }
            if entry.file_name() == CACHEDIR_TAG {
                continue;
            }
            out.push(CacheChild {
                root: self.clone(),
                path: entry.path(),
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheChild {
    root: CacheDir,
    path: PathBuf,
}

#[derive(Debug)]
pub enum CacheRemoveError {
    NoLongerRecognized(NotCache),
    NotInside,
    Symlink,
    HasGit,
    Unreadable(PathBuf),
    Io(io::Error),
}

impl std::fmt::Display for CacheRemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLongerRecognized(why) => write!(f, "no longer a cache dir: {}", why.reason()),
            Self::NotInside => f.write_str("no longer directly inside its cache dir"),
            Self::Symlink => f.write_str("became a symlink after the scan"),
            Self::HasGit => f.write_str("holds a .git"),
            Self::Unreadable(p) => write!(f, "{} cannot be read", p.display()),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// # Errors
/// See [`CacheRemoveError`]. Nothing is deleted on any error before the delete
/// starts.
pub fn remove_child(child: &CacheChild) -> Result<(), CacheRemoveError> {
    let root = CacheDir::recognize(&child.root.text, &child.root.allowed)
        .map_err(CacheRemoveError::NoLongerRecognized)?;
    if child.path.parent() != Some(root.path()) {
        return Err(CacheRemoveError::NotInside);
    }
    let meta = std::fs::symlink_metadata(&child.path).map_err(CacheRemoveError::Io)?;
    if meta.file_type().is_symlink() {
        return Err(CacheRemoveError::Symlink);
    }
    let dirs = if meta.is_dir() {
        real_dirs(&child.path)?
    } else {
        Vec::new()
    };
    make_writable(root.path()).map_err(CacheRemoveError::Io)?;
    for dir in &dirs {
        make_writable(dir).map_err(CacheRemoveError::Io)?;
    }
    if meta.is_dir() {
        std::fs::remove_dir_all(&child.path).map_err(CacheRemoveError::Io)
    } else {
        std::fs::remove_file(&child.path).map_err(CacheRemoveError::Io)
    }
}

fn real_dirs(root: &Path) -> Result<Vec<PathBuf>, CacheRemoveError> {
    let mut found = vec![root.to_path_buf()];
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|_| CacheRemoveError::Unreadable(dir.clone()))?;
        for entry in entries.flatten() {
            if entry.file_name() == GIT_DIR {
                return Err(CacheRemoveError::HasGit);
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                found.push(entry.path());
                stack.push(entry.path());
            }
        }
    }
    Ok(found)
}

const OWNER_RWX: u32 = 0o700;

fn make_writable(dir: &Path) -> io::Result<()> {
    let before = std::fs::symlink_metadata(dir)?;
    if !before.is_dir() || before.permissions().mode() & OWNER_RWX == OWNER_RWX {
        return Ok(());
    }
    let handle = File::open(dir)?;
    let opened = handle.metadata()?;
    if opened.dev() != before.dev() || opened.ino() != before.ino() {
        return Err(io::Error::other("directory changed during chmod"));
    }
    let mut perms = opened.permissions();
    perms.set_mode(perms.mode() | OWNER_RWX);
    handle.set_permissions(perms)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalState {
    Off,
    Refused,
    Blind,
    Met,
    Below,
    Pressure,
}

impl GoalState {
    fn from_word(w: &str) -> Option<Self> {
        match w {
            "off" => Some(Self::Off),
            "refused" => Some(Self::Refused),
            "blind" => Some(Self::Blind),
            "met" => Some(Self::Met),
            "below" => Some(Self::Below),
            "pressure" => Some(Self::Pressure),
            _ => None,
        }
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Refused => "refused",
            Self::Blind => "blind",
            Self::Met => "met",
            Self::Below => "below",
            Self::Pressure => "pressure",
        }
    }

    #[must_use]
    pub const fn tight(self) -> bool {
        matches!(self, Self::Below | Self::Pressure)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskReading {
    pub read: bool,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub pressured: bool,
}

#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
pub fn read_at(path: &Path, inflight: u32) -> DiskReading {
    let reader = crate::pressure::SystemPressureReader {
        path: path.to_path_buf(),
    };
    crate::pressure::PressureReader::read(&reader).map_or(
        DiskReading {
            read: false,
            free_bytes: 0,
            total_bytes: 0,
            pressured: false,
        },
        |r| DiskReading {
            read: true,
            free_bytes: (r.disk_free_gib * GIB as f64) as u64,
            total_bytes: (r.disk_total_gib * GIB as f64) as u64,
            pressured: crate::pressure::Thresholds::default().disk_pressured(&r, inflight),
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Goal {
    pub state: GoalState,
    pub goal_bytes: u64,
}

fn shown_bytes(v: &Json) -> Result<u64> {
    v.as_u64()
        .ok_or_else(|| anyhow!("cargo_target.b: expected a count, got {v}"))
}

/// # Errors
/// The policy failed to load or run, or answered a shape tend does not read.
pub fn assess(cfg: &DiskConfig, reading: &DiskReading) -> Result<Goal> {
    if !cfg.enable {
        return Ok(Goal {
            state: GoalState::Off,
            goal_bytes: 0,
        });
    }
    assess_free(cfg, reading, reading.free_bytes)
}

fn assess_free(cfg: &DiskConfig, reading: &DiskReading, free: u64) -> Result<Goal> {
    let out = crate::blue_policy::call(
        POLICY_SOURCE,
        "cargo_target.b",
        "goal_of",
        &[
            json!(reading.read),
            json!(int(free)),
            json!(int(reading.total_bytes)),
            json!(cfg.free_goal_pct),
            json!(reading.pressured),
        ],
    )?;
    let parts = out
        .as_array()
        .filter(|p| p.len() == 2)
        .ok_or_else(|| anyhow!("cargo_target.b: goal_of() answered {out}"))?;
    let state = parts[0]
        .as_str()
        .and_then(GoalState::from_word)
        .ok_or_else(|| anyhow!("cargo_target.b: unknown goal state {}", parts[0]))?;
    Ok(Goal {
        state,
        goal_bytes: shown_bytes(&parts[1])?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TriggerKind {
    Schedule,
    Pressure,
    Goal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    pub cargo_mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastSweep {
    pub elapsed: Duration,
    pub freed_bytes: u64,
}

/// # Errors
/// The policy failed to load or run, or answered a shape tend does not read.
pub fn trigger(
    cargo: &CargoTargetConfig,
    disk: &DiskConfig,
    reading: &DiskReading,
    last: Option<LastSweep>,
) -> Result<Option<Trigger>> {
    let since = |every: Duration| last.is_none_or(|l| l.elapsed >= every);
    if cargo.enable && since(cargo.interval()) {
        return Ok(Some(Trigger {
            kind: TriggerKind::Schedule,
            cargo_mode: Mode::Apply,
        }));
    }
    if cargo.auto_apply && reading.pressured && since(cargo.pressure_interval()) {
        return Ok(Some(Trigger {
            kind: TriggerKind::Pressure,
            cargo_mode: Mode::Apply,
        }));
    }
    let goal = assess(disk, reading)?;
    if goal.state != GoalState::Below {
        return Ok(None);
    }
    let (freed, elapsed) = last.map_or((-1, i64::MAX), |l| {
        (int(l.freed_bytes), int(l.elapsed.as_secs()))
    });
    let due = crate::blue_policy::call(
        POLICY_SOURCE,
        "cargo_target.b",
        "goal_sweep_due",
        &[
            json!(goal.state.word()),
            json!(freed),
            json!(elapsed),
            json!(int(cargo.pressure_interval().as_secs())),
            json!(int(cargo.interval().as_secs())),
        ],
    )?;
    let due = due
        .as_bool()
        .ok_or_else(|| anyhow!("cargo_target.b: goal_sweep_due() answered {due}"))?;
    Ok(due.then_some(Trigger {
        kind: TriggerKind::Goal,
        cargo_mode: if cargo.auto_apply {
            Mode::Apply
        } else {
            Mode::DryRun
        },
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryStatus {
    Open,
    OwnerLive,
    Blind,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildRow {
    pub path: String,
    pub bytes: u64,
    pub gib: f64,
    pub idle_days: f64,
    #[serde(flatten)]
    pub verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheReport {
    pub name: String,
    pub path: String,
    pub status: EntryStatus,
    pub budget_gib: u64,
    pub children: usize,
    pub bytes: u64,
    pub gib: f64,
    pub reclaim_gib: f64,
    pub freed_gib: f64,
    pub protected: Vec<String>,
    pub removing: Vec<ChildRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefusedCache {
    pub name: String,
    pub path: String,
    pub reason: NotCache,
    pub why: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskReport {
    pub state: GoalState,
    pub mode: Mode,
    pub free_goal_pct: u32,
    pub free_gib: f64,
    pub total_gib: f64,
    pub goal_gib: f64,
    pub reclaim_gib: f64,
    pub freed_gib: f64,
    pub caches: Vec<CacheReport>,
    pub off: Vec<String>,
    pub refused: Vec<RefusedCache>,
}

fn slot_entries(cfg: &DiskConfig) -> (Vec<CacheEntry>, Vec<RefusedCache>) {
    let mut entries: Vec<CacheEntry> = Vec::new();
    let mut refused = Vec::new();
    let mut names = HashSet::new();
    for raw in &cfg.caches {
        let shown = |key: &str| {
            raw.get(key)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let mut refuse = |reason: NotCache| {
            refused.push(RefusedCache {
                name: shown("name"),
                path: shown("path"),
                reason,
                why: reason.reason(),
            });
        };
        match serde_json::from_value::<CacheEntry>(raw.clone()) {
            Err(_) => refuse(NotCache::Malformed),
            Ok(e) if e.name.trim().is_empty() => refuse(NotCache::EmptyName),
            Ok(e) if !names.insert(e.name.clone()) => refuse(NotCache::DuplicateName),
            Ok(e) => entries.push(e),
        }
    }
    (entries, refused)
}

struct Open {
    entry: CacheEntry,
    status: EntryStatus,
    children: Vec<(CacheChild, Candidate)>,
    protected: Vec<(CacheChild, u64)>,
}

fn owner_status(view: &ProcessView, dir: &Path, owners: &[String]) -> EntryStatus {
    let ProcessView::Seen(procs) = view else {
        return EntryStatus::Blind;
    };
    let live = procs.iter().any(|p| {
        p.names.iter().any(|n| owners.contains(n))
            || p.paths
                .iter()
                .chain(p.other_paths.iter())
                .any(|q| q.starts_with(dir))
    });
    if live {
        EntryStatus::OwnerLive
    } else {
        EntryStatus::Open
    }
}

fn open_entry(entry: CacheEntry, dir: &CacheDir, view: &ProcessView) -> io::Result<Open> {
    let status = owner_status(view, dir.path(), &entry.owners);
    let process = match status {
        EntryStatus::Open => ProcessUse::Free,
        EntryStatus::OwnerLive => ProcessUse::Used,
        EntryStatus::Blind => ProcessUse::Unseen,
    };
    let mut children = Vec::new();
    let mut protected = Vec::new();
    for child in dir.children()? {
        let mut git = child.path.file_name() == Some(std::ffi::OsStr::new(GIT_DIR));
        let facts = walk_tree(&child.path, &mut |path, _, _| {
            git = git || path.file_name() == Some(std::ffi::OsStr::new(GIT_DIR));
        })?;
        if git || facts.unread {
            protected.push((child, facts.bytes));
        } else {
            children.push((
                child,
                Candidate {
                    bytes: facts.bytes,
                    newest: facts.newest,
                    lock_held: false,
                    process,
                },
            ));
        }
    }
    Ok(Open {
        entry,
        status,
        children,
        protected,
    })
}

fn policy_of(cfg: &DiskConfig, open: &Open) -> Policy {
    let protected: u64 = open.protected.iter().map(|(_, b)| *b).sum();
    let budget = open
        .entry
        .budget_gib
        .saturating_mul(GIB)
        .saturating_sub(protected);
    Policy {
        max_idle: Duration::from_secs(u64::from(open.entry.max_idle_days) * 86_400),
        budget_bytes: budget,
        pressure_budget_bytes: budget,
        active_window: Duration::from_secs(cfg.active_minutes.saturating_mul(60)),
    }
}

fn plan(
    cfg: &DiskConfig,
    opens: &[Open],
    goal: Goal,
    free: u64,
    now: SystemTime,
) -> Result<Vec<Vec<Verdict>>> {
    let rows: Vec<Json> = opens
        .iter()
        .map(|o| {
            let cands: Vec<Json> = o
                .children
                .iter()
                .map(|(_, c)| candidate_row(c, now))
                .collect();
            json!([cands, policy_row(policy_of(cfg, o))])
        })
        .collect();
    let out = crate::blue_policy::call(
        POLICY_SOURCE,
        "cargo_target.b",
        "plan_caches",
        &[
            Json::Array(rows),
            json!(goal.state.word()),
            json!(int(free)),
            json!(int(goal.goal_bytes)),
        ],
    )?;
    let answered = out
        .as_array()
        .filter(|a| a.len() == opens.len())
        .ok_or_else(|| anyhow!("cargo_target.b: plan_caches() answered {out}"))?;
    let mut plans = Vec::with_capacity(opens.len());
    for (open, verdicts) in opens.iter().zip(answered) {
        let verdicts = verdicts
            .as_array()
            .filter(|v| v.len() == open.children.len())
            .ok_or_else(|| anyhow!("cargo_target.b: one verdict per child, got {verdicts}"))?
            .iter()
            .map(|w| {
                w.as_str()
                    .and_then(Verdict::from_word)
                    .ok_or_else(|| anyhow!("cargo_target.b: unknown verdict {w}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let cands: Vec<Candidate> = open.children.iter().map(|(_, c)| *c).collect();
        guard(&cands, &verdicts, policy_of(cfg, open), now)?;
        plans.push(verdicts);
    }
    Ok(plans)
}

fn cache_mode(cfg: &DiskConfig, mode: Mode) -> Mode {
    if cfg.dry_run {
        Mode::DryRun
    } else {
        mode
    }
}

/// # Errors
/// Only the policy's; nothing is removed then.
pub fn sweep_caches(
    cfg: &DiskConfig,
    reading: &DiskReading,
    free: u64,
    mode: Mode,
    now: SystemTime,
    procs: &dyn ProcessTable,
    allowed: &Allowed,
) -> Result<DiskReport> {
    let mode = cache_mode(cfg, mode);
    let goal = if cfg.enable {
        assess_free(cfg, reading, free)?
    } else {
        Goal {
            state: GoalState::Off,
            goal_bytes: 0,
        }
    };
    let (entries, mut refused) = slot_entries(cfg);
    let mut off = Vec::new();
    let mut opens = Vec::new();
    let view = if cfg.enable && entries.iter().any(|e| e.enable) {
        Some(procs.view())
    } else {
        None
    };
    for entry in entries {
        let Some(view) = view.as_ref().filter(|_| entry.enable) else {
            off.push(entry.name);
            continue;
        };
        let mut refuse = |e: &CacheEntry, reason: NotCache| {
            refused.push(RefusedCache {
                name: e.name.clone(),
                path: e.path.clone(),
                reason,
                why: reason.reason(),
            });
        };
        let dir = match CacheDir::recognize(&entry.path, allowed) {
            Ok(d) => d,
            Err(why) => {
                refuse(&entry, why);
                continue;
            }
        };
        match open_entry(entry.clone(), &dir, view) {
            Ok(open) => opens.push(open),
            Err(_) => refuse(&entry, NotCache::Missing),
        }
    }
    refused.sort_by(|a, b| a.name.cmp(&b.name));

    let plans = plan(cfg, &opens, goal, free, now)?;

    let mut reclaim_total = 0;
    let mut freed_total = 0;
    let mut caches = Vec::with_capacity(opens.len());
    for (open, verdicts) in opens.iter().zip(&plans) {
        let mut removing = Vec::new();
        let mut reclaim = 0;
        let mut freed = 0;
        for ((child, cand), verdict) in open.children.iter().zip(verdicts) {
            if verdict.removes() {
                reclaim += cand.bytes;
            }
            let (removed, error) = if verdict.removes() && mode == Mode::Apply {
                match remove_child(child) {
                    Ok(()) => {
                        freed += cand.bytes;
                        (Some(true), None)
                    }
                    Err(e) => (Some(false), Some(e.to_string())),
                }
            } else {
                (None, None)
            };
            if verdict.removes() {
                removing.push(ChildRow {
                    path: child.path.display().to_string(),
                    bytes: cand.bytes,
                    gib: to_gib(cand.bytes),
                    idle_days: (age(cand, now).as_secs_f64() / 8_640.0).round() / 10.0,
                    verdict: *verdict,
                    removed,
                    error,
                });
            }
        }
        reclaim_total += reclaim;
        freed_total += freed;
        let kept: u64 = open.children.iter().map(|(_, c)| c.bytes).sum();
        let protected_bytes: u64 = open.protected.iter().map(|(_, b)| *b).sum();
        let bytes = kept + protected_bytes;
        caches.push(CacheReport {
            name: open.entry.name.clone(),
            path: open.entry.path.clone(),
            status: open.status,
            budget_gib: open.entry.budget_gib,
            children: open.children.len() + open.protected.len(),
            bytes,
            gib: to_gib(bytes),
            reclaim_gib: to_gib(reclaim),
            freed_gib: to_gib(freed),
            protected: open
                .protected
                .iter()
                .map(|(c, _)| c.path.display().to_string())
                .collect(),
            removing,
        });
    }
    Ok(DiskReport {
        state: goal.state,
        mode,
        free_goal_pct: cfg.free_goal_pct,
        free_gib: to_gib(free),
        total_gib: to_gib(reading.total_bytes),
        goal_gib: to_gib(goal.goal_bytes),
        reclaim_gib: to_gib(reclaim_total),
        freed_gib: to_gib(freed_total),
        caches,
        off,
        refused,
    })
}

/// # Errors
/// Only the policy's; nothing is removed then.
#[allow(clippy::too_many_arguments)]
pub fn cycle(
    cargo: &CargoTargetConfig,
    disk: &DiskConfig,
    repos: &[PathBuf],
    reading: &DiskReading,
    cargo_mode: Mode,
    mode: Mode,
    now: SystemTime,
    procs: &dyn ProcessTable,
    allowed: &Allowed,
) -> Result<Report> {
    let goal = assess(disk, reading)?;
    let tight = reading.pressured || goal.state.tight();
    let mut report = cargo_target::sweep(cargo, repos, tight, cargo_mode, now, procs)?;
    if disk.enable || !disk.caches.is_empty() {
        let cargo_frees: u64 = report
            .entries
            .iter()
            .filter(|e| e.verdict.removes() && e.removed != Some(false))
            .map(|e| e.bytes)
            .sum();
        let free = reading.free_bytes.saturating_add(cargo_frees);
        report.disk = Some(sweep_caches(disk, reading, free, mode, now, procs, allowed)?);
    }
    Ok(report)
}

#[must_use]
pub fn freed_bytes(report: &Report) -> u64 {
    let cargo: u64 = report
        .entries
        .iter()
        .filter(|e| e.removed == Some(true))
        .map(|e| e.bytes)
        .sum();
    let caches: u64 = report.disk.iter().flat_map(|d| d.caches.iter()).map(|c| {
        c.removing
            .iter()
            .filter(|r| r.removed == Some(true))
            .map(|r| r.bytes)
            .sum::<u64>()
    }).sum();
    cargo + caches
}

#[must_use]
pub fn failed_removal(report: &Report) -> bool {
    report.entries.iter().any(|e| e.removed == Some(false))
        || report.disk.as_ref().is_some_and(|d| {
            d.caches
                .iter()
                .any(|c| c.removing.iter().any(|r| r.removed == Some(false)))
        })
}

pub fn print_report(r: &DiskReport) {
    println!(
        "disk          {:.1} GiB free of {:.1} GiB; goal {}% = {:.1} GiB; state {}",
        r.free_gib,
        r.total_gib,
        r.free_goal_pct,
        r.goal_gib,
        r.state.word()
    );
    for c in &r.caches {
        let status = match c.status {
            EntryStatus::Open => "",
            EntryStatus::OwnerLive => "  [owner live, skipped]",
            EntryStatus::Blind => "  [processes unseen, skipped]",
        };
        println!(
            "  cache {:<20} {:>8.2} GiB  budget {} GiB  reclaim {:.2} GiB  freed {:.2} GiB  {}{}",
            c.name, c.gib, c.budget_gib, c.reclaim_gib, c.freed_gib, c.path, status
        );
        for row in &c.removing {
            let outcome = match (row.removed, &row.error) {
                (Some(true), _) => "  [removed]".to_string(),
                (Some(false), Some(err)) => "  [FAILED: ".to_string() + err + "]",
                _ => String::new(),
            };
            println!(
                "    {:>8.2}  {:>9.1}  {:<28}  {}{}",
                row.gib,
                row.idle_days,
                row.verdict.label(),
                row.path,
                outcome
            );
        }
        for path in &c.protected {
            println!("    protected {path}  (holds a .git or cannot be read)");
        }
    }
    for name in &r.off {
        println!("  cache {name}  off");
    }
    for x in &r.refused {
        println!("  cache {} refused  {}  ({})", x.name, x.path, x.why);
    }
    match r.mode {
        Mode::DryRun => println!("caches        dry run: would free {:.1} GiB", r.reclaim_gib),
        Mode::Apply => println!("caches        applied: freed {:.1} GiB", r.freed_gib),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo_target::{ActiveSignal, ProcessFacts};
    use tempfile::TempDir;

    const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
        # This file is a cache directory tag created by cargo.\n";

    struct Fixed(ProcessView);

    impl ProcessTable for Fixed {
        fn view(&self) -> ProcessView {
            self.0.clone()
        }
    }

    struct Unasked;

    impl ProcessTable for Unasked {
        fn view(&self) -> ProcessView {
            panic!("a disabled disk block must not read the process table")
        }
    }

    fn nobody() -> Fixed {
        Fixed(ProcessView::Seen(Vec::new()))
    }

    fn home() -> (TempDir, PathBuf, Allowed) {
        let tmp = TempDir::new().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap();
        let allowed = Allowed {
            home: Some(home.clone()),
            system: PathBuf::from("/nonexistent-system-cache"),
        };
        (tmp, home, allowed)
    }

    fn at(path: &Path) -> String {
        path.display().to_string()
    }

    fn entry(name: &str, path: &Path, budget: u64) -> Json {
        json!({"name": name, "path": at(path), "budget_gib": budget, "enable": true})
    }

    fn disk(pct: u32, caches: Vec<Json>) -> DiskConfig {
        DiskConfig {
            enable: true,
            free_goal_pct: pct,
            active_minutes: 0,
            dry_run: false,
            caches,
        }
    }

    fn below() -> DiskReading {
        DiskReading {
            read: true,
            free_bytes: 0,
            total_bytes: 1 << 40,
            pressured: false,
        }
    }

    fn met() -> DiskReading {
        DiskReading {
            free_bytes: 1 << 40,
            ..below()
        }
    }

    fn file(dir: &Path, name: &str, days_old: u64) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, vec![7u8; 8192]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(days_old * 86_400);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
        path
    }

    fn run(cfg: &DiskConfig, reading: &DiskReading, mode: Mode, allowed: &Allowed) -> DiskReport {
        sweep_caches(
            cfg,
            reading,
            reading.free_bytes,
            mode,
            SystemTime::now(),
            &nobody(),
            allowed,
        )
        .unwrap()
    }

    #[test]
    fn every_bad_path_is_refused_by_its_own_clause() {
        let (_tmp, home, allowed) = home();
        let elsewhere = TempDir::new().unwrap();
        let outside = std::fs::canonicalize(elsewhere.path()).unwrap();
        std::fs::create_dir_all(home.join("repo/.git")).unwrap();
        std::fs::create_dir_all(home.join("repo/cache")).unwrap();
        std::fs::create_dir_all(home.join("direct/.git")).unwrap();
        std::fs::create_dir_all(home.join("real")).unwrap();
        std::os::unix::fs::symlink(home.join("real"), home.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join("escape")).unwrap();
        file(&home, "plain", 0);
        std::fs::create_dir_all(home.join(".ssh/keys")).unwrap();
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        std::fs::create_dir_all(home.join("Library/Caches/tool")).unwrap();
        std::fs::create_dir_all(home.join(".cache")).unwrap();

        let table: Vec<(String, NotCache)> = vec![
            ("relative/cache".into(), NotCache::NotAbsolute),
            (at(&home.join("real/../real")), NotCache::ParentComponent),
            (at(&home.join("missing")), NotCache::Missing),
            (at(&home.join("link")), NotCache::Symlink),
            (at(&home.join("plain")), NotCache::NotADirectory),
            (at(&outside), NotCache::OutsideAllowed),
            ("/".into(), NotCache::OutsideAllowed),
            (at(home.parent().unwrap()), NotCache::OutsideAllowed),
            (at(&home), NotCache::IsHome),
            (at(&home.join(".ssh/keys")), NotCache::UserData),
            (at(&home.join("Documents")), NotCache::UserData),
            (at(&home.join("Library/Caches")), NotCache::UserData),
            (at(&home.join(".cache")), NotCache::UserData),
            (at(&home.join("direct")), NotCache::HasGit),
            (at(&home.join("repo/cache")), NotCache::HasGit),
        ];
        for (path, why) in table {
            assert_eq!(
                CacheDir::recognize(&path, &allowed).map(|_| ()),
                Err(why),
                "{path}"
            );
        }
        let through = home.join("escape");
        assert_eq!(
            CacheDir::recognize(&at(&through.join(".")), &allowed).map(|_| ()),
            Err(NotCache::OutsideAllowed),
            "a symlinked parent cannot walk out of the home"
        );
        assert!(CacheDir::recognize(&at(&home.join("Library/Caches/tool")), &allowed).is_ok());
    }

    #[test]
    fn a_host_allows_only_its_home_and_the_system_cache() {
        let allowed = Allowed::host();
        assert_eq!(allowed.system, PathBuf::from("/private/var/cache"));
        assert_eq!(
            CacheDir::recognize("/usr", &allowed).map(|_| ()),
            Err(NotCache::OutsideAllowed)
        );
        assert_eq!(
            CacheDir::recognize("~", &allowed).map(|_| ()),
            Err(NotCache::IsHome)
        );
    }

    #[test]
    fn a_refused_entry_leaves_its_siblings_working() {
        let (_tmp, home, allowed) = home();
        let good = home.join("caches/good");
        let old = file(&good, "old", 90);
        let off_dir = home.join("caches/off");
        let off_file = file(&off_dir, "old", 90);
        let elsewhere = TempDir::new().unwrap();
        let foreign = file(elsewhere.path(), "old", 90);
        let cfg = disk(
            50,
            vec![
                entry("outside", elsewhere.path(), 0),
                json!({"name": "typo", "path": at(&good), "budget_gib": "many", "enable": true}),
                json!({"name": "unknown-key", "path": at(&good), "budget_gib": 0, "enabel": true}),
                json!({"name": "off", "path": at(&off_dir), "budget_gib": 0}),
                entry("  ", &good, 0),
                entry("good", &good, 0),
                entry("good", &off_dir, 0),
            ],
        );
        let r = run(&cfg, &below(), Mode::Apply, &allowed);
        assert!(!old.exists());
        assert!(off_file.exists());
        assert!(foreign.exists());
        assert_eq!(r.caches.len(), 1);
        assert_eq!(r.caches[0].name, "good");
        assert_eq!(r.off, vec!["off".to_string()]);
        let mut reasons: Vec<NotCache> = r.refused.iter().map(|x| x.reason).collect();
        reasons.sort();
        assert_eq!(
            reasons,
            vec![
                NotCache::Malformed,
                NotCache::Malformed,
                NotCache::EmptyName,
                NotCache::DuplicateName,
                NotCache::OutsideAllowed,
            ]
        );
    }

    #[test]
    fn a_disabled_block_reads_nothing_and_removes_nothing() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let old = file(&dir, "old", 90);
        let cfg = DiskConfig {
            enable: false,
            ..disk(50, vec![entry("x", &dir, 0)])
        };
        let r = sweep_caches(
            &cfg,
            &below(),
            0,
            Mode::Apply,
            SystemTime::now(),
            &Unasked,
            &allowed,
        )
        .unwrap();
        assert_eq!(r.state, GoalState::Off);
        assert_eq!(r.off, vec!["x".to_string()]);
        assert!(old.exists());
    }

    #[test]
    fn eviction_runs_only_below_the_goal() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let old = file(&dir, "old", 90);
        let cfg = disk(50, vec![entry("x", &dir, 0)]);

        let r = run(&cfg, &met(), Mode::Apply, &allowed);
        assert_eq!(r.state, GoalState::Met);
        assert!(old.exists(), "a comfortable disk evicts nothing");

        let r = run(&cfg, &below(), Mode::Apply, &allowed);
        assert_eq!(r.state, GoalState::Below);
        assert!(!old.exists());
        assert!(r.freed_gib > 0.0);
    }

    #[test]
    fn eviction_is_oldest_first_and_stops_at_the_goal() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let newest = file(&dir, "a-newest", 20);
        let oldest = file(&dir, "b-oldest", 40);
        let middle = file(&dir, "c-middle", 30);
        let total = 1_000_000;
        let reading = DiskReading {
            read: true,
            free_bytes: 500_000 - 1,
            total_bytes: total,
            pressured: false,
        };
        let cfg = disk(50, vec![entry("x", &dir, 0)]);
        let r = run(&cfg, &reading, Mode::Apply, &allowed);
        assert!(!oldest.exists());
        assert!(middle.exists(), "one eviction reached the goal");
        assert!(newest.exists());
        assert_eq!(r.caches[0].removing.len(), 1);
    }

    #[test]
    fn dry_run_reports_what_it_would_remove_and_removes_nothing() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let old = file(&dir, "old", 90);
        let cfg = DiskConfig {
            dry_run: true,
            ..disk(50, vec![entry("x", &dir, 0)])
        };
        let r = run(&cfg, &below(), Mode::Apply, &allowed);
        assert_eq!(r.mode, Mode::DryRun);
        assert!(old.exists());
        assert!(r.reclaim_gib > 0.0);
        assert_eq!(r.freed_gib, 0.0);
        assert_eq!(r.caches[0].removing[0].verdict, Verdict::RemoveIdle);
        assert_eq!(r.caches[0].removing[0].removed, None);

        let plain = disk(50, vec![entry("x", &dir, 0)]);
        let r = run(&plain, &below(), Mode::DryRun, &allowed);
        assert_eq!(r.mode, Mode::DryRun);
        assert!(old.exists());
    }

    #[test]
    fn a_live_owner_skips_the_entry_and_no_sibling() {
        let (_tmp, home, allowed) = home();
        let named = home.join("caches/named");
        let named_file = file(&named, "old", 90);
        let by_exe = home.join("caches/by-exe");
        let by_exe_file = file(&by_exe, "old", 90);
        let by_cwd = home.join("caches/by-cwd");
        let by_cwd_file = file(&by_cwd, "old", 90);
        let free = home.join("caches/free");
        let free_file = file(&free, "old", 90);
        let mut named_entry = entry("named", &named, 0);
        named_entry["owners"] = json!(["sccache"]);
        let cfg = disk(
            50,
            vec![
                named_entry,
                entry("by-exe", &by_exe, 0),
                entry("by-cwd", &by_cwd, 0),
                entry("free", &free, 0),
            ],
        );
        let procs = Fixed(ProcessView::Seen(vec![
            ProcessFacts {
                names: vec!["sccache".into()],
                ..ProcessFacts::default()
            },
            ProcessFacts {
                paths: vec![by_exe.join("bin/lsp")],
                ..ProcessFacts::default()
            },
            ProcessFacts {
                other_paths: vec![by_cwd.clone()],
                ..ProcessFacts::default()
            },
        ]));
        let r = sweep_caches(
            &cfg,
            &below(),
            0,
            Mode::Apply,
            SystemTime::now(),
            &procs,
            &allowed,
        )
        .unwrap();
        assert!(named_file.exists());
        assert!(by_exe_file.exists());
        assert!(by_cwd_file.exists());
        assert!(!free_file.exists());
        let status: Vec<EntryStatus> = r.caches.iter().map(|c| c.status).collect();
        assert_eq!(
            status,
            vec![
                EntryStatus::OwnerLive,
                EntryStatus::OwnerLive,
                EntryStatus::OwnerLive,
                EntryStatus::Open
            ]
        );
    }

    #[test]
    fn a_blind_process_table_removes_nothing() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let old = file(&dir, "old", 90);
        let cfg = disk(50, vec![entry("x", &dir, 0)]);
        let r = sweep_caches(
            &cfg,
            &below(),
            0,
            Mode::Apply,
            SystemTime::now(),
            &Fixed(ProcessView::Blind("unread".into())),
            &allowed,
        )
        .unwrap();
        assert!(old.exists());
        assert_eq!(r.caches[0].status, EntryStatus::Blind);
    }

    #[test]
    fn a_recent_write_keeps_a_child() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let fresh = file(&dir, "fresh", 0);
        let cfg = DiskConfig {
            active_minutes: 60,
            ..disk(50, vec![entry("x", &dir, 0)])
        };
        let r = run(&cfg, &below(), Mode::Apply, &allowed);
        assert!(fresh.exists());
        assert!(r.caches[0].removing.is_empty());
    }

    #[test]
    fn symlinks_are_never_followed() {
        let (_tmp, home, allowed) = home();
        let precious = home.join("precious");
        let kept = file(&precious, "file", 90);
        let dir = home.join("caches/x");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&precious, dir.join("a-child-link")).unwrap();
        let nested = dir.join("b-child");
        file(&nested, "old", 90);
        std::os::unix::fs::symlink(&precious, nested.join("inner-link")).unwrap();
        File::open(&nested)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(90 * 86_400))
            .unwrap();
        let cfg = disk(50, vec![entry("x", &dir, 0)]);
        let r = sweep_caches(
            &cfg,
            &below(),
            0,
            Mode::Apply,
            SystemTime::now() + Duration::from_secs(86_400),
            &nobody(),
            &allowed,
        )
        .unwrap();
        assert_eq!(r.caches[0].children, 1, "a symlink child is not a candidate");
        assert!(!nested.exists());
        assert!(kept.exists(), "the referent survives");
        assert!(dir.join("a-child-link").symlink_metadata().is_ok());
    }

    #[test]
    fn a_read_only_module_tree_is_removed_whole() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("go/pkg/mod");
        let module = dir.join("example.com/x@v1.0.0");
        file(&module.join("internal/deep"), "a.go", 90);
        let readonly = std::fs::Permissions::from_mode(0o555);
        for d in [
            module.join("internal/deep"),
            module.join("internal"),
            module.clone(),
            dir.join("example.com"),
        ] {
            std::fs::set_permissions(&d, readonly.clone()).unwrap();
        }
        let root = CacheDir::recognize(&at(&dir), &allowed).unwrap();
        let child = root.children().unwrap().pop().unwrap();
        remove_child(&child).unwrap();
        assert!(!dir.join("example.com").exists());
    }

    #[test]
    fn a_child_holding_a_git_dir_is_protected_and_still_counts() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let repo = dir.join("a-clone/nested");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let source = file(&repo, "source.rs", 90);
        let plain = file(&dir, "b-plain", 90);
        let cfg = disk(50, vec![entry("x", &dir, 0)]);
        let r = run(&cfg, &below(), Mode::Apply, &allowed);
        assert!(source.exists());
        assert!(!plain.exists());
        assert_eq!(r.caches[0].protected.len(), 1);
        assert!(r.caches[0].gib > 0.0);
    }

    #[test]
    fn removal_rechecks_the_root_and_the_tree() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let tree = dir.join("child");
        let inside = file(&tree, "f", 90);
        let root = CacheDir::recognize(&at(&dir), &allowed).unwrap();
        let child = root.children().unwrap().pop().unwrap();

        std::fs::create_dir_all(tree.join("late/.git")).unwrap();
        assert!(matches!(remove_child(&child), Err(CacheRemoveError::HasGit)));
        assert!(inside.exists());
        std::fs::remove_dir_all(tree.join("late")).unwrap();

        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(matches!(
            remove_child(&child),
            Err(CacheRemoveError::NoLongerRecognized(NotCache::HasGit))
        ));
        assert!(inside.exists());
        std::fs::remove_dir_all(dir.join(".git")).unwrap();

        std::fs::remove_dir_all(&tree).unwrap();
        let precious = home.join("precious");
        let kept = file(&precious, "f", 90);
        std::os::unix::fs::symlink(&precious, &tree).unwrap();
        assert!(matches!(remove_child(&child), Err(CacheRemoveError::Symlink)));
        assert!(kept.exists());
    }

    #[test]
    fn a_large_cache_fits_the_policy_budget() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..3000 {
            std::fs::write(dir.join(format!("{i}")), "x").unwrap();
        }
        let cfg = DiskConfig {
            dry_run: true,
            ..disk(50, vec![entry("x", &dir, 0)])
        };
        let r = sweep_caches(
            &cfg,
            &below(),
            0,
            Mode::Apply,
            SystemTime::now() + Duration::from_secs(30 * 86_400),
            &nobody(),
            &allowed,
        )
        .unwrap();
        assert_eq!(r.caches[0].children, 3000);
        assert_eq!(r.caches[0].removing.len(), 3000);

        let mut over_budget = entry("x", &dir, 0);
        over_budget["max_idle_days"] = json!(400);
        let cfg = DiskConfig {
            dry_run: true,
            ..disk(50, vec![over_budget])
        };
        let r = sweep_caches(
            &cfg,
            &below(),
            0,
            Mode::Apply,
            SystemTime::now() + Duration::from_secs(30 * 86_400),
            &nobody(),
            &allowed,
        )
        .unwrap();
        assert_eq!(r.caches[0].removing.len(), 3000);
        assert_eq!(r.caches[0].removing[0].verdict, Verdict::RemoveOverBudget);
    }

    fn cargo_dir(root: &Path, name: &str) -> PathBuf {
        let t = root.join(name);
        std::fs::create_dir_all(t.join("debug")).unwrap();
        std::fs::write(t.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        std::fs::write(t.join("debug/blob"), vec![7u8; 8192]).unwrap();
        t
    }

    fn cargo_recent() -> CargoTargetConfig {
        CargoTargetConfig {
            max_idle_days: 400,
            budget_gib: 1,
            pressure_budget_gib: 0,
            active_minutes: 0,
            ..CargoTargetConfig::default()
        }
    }

    fn cycle_of(
        cargo: &CargoTargetConfig,
        cfg: &DiskConfig,
        repo: &Path,
        reading: &DiskReading,
        allowed: &Allowed,
    ) -> Report {
        cycle(
            cargo,
            cfg,
            &[repo.to_path_buf()],
            reading,
            Mode::Apply,
            Mode::Apply,
            SystemTime::now() + Duration::from_secs(86_400),
            &nobody(),
            allowed,
        )
        .unwrap()
    }

    #[test]
    fn below_the_goal_the_sweep_uses_the_tighter_budget() {
        let (_tmp, home, allowed) = home();
        let repo = home.join("repo");
        let target = cargo_dir(&repo, "target");

        let r = cycle_of(&cargo_recent(), &disk(50, vec![]), &repo, &met(), &allowed);
        assert!(target.exists(), "goal met: the ordinary budget holds");
        assert!(!r.pressured);

        let r = cycle_of(&cargo_recent(), &disk(50, vec![]), &repo, &below(), &allowed);
        assert!(!target.exists(), "below the goal: the tighter budget");
        assert!(r.pressured);
        assert_eq!(r.disk.unwrap().state, GoalState::Below);
    }

    #[test]
    fn a_disabled_goal_leaves_the_sweep_as_it_was() {
        let (_tmp, home, allowed) = home();
        let repo = home.join("repo");
        let target = cargo_dir(&repo, "target");
        let off = DiskConfig {
            enable: false,
            free_goal_pct: 50,
            ..DiskConfig::default()
        };
        let r = cycle_of(&cargo_recent(), &off, &repo, &below(), &allowed);
        assert!(target.exists());
        assert!(!r.pressured);
        assert!(r.disk.is_none());
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("disk").is_none(), "the report is unchanged");

        let pressured = DiskReading {
            pressured: true,
            ..below()
        };
        let r = cycle_of(
            &cargo_recent(),
            &DiskConfig::default(),
            &repo,
            &pressured,
            &allowed,
        );
        assert!(!target.exists(), "emergency pressure still tightens");
        assert!(r.pressured);
    }

    #[test]
    fn what_the_cargo_sweep_frees_counts_toward_the_goal() {
        let (_tmp, home, allowed) = home();
        let repo = home.join("repo");
        cargo_dir(&repo, "target");
        let dir = home.join("caches/x");
        let old = file(&dir, "old", 90);
        let reading = DiskReading {
            read: true,
            free_bytes: 500_000 - 1,
            total_bytes: 1_000_000,
            pressured: false,
        };
        let cfg = disk(50, vec![entry("x", &dir, 0)]);
        let r = cycle_of(&cargo_recent(), &cfg, &repo, &reading, &allowed);
        assert!(old.exists(), "the cargo sweep alone reached the goal");
        assert_eq!(r.disk.unwrap().state, GoalState::Met);
    }

    fn after(secs: u64, freed: u64) -> Option<LastSweep> {
        Some(LastSweep {
            elapsed: Duration::from_secs(secs),
            freed_bytes: freed,
        })
    }

    #[test]
    fn the_goal_triggers_on_the_short_clock_and_backs_off_when_it_frees_nothing() {
        let cargo = CargoTargetConfig::default();
        let on = disk(50, vec![]);
        let off = DiskConfig::default();
        let kind = |d: &DiskConfig, r: &DiskReading, l| {
            trigger(&cargo, d, r, l).unwrap().map(|t| t.kind)
        };
        assert_eq!(kind(&off, &below(), None), None);
        assert_eq!(kind(&on, &met(), None), None);
        assert_eq!(kind(&on, &below(), None), Some(TriggerKind::Goal));
        assert_eq!(kind(&on, &below(), after(299, 1)), None);
        assert_eq!(kind(&on, &below(), after(300, 1)), Some(TriggerKind::Goal));
        assert_eq!(kind(&on, &below(), after(300, 0)), None);
        assert_eq!(kind(&on, &below(), after(3600, 0)), Some(TriggerKind::Goal));
    }

    #[test]
    fn the_older_triggers_keep_their_order_and_their_off_switch() {
        let pressured = DiskReading {
            pressured: true,
            ..below()
        };
        let on = disk(50, vec![]);
        let scheduled = CargoTargetConfig {
            enable: true,
            ..CargoTargetConfig::default()
        };
        let t = trigger(&scheduled, &on, &met(), None).unwrap().unwrap();
        assert_eq!(t.kind, TriggerKind::Schedule);
        let relief = CargoTargetConfig::default();
        let t = trigger(&relief, &on, &pressured, None).unwrap().unwrap();
        assert_eq!(t.kind, TriggerKind::Pressure);
        let no_relief = CargoTargetConfig {
            auto_apply: false,
            ..CargoTargetConfig::default()
        };
        assert_eq!(
            trigger(&no_relief, &DiskConfig::default(), &pressured, None).unwrap(),
            None
        );
        let t = trigger(&no_relief, &on, &below(), None).unwrap().unwrap();
        assert_eq!(t.kind, TriggerKind::Goal);
        assert_eq!(
            t.cargo_mode,
            Mode::DryRun,
            "auto_apply off keeps the target dirs out of a goal sweep"
        );
    }

    #[test]
    fn a_bad_percent_is_refused_alone() {
        let (_tmp, home, allowed) = home();
        let dir = home.join("caches/x");
        let old = file(&dir, "old", 90);
        let cfg = disk(100, vec![entry("x", &dir, 0)]);
        assert_eq!(assess(&cfg, &below()).unwrap().state, GoalState::Refused);
        let r = run(&cfg, &below(), Mode::Apply, &allowed);
        assert_eq!(r.caches.len(), 1, "the entries are still reported");
        assert!(old.exists());
        let pressured = DiskReading {
            pressured: true,
            ..below()
        };
        run(&cfg, &pressured, Mode::Apply, &allowed);
        assert!(!old.exists(), "emergency pressure still evicts");
    }

    #[test]
    fn the_block_parses_from_yaml_with_a_broken_entry_beside_a_good_one() {
        let cfg: DiskConfig = serde_yaml_ng::from_str(
            "enable: true\nfree_goal_pct: 25\ncaches:\n  - {name: go, path: ~/go/pkg, budget_gib: 5, max_idle_days: 30, enable: true, owners: [go]}\n  - {name: bad, path: 7}\n",
        )
        .unwrap();
        assert_eq!(cfg.free_goal_pct, 25);
        assert_eq!(cfg.active_minutes, DEFAULT_ACTIVE_MINUTES);
        let (entries, refused) = slot_entries(&cfg);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].max_idle_days, 30);
        assert_eq!(entries[0].owners, vec!["go".to_string()]);
        assert_eq!(refused[0].reason, NotCache::Malformed);
        assert_eq!(refused[0].name, "bad");
    }

    #[test]
    fn an_entry_is_off_until_it_says_otherwise() {
        let cfg: DiskConfig =
            serde_yaml_ng::from_str("caches:\n  - {name: a, path: /x, budget_gib: 1}\n").unwrap();
        assert!(!cfg.enable);
        let (entries, _) = slot_entries(&cfg);
        assert!(!entries[0].enable);
        assert_eq!(entries[0].max_idle_days, DEFAULT_MAX_IDLE_DAYS);
        assert_eq!(DiskConfig::default().free_goal_pct, 0);
    }

    #[test]
    fn verdicts_the_policy_can_answer_decode() {
        assert_eq!(
            Verdict::from_word("in-use-process"),
            Some(Verdict::Active {
                signal: ActiveSignal::ProcessUses
            })
        );
    }
}
