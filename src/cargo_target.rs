//! Bounds on local cargo `target/` directories in the workspace repos.
//!
//! # Why
//!
//! Measured 2026-09-29 on the operator's Mac: 35 cargo `target/`
//! directories under `~/code/github` held **86.7 GiB**, all `debug/`
//! output from local `cargo` and rust-analyzer runs (`fumi/target` alone
//! was 14 GiB), on a data volume 88% full with 60 GiB free. Nix builds
//! never write these directories and nothing else managed them, so they
//! only grew. tend already walks every workspace repo each cycle, and it
//! already knows when the disk is tight ([`crate::pressure`]), so the
//! bound goes here: where the repos are, the same reasoning that put
//! [`crate::logrotate`] next to the log writers.
//!
//! # The safety invariant
//!
//! **A directory is removable only if it is a [`CargoTargetDir`], and the
//! only constructor is [`CargoTargetDir::recognize`].** Recognition requires
//! all of:
//!
//! - the directory's own name is exactly `target` (byte comparison, so a
//!   `Target/` React component folder on a case-insensitive volume is not it);
//! - it is a real directory, not a symlink (removing through a symlink would
//!   delete whatever it points at);
//! - it holds a `CACHEDIR.TAG` regular file that starts with the
//!   cachedir-spec signature AND says cargo created it. The signature alone
//!   is not enough: Python virtualenvs and pytest caches write the same
//!   signature, and a Maven `target/` writes no tag at all.
//!
//! [`remove`] re-runs recognition immediately before deleting, so a directory
//! swapped between scan and removal is refused too.
//!
//! # The in-use signal
//!
//! Two signals, both checked; either one keeps a directory:
//!
//! 1. **A cargo lock is held.** cargo holds an exclusive `flock` on
//!    `<target>/<profile>/.cargo-lock` (and, in newer cargo,
//!    `.cargo-build-lock` / `.cargo-artifact-lock`) for the whole of a build.
//!    tend probes each with a non-blocking shared lock; `WouldBlock` means a
//!    build is running. This is definitive, but only while a build runs.
//! 2. **Recent writes.** A directory whose newest mtime is within
//!    `active_minutes` is treated as in use. rust-analyzer and an editing
//!    session write `target/` in bursts with no lock held in between, so the
//!    lock alone would call a repo idle between two saves.
//!
//! [`remove`] also holds an EXCLUSIVE lock on every cargo lock file while it
//! deletes, so a build that starts after the scan waits rather than writing
//! into a half-deleted tree.
//!
//! # What this is not
//!
//! Not `cargo clean`: it never runs cargo, and it removes a target directory
//! whole. A removed directory costs one cold build to bring back; that is the
//! whole downside, and it is why idleness, not size, is the primary rule.

use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

// ── policy surface ─────────────────────────────────────────────────────

/// Default idle age after which a target directory is removed whole.
///
/// Two weeks: long enough that a repo touched every sprint keeps its warm
/// cache, short enough that a repo built once for a one-off check stops
/// holding GiB for months. A mistake here costs one cold build.
pub(crate) const DEFAULT_MAX_IDLE_DAYS: u32 = 14;

/// Default ceiling on the TOTAL across every workspace repo, GiB.
///
/// The measured 86.7 GiB across 35 directories is ~2.5 GiB each; 50 GiB
/// keeps roughly twenty warm caches, which is more repos than one person
/// actively builds in a fortnight.
pub(crate) const DEFAULT_BUDGET_GIB: u64 = 50;

/// Default ceiling while [`crate::pressure`] reports disk pressure, GiB.
///
/// Under pressure the ordinary budget is not enough — the host is already
/// near the point where tend stops working entirely — so the sweep tightens
/// to this and keeps only the most recently used caches.
pub(crate) const DEFAULT_PRESSURE_BUDGET_GIB: u64 = 10;

/// Default recent-write window treated as "in use", minutes.
pub(crate) const DEFAULT_ACTIVE_MINUTES: u64 = 60;

/// Default minimum time between daemon sweeps, minutes. Measuring means
/// stat'ing every file in every target dir (hundreds of thousands of
/// inodes), which is too heavy for every 300s daemon cycle and pointless at
/// that rate: idleness is measured in days.
pub(crate) const DEFAULT_INTERVAL_MINUTES: u64 = 60;

/// Default search depth below each repo root. 1 finds only `<repo>/target`;
/// 2 also finds `<repo>/<dir>/target` (a nested, non-workspace crate). All 35
/// measured directories were at depth 1.
pub(crate) const DEFAULT_SEARCH_DEPTH: u32 = 2;

/// `cargo_target:` in tend's config. Every field defaults, so a partial
/// block is valid and an absent block means disabled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CargoTargetConfig {
    /// Run the sweep from the daemon. Default false: tend deletes nothing
    /// unless the operator turns this on. `tend cargo-target report` works
    /// either way.
    pub enable: bool,
    /// Remove a directory whose newest mtime is older than this many days.
    pub max_idle_days: u32,
    /// Total GiB the target dirs may hold; least-recently-used go first.
    pub budget_gib: u64,
    /// Tighter total used while disk pressure is high.
    pub pressure_budget_gib: u64,
    /// A directory written within this many minutes counts as in use.
    pub active_minutes: u64,
    /// Minimum minutes between daemon sweeps.
    pub interval_minutes: u64,
    /// How deep below each repo root to look for `target/`.
    pub search_depth: u32,
    /// Decide and report, but delete nothing.
    pub dry_run: bool,
}

impl Default for CargoTargetConfig {
    fn default() -> Self {
        Self {
            enable: false,
            max_idle_days: DEFAULT_MAX_IDLE_DAYS,
            budget_gib: DEFAULT_BUDGET_GIB,
            pressure_budget_gib: DEFAULT_PRESSURE_BUDGET_GIB,
            active_minutes: DEFAULT_ACTIVE_MINUTES,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            search_depth: DEFAULT_SEARCH_DEPTH,
            dry_run: false,
        }
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

impl CargoTargetConfig {
    /// The pure policy this config selects.
    #[must_use]
    pub fn policy(&self) -> Policy {
        Policy {
            max_idle: Duration::from_secs(u64::from(self.max_idle_days) * 86_400),
            budget_bytes: self.budget_gib.saturating_mul(GIB),
            pressure_budget_bytes: self.pressure_budget_gib.saturating_mul(GIB),
            active_window: Duration::from_secs(self.active_minutes.saturating_mul(60)),
        }
    }

    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_minutes.saturating_mul(60))
    }
}

// ── the guarded type ───────────────────────────────────────────────────

/// Name a directory must have to be a cargo target dir.
const TARGET_DIR_NAME: &str = "target";
/// First line of every cachedir-spec tag (https://bford.info/cachedir/).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";
/// What cargo writes on the tag's second line; the signature alone is shared
/// with virtualenv and pytest.
const CARGO_TAG_MARK: &str = "created by cargo";
/// Lock files cargo holds (flock, exclusive) during a build.
const CARGO_LOCK_NAMES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];
/// Lock files sit at `<target>/<profile>/` or `<target>/<tool>/<profile>/`.
const LOCK_MAX_DEPTH: usize = 3;

/// Why a directory named `target` is NOT a cargo target dir. Reported, never
/// acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotCargoTarget {
    /// Its name is not exactly `target`.
    WrongName,
    /// A symlink — deleting through it would delete its referent.
    Symlink,
    /// Not a directory, or unreadable.
    NotADirectory,
    /// No `CACHEDIR.TAG` (a Maven `target/`, a source folder).
    NoCachedirTag,
    /// A tag, but not one cargo wrote.
    ForeignTag,
}

impl NotCargoTarget {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::WrongName => "name is not exactly `target`",
            Self::Symlink => "is a symlink",
            Self::NotADirectory => "is not a directory",
            Self::NoCachedirTag => "has no CACHEDIR.TAG",
            Self::ForeignTag => "CACHEDIR.TAG was not written by cargo",
        }
    }
}

/// A directory proven to be a cargo target dir. The field is private and
/// [`CargoTargetDir::recognize`] is the only constructor, so no code path can
/// hand [`remove`] a directory that was not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTargetDir {
    path: PathBuf,
}

impl CargoTargetDir {
    /// Check every clause of the invariant (see the module doc).
    ///
    /// # Errors
    /// The first clause the directory fails.
    pub fn recognize(path: &Path) -> Result<Self, NotCargoTarget> {
        if path.file_name().and_then(|n| n.to_str()) != Some(TARGET_DIR_NAME) {
            return Err(NotCargoTarget::WrongName);
        }
        let meta = std::fs::symlink_metadata(path).map_err(|_| NotCargoTarget::NotADirectory)?;
        if meta.file_type().is_symlink() {
            return Err(NotCargoTarget::Symlink);
        }
        if !meta.is_dir() {
            return Err(NotCargoTarget::NotADirectory);
        }
        let tag = path.join("CACHEDIR.TAG");
        match std::fs::symlink_metadata(&tag) {
            Ok(m) if m.is_file() => {}
            _ => return Err(NotCargoTarget::NoCachedirTag),
        }
        let text = std::fs::read_to_string(&tag).map_err(|_| NotCargoTarget::ForeignTag)?;
        if !text.starts_with(CACHEDIR_SIGNATURE) || !text.contains(CARGO_TAG_MARK) {
            return Err(NotCargoTarget::ForeignTag);
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ── discovery ──────────────────────────────────────────────────────────

/// Directories never descended into while looking for `target/`: VCS state,
/// vendored trees and JS dependency trees hold no cargo output of this
/// repo's, and walking them is most of the cost.
const SKIP_DIRS: &[&str] = &["node_modules", "vendor"];

/// What discovery found.
#[derive(Debug, Default)]
pub struct Discovered {
    /// (repo, recognized dir).
    pub dirs: Vec<(PathBuf, CargoTargetDir)>,
    /// A `target` directory that failed recognition, and why.
    pub refused: Vec<(PathBuf, NotCargoTarget)>,
}

/// Find `target/` directories up to `depth` levels below each repo root.
/// Hidden directories are not descended into, and nothing inside a `target`
/// is searched.
#[must_use]
pub fn discover(repos: &[PathBuf], depth: u32) -> Discovered {
    let mut out = Discovered::default();
    for repo in repos {
        let mut stack: Vec<(PathBuf, u32)> = vec![(repo.clone(), 1)];
        while let Some((dir, level)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_dir() {
                    // A symlink named `target` is reported, not followed.
                    if ft.is_symlink() && entry.file_name() == TARGET_DIR_NAME {
                        out.refused.push((entry.path(), NotCargoTarget::Symlink));
                    }
                    continue;
                }
                let name = entry.file_name();
                if name == TARGET_DIR_NAME {
                    match CargoTargetDir::recognize(&entry.path()) {
                        Ok(d) => out.dirs.push((repo.clone(), d)),
                        Err(why) => out.refused.push((entry.path(), why)),
                    }
                    continue;
                }
                let name = name.to_string_lossy();
                if level < depth && !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
                    stack.push((entry.path(), level + 1));
                }
            }
        }
    }
    out.dirs.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    out.refused.sort();
    out
}

// ── measurement ────────────────────────────────────────────────────────

/// A recognized directory plus what the policy needs to know about it.
#[derive(Debug, Clone)]
pub struct Measured {
    pub repo: PathBuf,
    pub dir: CargoTargetDir,
    pub candidate: Candidate,
}

/// The policy's view of one directory — plain data, so [`decide`] is pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Allocated bytes (st_blocks x 512), hard links counted once — what
    /// removing it would actually free, which is what `du` reports.
    pub bytes: u64,
    /// Newest mtime of anything in the tree, the directory included.
    pub newest: SystemTime,
    /// A cargo lock file inside is held by a running build.
    pub lock_held: bool,
}

/// Walk `dir` without following symlinks: size, newest mtime, lock state.
///
/// # Errors
/// Only when the root itself cannot be read; unreadable entries below it are
/// skipped (they can only make the size an under-estimate).
pub fn measure(repo: &Path, dir: CargoTargetDir) -> io::Result<Measured> {
    let root_meta = std::fs::symlink_metadata(dir.path())?;
    let mut bytes: u64 = root_meta.blocks() * 512;
    let mut newest = root_meta.modified()?;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut locks: Vec<PathBuf> = Vec::new();

    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.path().to_path_buf(), 0)];
    while let Some((d, level)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if let Ok(m) = meta.modified() {
                newest = newest.max(m);
            }
            // Hard links (cargo links `target/debug/<bin>` to `deps/<bin>-<hash>`)
            // occupy their blocks once.
            if meta.nlink() <= 1 || seen.insert((meta.dev(), meta.ino())) {
                bytes += meta.blocks() * 512;
            }
            if meta.is_dir() {
                stack.push((path, level + 1));
            } else if meta.is_file()
                && level < LOCK_MAX_DEPTH
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| CARGO_LOCK_NAMES.contains(&n))
            {
                locks.push(path);
            }
        }
    }

    let lock_held = locks.iter().any(|l| lock_is_held(l));
    Ok(Measured {
        repo: repo.to_path_buf(),
        dir,
        candidate: Candidate {
            bytes,
            newest,
            lock_held,
        },
    })
}

/// A non-blocking SHARED lock attempt: fails with `WouldBlock` only while a
/// cargo build holds its exclusive lock. Shared, so tend can never make a
/// build wait on the probe beyond the instant it takes.
fn lock_is_held(path: &Path) -> bool {
    let Ok(f) = File::open(path) else {
        return false;
    };
    matches!(f.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock))
}

// ── the pure policy ────────────────────────────────────────────────────

/// The numbers [`decide`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub max_idle: Duration,
    pub budget_bytes: u64,
    pub pressure_budget_bytes: u64,
    pub active_window: Duration,
}

/// Why a directory is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActiveSignal {
    /// A cargo build holds its lock right now.
    LockHeld,
    /// Written within the active window.
    RecentlyWritten,
}

/// What the policy decided for one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "verdict")]
pub enum Verdict {
    /// In use; never removed, whatever the budget says.
    Active { signal: ActiveSignal },
    /// Within policy.
    Keep,
    /// Idle longer than `max_idle`.
    RemoveIdle,
    /// Not idle, but the least recently used while the total is over budget.
    RemoveOverBudget,
}

impl Verdict {
    #[must_use]
    pub const fn removes(self) -> bool {
        matches!(self, Self::RemoveIdle | Self::RemoveOverBudget)
    }
}

/// [`decide`]'s answer, one verdict per candidate in input order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub verdicts: Vec<Verdict>,
    pub total_bytes: u64,
    pub after_bytes: u64,
    pub effective_budget_bytes: u64,
    /// Still over budget after every removable directory went: the active
    /// ones alone exceed it. Reported rather than resolved by touching a
    /// directory in use — the same stance as `logrotate`'s live files.
    pub budget_unreachable: bool,
}

/// Decide, given measurements, the policy, the clock and disk pressure.
///
/// In order: in-use directories are fixed as `Active`; idle ones are removed;
/// then, if the rest is still over the effective budget (the tighter pressure
/// budget when `pressured`), the least recently used of the remainder go
/// until it fits.
#[must_use]
pub fn decide(cands: &[Candidate], policy: Policy, now: SystemTime, pressured: bool) -> Plan {
    let age = |c: &Candidate| now.duration_since(c.newest).unwrap_or(Duration::ZERO);

    let mut verdicts: Vec<Verdict> = cands
        .iter()
        .map(|c| {
            if c.lock_held {
                Verdict::Active {
                    signal: ActiveSignal::LockHeld,
                }
            } else if age(c) < policy.active_window {
                // A future mtime (clock skew) has age zero: in use, not idle.
                Verdict::Active {
                    signal: ActiveSignal::RecentlyWritten,
                }
            } else if age(c) > policy.max_idle {
                Verdict::RemoveIdle
            } else {
                Verdict::Keep
            }
        })
        .collect();

    let total: u64 = cands.iter().map(|c| c.bytes).sum();
    let mut after: u64 = cands
        .iter()
        .zip(&verdicts)
        .filter(|(_, v)| !v.removes())
        .map(|(c, _)| c.bytes)
        .sum();

    let budget = if pressured {
        policy.budget_bytes.min(policy.pressure_budget_bytes)
    } else {
        policy.budget_bytes
    };

    if after > budget {
        let mut lru: Vec<usize> = (0..cands.len())
            .filter(|&i| verdicts[i] == Verdict::Keep)
            .collect();
        lru.sort_by_key(|&i| cands[i].newest);
        for i in lru {
            if after <= budget {
                break;
            }
            verdicts[i] = Verdict::RemoveOverBudget;
            after -= cands[i].bytes;
        }
    }

    Plan {
        verdicts,
        total_bytes: total,
        after_bytes: after,
        effective_budget_bytes: budget,
        budget_unreachable: after > budget,
    }
}

// ── removal ────────────────────────────────────────────────────────────

/// Why a removal did not happen.
#[derive(Debug)]
pub enum RemoveError {
    /// Recognition failed at removal time.
    NoLongerRecognized(NotCargoTarget),
    /// A build took a cargo lock after the scan.
    BecameActive,
    Io(io::Error),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLongerRecognized(why) => {
                write!(f, "no longer a cargo target dir: {}", why.reason())
            }
            Self::BecameActive => f.write_str("a cargo build took its lock after the scan"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Remove a recognized directory whole.
///
/// Re-recognizes first, then takes an EXCLUSIVE non-blocking lock on every
/// cargo lock file and holds them across the delete: a build already running
/// makes this refuse, and one starting now waits instead of writing into a
/// tree being deleted.
///
/// # Errors
/// See [`RemoveError`]. Nothing is deleted on any error before the delete
/// starts.
pub fn remove(dir: &CargoTargetDir) -> Result<(), RemoveError> {
    let dir = CargoTargetDir::recognize(dir.path()).map_err(RemoveError::NoLongerRecognized)?;
    let mut held: Vec<File> = Vec::new();
    for lock in lock_files(dir.path()) {
        let Ok(f) = File::open(&lock) else { continue };
        match f.try_lock() {
            Ok(()) => held.push(f),
            Err(std::fs::TryLockError::WouldBlock) => return Err(RemoveError::BecameActive),
            Err(std::fs::TryLockError::Error(e)) => return Err(RemoveError::Io(e)),
        }
    }
    let result = std::fs::remove_dir_all(dir.path()).map_err(RemoveError::Io);
    drop(held);
    result
}

/// Cargo lock files within [`LOCK_MAX_DEPTH`] of the target root.
fn lock_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((d, level)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() && level + 1 < LOCK_MAX_DEPTH {
                stack.push((entry.path(), level + 1));
            } else if ft.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| CARGO_LOCK_NAMES.contains(&n))
            {
                out.push(entry.path());
            }
        }
    }
    out
}

// ── the sweep: discover → measure → decide → (remove) ─────────────────

/// Whether a sweep deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    DryRun,
    Apply,
}

/// One directory in a report.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub repo: String,
    pub path: String,
    pub bytes: u64,
    pub gib: f64,
    pub idle_days: f64,
    #[serde(flatten)]
    pub verdict: Verdict,
    /// Set in apply mode: whether the removal happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A `target` directory that was not recognized.
#[derive(Debug, Clone, Serialize)]
pub struct Refused {
    pub path: String,
    pub reason: NotCargoTarget,
}

/// The whole sweep, typed — what the CLI prints and the MCP tool returns.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub mode: Mode,
    pub pressured: bool,
    pub count: usize,
    pub total_gib: f64,
    pub reclaim_gib: f64,
    pub after_gib: f64,
    pub effective_budget_gib: f64,
    pub budget_unreachable: bool,
    /// Bytes actually freed (apply mode; 0 in a dry run).
    pub freed_gib: f64,
    pub entries: Vec<Entry>,
    pub refused: Vec<Refused>,
    pub unmeasurable: Vec<String>,
}

#[allow(clippy::cast_precision_loss)]
fn to_gib(b: u64) -> f64 {
    b as f64 / GIB as f64
}

/// Discover, measure, decide, and in [`Mode::Apply`] remove.
///
/// `cfg.dry_run` forces a dry run whatever `mode` says: config can switch
/// deletion off, a flag can never switch it back on.
#[must_use]
pub fn sweep(
    cfg: &CargoTargetConfig,
    repos: &[PathBuf],
    pressured: bool,
    mode: Mode,
    now: SystemTime,
) -> Report {
    let mode = if cfg.dry_run { Mode::DryRun } else { mode };
    let found = discover(repos, cfg.search_depth.max(1));

    let mut measured = Vec::new();
    let mut unmeasurable = Vec::new();
    for (repo, dir) in found.dirs {
        let shown = dir.path().display().to_string();
        match measure(&repo, dir) {
            Ok(m) => measured.push(m),
            Err(e) => unmeasurable.push(shown + ": " + &e.to_string()),
        }
    }

    let cands: Vec<Candidate> = measured.iter().map(|m| m.candidate).collect();
    let plan = decide(&cands, cfg.policy(), now, pressured);

    let mut freed: u64 = 0;
    let mut entries = Vec::with_capacity(measured.len());
    for (m, v) in measured.iter().zip(&plan.verdicts) {
        let idle = now
            .duration_since(m.candidate.newest)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64()
            / 86_400.0;
        let (removed, error) = if mode == Mode::Apply && v.removes() {
            match remove(&m.dir) {
                Ok(()) => {
                    freed += m.candidate.bytes;
                    (Some(true), None)
                }
                Err(e) => (Some(false), Some(e.to_string())),
            }
        } else {
            (None, None)
        };
        entries.push(Entry {
            repo: m.repo.display().to_string(),
            path: m.dir.path().display().to_string(),
            bytes: m.candidate.bytes,
            gib: to_gib(m.candidate.bytes),
            idle_days: (idle * 10.0).round() / 10.0,
            verdict: *v,
            removed,
            error,
        });
    }
    // Largest first: the report's reader is deciding where the space went.
    entries.sort_by_key(|e| std::cmp::Reverse(e.bytes));

    Report {
        mode,
        pressured,
        count: entries.len(),
        total_gib: to_gib(plan.total_bytes),
        reclaim_gib: to_gib(plan.total_bytes - plan.after_bytes),
        after_gib: to_gib(plan.after_bytes),
        effective_budget_gib: to_gib(plan.effective_budget_bytes),
        budget_unreachable: plan.budget_unreachable,
        freed_gib: to_gib(freed),
        entries,
        refused: found
            .refused
            .into_iter()
            .map(|(p, why)| Refused {
                path: p.display().to_string(),
                reason: why,
            })
            .collect(),
        unmeasurable,
    }
}

impl Verdict {
    /// Short label for the terminal table.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Active {
                signal: ActiveSignal::LockHeld,
            } => "in use (cargo lock held)",
            Self::Active {
                signal: ActiveSignal::RecentlyWritten,
            } => "in use (recent writes)",
            Self::Keep => "keep",
            Self::RemoveIdle => "remove (idle)",
            Self::RemoveOverBudget => "remove (over budget, LRU)",
        }
    }
}

/// Render a report for `tend cargo-target report|apply`.
pub fn print_report(r: &Report, cfg: &CargoTargetConfig) {
    println!(
        "policy        max_idle_days {}  budget {} GiB (pressure {} GiB)  active {} min{}",
        cfg.max_idle_days,
        cfg.budget_gib,
        cfg.pressure_budget_gib,
        cfg.active_minutes,
        if cfg.enable {
            ""
        } else {
            "  [daemon sweep disabled]"
        }
    );
    println!("{:>8}  {:>9}  {:<28}  path", "GiB", "idle days", "verdict");
    for e in &r.entries {
        let outcome = match (e.removed, &e.error) {
            (Some(true), _) => "  [removed]".to_string(),
            (Some(false), Some(err)) => "  [FAILED: ".to_string() + err + "]",
            _ => String::new(),
        };
        println!(
            "{:>8.2}  {:>9.1}  {:<28}  {}{}",
            e.gib,
            e.idle_days,
            e.verdict.label(),
            e.path,
            outcome
        );
    }
    for x in &r.refused {
        println!("  refused   {}  ({})", x.path, x.reason.reason());
    }
    for x in &r.unmeasurable {
        println!("  unmeasurable  {x}");
    }
    let removing = r.entries.iter().filter(|e| e.verdict.removes()).count();
    println!(
        "total         {} dirs, {:.1} GiB; policy removes {} ({:.1} GiB) -> {:.1} GiB against a {:.1} GiB budget{}",
        r.count,
        r.total_gib,
        removing,
        r.reclaim_gib,
        r.after_gib,
        r.effective_budget_gib,
        if r.pressured { " (disk pressure)" } else { "" }
    );
    if r.budget_unreachable {
        println!("              in-use dirs alone exceed the budget; they are kept");
    }
    match r.mode {
        Mode::DryRun => println!("mode          dry run: nothing deleted"),
        Mode::Apply => println!("mode          applied: freed {:.1} GiB", r.freed_gib),
    }
}

/// Disk pressure as [`crate::pressure`] defines it, measured at `path`. An
/// unreadable reading is "not pressured": the idle and budget rules still
/// apply, only the tighter pressure budget is lost.
#[must_use]
pub fn disk_pressured_at(path: &Path, inflight: u32) -> bool {
    let reader = crate::pressure::SystemPressureReader {
        path: path.to_path_buf(),
    };
    crate::pressure::PressureReader::read(&reader)
        .is_ok_and(|r| crate::pressure::Thresholds::default().disk_pressured(&r, inflight))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
        # This file is a cache directory tag created by cargo.\n\
        # For information about cache directory tags see https://bford.info/cachedir/\n";

    /// `<root>/<repo>/target` with a cargo tag and a `debug/` holding `bytes`.
    fn cargo_target(root: &Path, repo: &str, bytes: usize) -> PathBuf {
        let t = root.join(repo).join("target");
        std::fs::create_dir_all(t.join("debug")).unwrap();
        std::fs::write(t.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        std::fs::write(t.join("debug/.cargo-lock"), "").unwrap();
        std::fs::write(t.join("debug/blob"), vec![7u8; bytes]).unwrap();
        t
    }

    fn policy() -> Policy {
        Policy {
            max_idle: Duration::from_secs(14 * 86_400),
            budget_bytes: 100,
            pressure_budget_bytes: 30,
            active_window: Duration::from_secs(3600),
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    fn cand(bytes: u64, days_ago: u64) -> Candidate {
        Candidate {
            bytes,
            newest: now() - Duration::from_secs(days_ago * 86_400),
            lock_held: false,
        }
    }

    // ── recognition: the safety invariant ─────────────────────────────

    #[test]
    fn a_cargo_tagged_target_is_recognized() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        assert!(CargoTargetDir::recognize(&t).is_ok());
    }

    #[test]
    fn a_target_without_a_tag_is_refused() {
        // A Maven `target/`, or a source folder that happens to be named so.
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        std::fs::create_dir_all(&t).unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&t),
            Err(NotCargoTarget::NoCachedirTag)
        );
    }

    #[test]
    fn a_non_cargo_cachedir_tag_is_refused() {
        // virtualenv / pytest write the same signature line.
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# created by virtualenv\n",
        )
        .unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&t),
            Err(NotCargoTarget::ForeignTag)
        );
    }

    #[test]
    fn a_tag_without_the_signature_is_refused() {
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("CACHEDIR.TAG"), "created by cargo\n").unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&t),
            Err(NotCargoTarget::ForeignTag)
        );
    }

    #[test]
    fn a_tagged_dir_with_another_name_is_refused() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let other = tmp.path().join("r/Target-copy");
        std::fs::rename(&t, &other).unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&other),
            Err(NotCargoTarget::WrongName)
        );
    }

    #[test]
    fn a_symlinked_target_is_refused_and_its_referent_survives() {
        let tmp = TempDir::new().unwrap();
        let real = cargo_target(tmp.path(), "elsewhere", 10);
        std::fs::create_dir_all(tmp.path().join("r")).unwrap();
        let link = tmp.path().join("r/target");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            CargoTargetDir::recognize(&link),
            Err(NotCargoTarget::Symlink)
        );
        let found = discover(&[tmp.path().join("r")], 2);
        assert!(found.dirs.is_empty());
        assert_eq!(found.refused, vec![(link, NotCargoTarget::Symlink)]);
        assert!(real.join("debug/blob").exists());
    }

    /// End to end: an untagged `target/` that the policy would otherwise
    /// remove (ancient, over budget) is never deleted, because it never
    /// becomes a `CargoTargetDir` at all.
    #[test]
    fn a_sweep_never_deletes_an_untagged_target() {
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("maven/target");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(t.join("app.jar"), vec![1u8; 4096]).unwrap();
        let cfg = CargoTargetConfig {
            enable: true,
            max_idle_days: 0,
            budget_gib: 0,
            active_minutes: 0,
            ..CargoTargetConfig::default()
        };
        let far_future = SystemTime::now() + Duration::from_secs(365 * 86_400);
        let r = sweep(
            &cfg,
            &[tmp.path().join("maven")],
            true,
            Mode::Apply,
            far_future,
        );
        assert_eq!(r.count, 0);
        assert_eq!(r.refused.len(), 1);
        assert_eq!(r.refused[0].reason, NotCargoTarget::NoCachedirTag);
        assert!(t.join("app.jar").exists());
    }

    #[test]
    fn remove_re_recognizes_before_deleting() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let dir = CargoTargetDir::recognize(&t).unwrap();
        // The tag vanishes between scan and removal.
        std::fs::remove_file(t.join("CACHEDIR.TAG")).unwrap();
        assert!(matches!(
            remove(&dir),
            Err(RemoveError::NoLongerRecognized(
                NotCargoTarget::NoCachedirTag
            ))
        ));
        assert!(t.join("debug/blob").exists());
    }

    // ── discovery ─────────────────────────────────────────────────────

    #[test]
    fn discovery_respects_depth_and_skips_hidden_and_vendor() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("repo");
        cargo_target(&root, ".", 1);
        cargo_target(&root, "sdk", 1);
        cargo_target(&root, "a/b", 1);
        cargo_target(&root, ".claude", 1);
        cargo_target(&root, "vendor", 1);

        let d1 = discover(std::slice::from_ref(&root), 1);
        assert_eq!(d1.dirs.len(), 1, "depth 1 = <repo>/target only");

        let d2 = discover(std::slice::from_ref(&root), 2);
        let paths: Vec<_> = d2
            .dirs
            .iter()
            .map(|(_, d)| d.path().to_path_buf())
            .collect();
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths
            .iter()
            .all(|p| !p.to_string_lossy().contains(".claude")));
        assert!(paths
            .iter()
            .all(|p| !p.to_string_lossy().contains("vendor")));
    }

    // ── measurement ───────────────────────────────────────────────────

    #[test]
    fn measure_counts_hard_links_once() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 64 * 1024);
        let one = CargoTargetDir::recognize(&t).unwrap();
        let before = measure(tmp.path(), one).unwrap().candidate.bytes;
        std::fs::hard_link(t.join("debug/blob"), t.join("debug/blob-link")).unwrap();
        let two = CargoTargetDir::recognize(&t).unwrap();
        let after = measure(tmp.path(), two).unwrap().candidate.bytes;
        assert_eq!(before, after, "a hard link frees nothing extra");
        assert!(before >= 64 * 1024);
    }

    #[test]
    fn a_held_cargo_lock_reads_as_in_use() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let lock = File::open(t.join("debug/.cargo-lock")).unwrap();
        lock.lock().unwrap(); // what cargo does for a build

        let m = measure(tmp.path(), CargoTargetDir::recognize(&t).unwrap()).unwrap();
        assert!(m.candidate.lock_held);
        assert!(matches!(remove(&m.dir), Err(RemoveError::BecameActive)));
        assert!(t.exists(), "a locked target must survive removal");

        drop(lock);
        let m = measure(tmp.path(), CargoTargetDir::recognize(&t).unwrap()).unwrap();
        assert!(!m.candidate.lock_held);
        remove(&m.dir).unwrap();
        assert!(!t.exists());
    }

    // ── the policy ────────────────────────────────────────────────────

    #[test]
    fn idle_past_the_limit_is_removed_and_fresh_is_kept() {
        let p = decide(&[cand(10, 30), cand(10, 3)], policy(), now(), false);
        assert_eq!(p.verdicts, vec![Verdict::RemoveIdle, Verdict::Keep]);
        assert_eq!(p.after_bytes, 10);
    }

    #[test]
    fn exactly_at_the_idle_limit_is_kept() {
        let p = decide(&[cand(10, 14)], policy(), now(), false);
        assert_eq!(p.verdicts, vec![Verdict::Keep]);
    }

    #[test]
    fn over_budget_removes_least_recently_used_first() {
        // 150 bytes against a 100 budget; the oldest (9 days) must go first,
        // then no more than needed.
        let cands = [cand(50, 2), cand(50, 9), cand(50, 5)];
        let p = decide(&cands, policy(), now(), false);
        assert_eq!(
            p.verdicts,
            vec![Verdict::Keep, Verdict::RemoveOverBudget, Verdict::Keep]
        );
        assert_eq!(p.after_bytes, 100);
        assert!(!p.budget_unreachable);
    }

    #[test]
    fn idle_removals_count_toward_the_budget() {
        // The idle one already brings the total under budget: nothing else goes.
        let cands = [cand(80, 30), cand(60, 2)];
        let p = decide(&cands, policy(), now(), false);
        assert_eq!(p.verdicts, vec![Verdict::RemoveIdle, Verdict::Keep]);
    }

    #[test]
    fn pressure_tightens_the_budget() {
        let cands = [cand(20, 1), cand(20, 2), cand(20, 3)];
        let calm = decide(&cands, policy(), now(), false);
        assert!(calm.verdicts.iter().all(|v| *v == Verdict::Keep));

        let tight = decide(&cands, policy(), now(), true);
        assert_eq!(tight.effective_budget_bytes, 30);
        assert_eq!(
            tight.verdicts,
            vec![
                Verdict::Keep,
                Verdict::RemoveOverBudget,
                Verdict::RemoveOverBudget
            ]
        );
    }

    #[test]
    fn active_dirs_are_never_removed_even_when_idle_or_over_budget() {
        let mut locked = cand(500, 90);
        locked.lock_held = true;
        let recent = Candidate {
            bytes: 500,
            newest: now() - Duration::from_secs(600),
            lock_held: false,
        };
        let p = decide(&[locked, recent], policy(), now(), true);
        assert_eq!(
            p.verdicts,
            vec![
                Verdict::Active {
                    signal: ActiveSignal::LockHeld
                },
                Verdict::Active {
                    signal: ActiveSignal::RecentlyWritten
                },
            ]
        );
        assert!(p.budget_unreachable, "reported, not resolved by deleting");
    }

    #[test]
    fn a_future_mtime_reads_as_in_use_not_idle() {
        let skewed = Candidate {
            bytes: 10,
            newest: now() + Duration::from_secs(86_400 * 400),
            lock_held: false,
        };
        let p = decide(&[skewed], policy(), now(), false);
        assert_eq!(
            p.verdicts,
            vec![Verdict::Active {
                signal: ActiveSignal::RecentlyWritten
            }]
        );
    }

    // ── the sweep ─────────────────────────────────────────────────────

    #[test]
    fn dry_run_decides_but_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 4096);
        let cfg = CargoTargetConfig {
            max_idle_days: 1,
            active_minutes: 0,
            ..CargoTargetConfig::default()
        };
        let later = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let r = sweep(&cfg, &[tmp.path().join("r")], false, Mode::DryRun, later);
        assert_eq!(r.count, 1);
        assert_eq!(r.entries[0].verdict, Verdict::RemoveIdle);
        assert!(r.reclaim_gib > 0.0);
        assert_eq!(r.freed_gib, 0.0);
        assert!(t.exists());
    }

    #[test]
    fn config_dry_run_overrides_apply() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 4096);
        let cfg = CargoTargetConfig {
            max_idle_days: 1,
            active_minutes: 0,
            dry_run: true,
            ..CargoTargetConfig::default()
        };
        let later = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let r = sweep(&cfg, &[tmp.path().join("r")], false, Mode::Apply, later);
        assert_eq!(r.mode, Mode::DryRun);
        assert!(t.exists());
    }

    #[test]
    fn apply_removes_idle_dirs_whole() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 4096);
        let cfg = CargoTargetConfig {
            max_idle_days: 1,
            active_minutes: 0,
            ..CargoTargetConfig::default()
        };
        let later = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let r = sweep(&cfg, &[tmp.path().join("r")], false, Mode::Apply, later);
        assert_eq!(r.entries[0].removed, Some(true));
        assert!(r.freed_gib > 0.0);
        assert!(!t.exists());
        assert!(
            tmp.path().join("r").exists(),
            "only target/ goes, never the repo"
        );
    }

    #[test]
    fn config_defaults_and_partial_blocks() {
        let d = CargoTargetConfig::default();
        assert!(!d.enable, "tend deletes nothing unless told to");
        assert_eq!(d.max_idle_days, 14);
        assert_eq!(d.budget_gib, 50);
        assert_eq!(d.pressure_budget_gib, 10);
        assert_eq!(d.active_minutes, 60);
        assert_eq!(d.interval_minutes, 60);
        assert_eq!(d.search_depth, 2);
        assert!(!d.dry_run);

        let partial: CargoTargetConfig =
            serde_yaml_ng::from_str("enable: true\nbudget_gib: 40\n").unwrap();
        assert!(partial.enable);
        assert_eq!(partial.budget_gib, 40);
        assert_eq!(partial.max_idle_days, 14);
    }
}
