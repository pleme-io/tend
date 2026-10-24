//! Bounds on local cargo target directories: the workspace repos' `target/`
//! and the agent caches under configured extra roots.
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
//! Measured 2026-10-23 on ryn: agent builds set `CARGO_TARGET_DIR` to
//! `~/.cache/<x>-target`, outside every repo. Those held ~20 GiB that the
//! repo walk never saw, and filled the disk mid-build twice. `extra_roots`
//! names such directories; each is scanned one level deep.
//!
//! # The safety invariant
//!
//! **A directory is removable only if it is a [`CargoTargetDir`], and the
//! only constructor is [`CargoTargetDir::recognize`].** Every form needs a
//! real directory (not a symlink: removing through one deletes its
//! referent). Then one of three forms:
//!
//! - [`Form::Named`]: the name is exactly `target` (byte comparison, so a
//!   `Target/` React folder on a case-insensitive volume is not it) and it
//!   holds a `CACHEDIR.TAG` regular file that starts with the cachedir-spec
//!   signature AND says cargo created it. The signature alone is not
//!   enough: virtualenv and pytest write it too, and Maven writes no tag.
//! - [`Form::Layout`]: any name, cargo's tag, AND cargo's layout: a
//!   `.rustc_info.json` regular file holding a `rustc_fingerprint`, and a
//!   `.fingerprint/` directory one or two levels down (`<profile>/` or
//!   `<triple>/<profile>/`). A tag alone does not prove a directory of any
//!   other name is cargo's whole output root.
//! - [`Form::Adopted`]: the name `target`, cargo's layout, NO tag, and
//!   `adopt_untagged: true`. Off by default, so such a directory is refused
//!   as [`NotCargoTarget::UntaggedCargoLayout`] and reported.
//!
//! Why tags go missing, measured 2026-10-23: cargo writes `CACHEDIR.TAG`
//! only into a directory it creates. `blue/target` and `kensa/target` were
//! born in the same second as their `rust-analyzer/` child: rust-analyzer's
//! `cargo check --target-dir target/rust-analyzer` created `target/` as a
//! plain parent, so the later `cargo build` found it existing and wrote no
//! tag there. Their layout is cargo's; their tag is absent by accident.
//!
//! [`remove`] re-runs recognition, under the same admission, immediately
//! before deleting, so a directory swapped between scan and removal is
//! refused too.
//!
//! # The in-use signal
//!
//! Four signals; any one keeps a directory, whatever the policy says:
//!
//! 1. **A cargo lock is held.** cargo holds an exclusive `flock` on
//!    `<target>/<profile>/.cargo-lock` (and, in newer cargo,
//!    `.cargo-build-lock` / `.cargo-artifact-lock`) for the whole of a build.
//!    tend probes each with a non-blocking shared lock.
//! 2. **A running process uses it.** A same-user `cargo`, `rustc`,
//!    rust-analyzer, rustdoc, clippy, `cargo-*` or build-script process
//!    whose working directory lies under the directory's parent, or whose
//!    `CARGO_TARGET_DIR` / `CARGO_BUILD_TARGET_DIR` / `--target-dir`
//!    overlaps it, or whose argv names a path inside it; or any same-user
//!    process whose executable lives inside it (a test binary). Other
//!    users' processes cannot write these directories and are not read:
//!    on ryn, 2026-10-23, every nix build ran ~20 `_nixbld` rustc
//!    processes whose owner `sysinfo` could not read, and `ps` names those
//!    owners instead. A root build is seen through its cargo lock only.
//! 3. **Processes unseen.** When the process table cannot be read, or a
//!    same-user build process's working directory or environment cannot
//!    be, nothing can be said about any directory: every one is kept.
//!    Blind is its own state, never "not in use". An unreadable process is
//!    read a second time; one that has exited by then held nothing (rustc
//!    processes live for seconds, and the first ryn run, 2026-10-23, went
//!    blind on one that had just exited).
//! 4. **Recent writes.** A newest mtime within `active_minutes`.
//!
//! [`remove`] holds an EXCLUSIVE lock on every cargo lock file while it
//! deletes, so a build that starts after the scan waits rather than writing
//! into a half-deleted tree.
//!
//! # Policy in blue
//!
//! Which directories go, and in what order, is `blue/cargo_target.b`,
//! loaded through [`crate::blue_policy`]. Rust walks, recognizes, measures,
//! reads processes and deletes. Rust also refuses a plan that would remove
//! a directory any in-use signal holds, so no policy can delete one.
//!
//! # What this is not
//!
//! Not `cargo clean`: it never runs cargo, and it removes a target directory
//! whole. A removed directory costs one cold build to bring back; that is the
//! whole downside, and it is why idleness, not size, is the primary rule.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

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

/// Default minimum time between pressure-relief sweeps, minutes. Under
/// pressure a build can fill the disk within one ordinary interval, so the
/// relief sweep runs on a shorter clock.
pub(crate) const DEFAULT_PRESSURE_INTERVAL_MINUTES: u64 = 5;

/// Default search depth below each repo root. 1 finds only `<repo>/target`;
/// 2 also finds `<repo>/<dir>/target` (a nested, non-workspace crate). All 35
/// measured directories were at depth 1.
pub(crate) const DEFAULT_SEARCH_DEPTH: u32 = 2;

/// `cargo_target:` in tend's config. Every field defaults, so a partial
/// block is valid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CargoTargetConfig {
    /// Run the sweep from the daemon every `interval_minutes`. Default
    /// false. `tend cargo-target report` works either way.
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
    /// Directories scanned one level deep for cargo target dirs of any
    /// name, such as `~/.cache` holding agents' `CARGO_TARGET_DIR`s. `~`
    /// expands. Default empty.
    pub extra_roots: Vec<String>,
    /// Also remove a directory named `target` that has cargo's layout but
    /// no `CACHEDIR.TAG`. Default false: such directories are reported as
    /// `untagged-cargo-layout` and kept.
    pub adopt_untagged: bool,
    /// While disk pressure is high, the daemon sweeps and applies even when
    /// `enable` is false, at most every `pressure_interval_minutes`.
    /// Default true; false leaves only the `enable` schedule.
    pub auto_apply: bool,
    /// Minimum minutes between pressure-relief sweeps.
    pub pressure_interval_minutes: u64,
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
            extra_roots: Vec::new(),
            adopt_untagged: false,
            auto_apply: true,
            pressure_interval_minutes: DEFAULT_PRESSURE_INTERVAL_MINUTES,
        }
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

impl CargoTargetConfig {
    /// The numbers this config hands the policy.
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

    #[must_use]
    pub fn pressure_interval(&self) -> Duration {
        Duration::from_secs(self.pressure_interval_minutes.saturating_mul(60))
    }

    #[must_use]
    pub fn admission(&self) -> Admission {
        Admission {
            untagged: self.adopt_untagged,
        }
    }

    /// `extra_roots` with `~` expanded; blank entries dropped.
    #[must_use]
    pub fn extra_root_paths(&self) -> Vec<PathBuf> {
        self.extra_roots
            .iter()
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .map(|r| PathBuf::from(shellexpand::tilde(r).into_owned()))
            .collect()
    }
}

// ── the guarded type ───────────────────────────────────────────────────

/// Name a directory must have for [`Form::Named`] and [`Form::Adopted`].
const TARGET_DIR_NAME: &str = "target";
/// First line of every cachedir-spec tag (https://bford.info/cachedir/).
const CACHEDIR_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";
/// What cargo writes on the tag's second line; the signature alone is shared
/// with virtualenv and pytest.
const CARGO_TAG_MARK: &str = "created by cargo";
/// cargo's rustc-info cache at the target root, and the key it always holds.
const RUSTC_INFO: &str = ".rustc_info.json";
const RUSTC_INFO_KEY: &str = "rustc_fingerprint";
/// A `.rustc_info.json` larger than this is not cargo's (measured: 3-6 KiB).
const RUSTC_INFO_MAX_BYTES: u64 = 1024 * 1024;
/// cargo's per-unit fingerprint directory inside each profile directory.
const FINGERPRINT_DIR: &str = ".fingerprint";
/// Lock files cargo holds (flock, exclusive) during a build.
const CARGO_LOCK_NAMES: &[&str] = &[".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];
/// Lock files sit at `<target>/<profile>/` or `<target>/<tool>/<profile>/`.
const LOCK_MAX_DEPTH: usize = 3;

/// Which forms [`CargoTargetDir::recognize`] admits beyond the two proven
/// by a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Admission {
    /// Admit [`Form::Adopted`].
    pub untagged: bool,
}

/// How a directory proved it is cargo's output root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Form {
    /// Named `target`, cargo's tag.
    Named,
    /// Any name, cargo's tag and cargo's layout.
    Layout,
    /// Named `target`, cargo's layout, no tag; admitted by `adopt_untagged`.
    Adopted,
}

/// Why a directory is NOT a cargo target dir. Reported, never acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotCargoTarget {
    /// Not named `target`, and no cargo tag.
    WrongName,
    /// A symlink — deleting through it would delete its referent.
    Symlink,
    /// Not a directory, or unreadable.
    NotADirectory,
    /// Named `target`, no `CACHEDIR.TAG`, no cargo layout (a Maven
    /// `target/`, a source folder).
    NoCachedirTag,
    /// A tag, but not one cargo wrote.
    ForeignTag,
    /// Another name with cargo's tag but not cargo's layout.
    NoCargoLayout,
    /// Named `target` with cargo's layout but no tag; `adopt_untagged`
    /// would admit it.
    UntaggedCargoLayout,
}

impl NotCargoTarget {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::WrongName => "name is not `target` and there is no cargo CACHEDIR.TAG",
            Self::Symlink => "is a symlink",
            Self::NotADirectory => "is not a directory",
            Self::NoCachedirTag => "has no CACHEDIR.TAG",
            Self::ForeignTag => "CACHEDIR.TAG was not written by cargo",
            Self::NoCargoLayout => {
                "cargo CACHEDIR.TAG but no cargo layout (.rustc_info.json and a .fingerprint/)"
            }
            Self::UntaggedCargoLayout => {
                "cargo layout but no CACHEDIR.TAG; kept unless adopt_untagged is on"
            }
        }
    }
}

/// A directory proven to be a cargo target dir. The fields are private and
/// [`CargoTargetDir::recognize`] is the only constructor, so no code path can
/// hand [`remove`] a directory that was not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTargetDir {
    path: PathBuf,
    form: Form,
    admission: Admission,
}

enum Tag {
    Cargo,
    Foreign,
    Absent,
}

impl CargoTargetDir {
    /// Check every clause of the invariant (see the module doc).
    ///
    /// # Errors
    /// The first clause the directory fails.
    pub fn recognize(path: &Path, admission: Admission) -> Result<Self, NotCargoTarget> {
        let named = path.file_name().and_then(|n| n.to_str()) == Some(TARGET_DIR_NAME);
        let meta = std::fs::symlink_metadata(path).map_err(|_| NotCargoTarget::NotADirectory)?;
        if meta.file_type().is_symlink() {
            return Err(NotCargoTarget::Symlink);
        }
        if !meta.is_dir() {
            return Err(NotCargoTarget::NotADirectory);
        }
        let form = match (named, tag_of(path)) {
            (true, Tag::Cargo) => Form::Named,
            (true, Tag::Foreign) => return Err(NotCargoTarget::ForeignTag),
            (true, Tag::Absent) if !has_cargo_layout(path) => {
                return Err(NotCargoTarget::NoCachedirTag)
            }
            (true, Tag::Absent) if admission.untagged => Form::Adopted,
            (true, Tag::Absent) => return Err(NotCargoTarget::UntaggedCargoLayout),
            (false, Tag::Cargo) if has_cargo_layout(path) => Form::Layout,
            (false, Tag::Cargo) => return Err(NotCargoTarget::NoCargoLayout),
            (false, _) => return Err(NotCargoTarget::WrongName),
        };
        Ok(Self {
            path: path.to_path_buf(),
            form,
            admission,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn form(&self) -> Form {
        self.form
    }
}

fn tag_of(dir: &Path) -> Tag {
    let tag = dir.join("CACHEDIR.TAG");
    match std::fs::symlink_metadata(&tag) {
        Ok(m) if m.is_file() => {}
        _ => return Tag::Absent,
    }
    match std::fs::read_to_string(&tag) {
        Ok(text) if text.starts_with(CACHEDIR_SIGNATURE) && text.contains(CARGO_TAG_MARK) => {
            Tag::Cargo
        }
        _ => Tag::Foreign,
    }
}

fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

fn has_cargo_layout(dir: &Path) -> bool {
    rustc_info_is_cargos(&dir.join(RUSTC_INFO)) && fingerprint_within(dir)
}

fn rustc_info_is_cargos(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && m.len() <= RUSTC_INFO_MAX_BYTES => {}
        _ => return false,
    }
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Json>(&bytes).ok())
        .is_some_and(|v| v.get(RUSTC_INFO_KEY).is_some())
}

fn fingerprint_within(dir: &Path) -> bool {
    let children = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .map(|es| {
                es.flatten()
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    children(dir).iter().any(|profile| {
        real_dir(&profile.join(FINGERPRINT_DIR))
            || children(profile)
                .iter()
                .any(|p| real_dir(&p.join(FINGERPRINT_DIR)))
    })
}

// ── discovery ──────────────────────────────────────────────────────────

/// Directories never descended into while looking for `target/`: VCS state,
/// vendored trees and JS dependency trees hold no cargo output of this
/// repo's, and walking them is most of the cost.
const SKIP_DIRS: &[&str] = &["node_modules", "vendor"];

/// What discovery found.
#[derive(Debug, Default)]
pub struct Discovered {
    /// (repo or extra root, recognized dir).
    pub dirs: Vec<(PathBuf, CargoTargetDir)>,
    /// A candidate directory that failed recognition, and why.
    pub refused: Vec<(PathBuf, NotCargoTarget)>,
}

/// Find `target/` directories up to `depth` levels below each repo root.
/// Hidden directories are not descended into, and nothing inside a `target`
/// is searched.
#[must_use]
pub fn discover(repos: &[PathBuf], depth: u32, admission: Admission) -> Discovered {
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
                    if ft.is_symlink() && entry.file_name() == TARGET_DIR_NAME {
                        out.refused.push((entry.path(), NotCargoTarget::Symlink));
                    }
                    continue;
                }
                let name = entry.file_name();
                if name == TARGET_DIR_NAME {
                    match CargoTargetDir::recognize(&entry.path(), admission) {
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

/// Recognize each directory directly inside each extra root. A refusal is
/// reported only when the directory carries part of cargo's proof (a tag
/// without the layout, the layout without a tag); the rest of a cache root
/// is other tools' business and is passed over silently.
#[must_use]
pub fn discover_roots(roots: &[PathBuf], admission: Admission) -> Discovered {
    let mut out = Discovered::default();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            match CargoTargetDir::recognize(&entry.path(), admission) {
                Ok(d) => out.dirs.push((root.clone(), d)),
                Err(
                    why @ (NotCargoTarget::NoCargoLayout | NotCargoTarget::UntaggedCargoLayout),
                ) => {
                    out.refused.push((entry.path(), why));
                }
                Err(_) => {}
            }
        }
    }
    out.dirs.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    out.refused.sort();
    out
}

// ── processes ──────────────────────────────────────────────────────────

/// Process names that build into a target dir.
const BUILD_PROCESS_NAMES: &[&str] = &[
    "cargo",
    "rustc",
    "rustdoc",
    "rust-analyzer",
    "clippy-driver",
];
const BUILD_PROCESS_PREFIXES: &[&str] = &["cargo-", "build-script-", "rust-analyzer-"];
const TARGET_DIR_ENV: &[&str] = &["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"];
const TARGET_DIR_FLAG: &str = "--target-dir";

/// What one process says about where it builds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessFacts {
    pub pid: u32,
    /// A build process's working directory; `None` for any other process.
    pub cwd: Option<PathBuf>,
    /// Target dirs it names by env or `--target-dir`.
    pub target_dirs: Vec<PathBuf>,
    /// Absolute paths in its argv, and its executable.
    pub paths: Vec<PathBuf>,
}

impl ProcessFacts {
    fn uses(&self, target: &Path, parent: &Path) -> bool {
        self.cwd.as_ref().is_some_and(|c| c.starts_with(parent))
            || self
                .target_dirs
                .iter()
                .any(|d| d.starts_with(target) || target.starts_with(d))
            || self.paths.iter().any(|p| p.starts_with(target))
    }
}

/// The process table as the sweep sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessView {
    Seen(Vec<ProcessFacts>),
    /// Unreadable; the reason names what could not be read.
    Blind(String),
}

/// Whether running processes use one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessUse {
    Free,
    Used,
    /// Not known; kept.
    Unseen,
}

impl ProcessUse {
    const fn word(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Used => "used",
            Self::Unseen => "unseen",
        }
    }
}

/// The source of a [`ProcessView`].
pub trait ProcessTable {
    fn view(&self) -> ProcessView;
}

/// The host's process table, through `sysinfo`.
pub struct SystemProcessTable;

enum Read {
    Skip,
    Facts(ProcessFacts),
    NoOwner,
    Unreadable(String),
}

fn read_process(p: &sysinfo::Process, uid: &sysinfo::Uid) -> Read {
    let name = p.name().to_string_lossy();
    let build = is_build_process(&name);
    let pid = p.pid().as_u32();
    match p.user_id() {
        Some(u) if u == uid => {}
        Some(_) => return Read::Skip,
        None if build => return Read::NoOwner,
        None => return Read::Skip,
    }
    let exe = p.exe().map(Path::to_path_buf);
    if !build {
        return exe.map_or(Read::Skip, |exe| {
            Read::Facts(ProcessFacts {
                pid,
                paths: vec![canonical_or(exe)],
                ..ProcessFacts::default()
            })
        });
    }
    let Some(cwd) = p.cwd() else {
        return Read::Unreadable(format!("pid {pid} ({name}): working directory unreadable"));
    };
    if p.environ().is_empty() {
        return Read::Unreadable(format!("pid {pid} ({name}): environment unreadable"));
    }
    Read::Facts(facts_of(pid, cwd, p.environ(), p.cmd(), exe))
}

impl ProcessTable for SystemProcessTable {
    fn view(&self) -> ProcessView {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let kind = ProcessRefreshKind::nothing()
            .with_user(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .with_environ(UpdateKind::Always)
            .with_cwd(UpdateKind::Always);
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        let Ok(me) = sysinfo::get_current_pid() else {
            return ProcessView::Blind("own pid unreadable".into());
        };
        let Some(uid) = sys.process(me).and_then(|p| p.user_id()).cloned() else {
            return ProcessView::Blind("own user id unreadable".into());
        };
        let mut seen = Vec::new();
        let mut again = Vec::new();
        let mut ownerless = Vec::new();
        for (pid, p) in sys.processes() {
            match read_process(p, &uid) {
                Read::Skip => {}
                Read::Facts(f) => seen.push(f),
                Read::NoOwner => ownerless.push(pid.as_u32()),
                Read::Unreadable(_) => again.push(*pid),
            }
        }
        if !ownerless.is_empty() {
            let Some(owners) = ps_owners() else {
                return ProcessView::Blind(
                    "`ps` could not name the owners sysinfo could not".into(),
                );
            };
            if let Some(pid) = ownerless.iter().find(|p| owners.get(p) == Some(&*uid)) {
                return ProcessView::Blind(format!(
                    "pid {pid}: own build process, details unreadable"
                ));
            }
        }
        if again.is_empty() {
            return ProcessView::Seen(seen);
        }
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&again), true, kind);
        for pid in &again {
            let Some(p) = sys.process(*pid) else { continue };
            match read_process(p, &uid) {
                Read::Skip | Read::NoOwner => {}
                Read::Facts(f) => seen.push(f),
                Read::Unreadable(why) => return ProcessView::Blind(why),
            }
        }
        ProcessView::Seen(seen)
    }
}

/// pid → uid for every process, from `ps`, which reads the kernel's process
/// table and so names owners `sysinfo` cannot read (another user's process
/// on macOS). A pid missing here has exited. macOS prints `nobody` as uid
/// `-2`; it wraps to the kernel's 4294967294.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ps_owners() -> Option<std::collections::HashMap<u32, u32>> {
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,uid="])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| {
            let mut f = line.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let uid = f.next()?.parse::<i64>().ok()?;
            Some((pid, uid as u32))
        })
        .collect()
}

fn is_build_process(name: &str) -> bool {
    BUILD_PROCESS_NAMES.contains(&name)
        || BUILD_PROCESS_PREFIXES.iter().any(|p| name.starts_with(p))
}

fn canonical_or(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
}

/// A build process's facts from its raw cwd, environment, argv and exe.
#[must_use]
pub fn facts_of(
    pid: u32,
    cwd: &Path,
    environ: &[OsString],
    cmd: &[OsString],
    exe: Option<PathBuf>,
) -> ProcessFacts {
    let resolve = |s: &OsStr| {
        let p = Path::new(s);
        canonical_or(if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        })
    };
    let mut target_dirs: Vec<PathBuf> = environ
        .iter()
        .filter_map(|kv| {
            let kv = kv.to_string_lossy();
            let (k, v) = kv.split_once('=')?;
            (TARGET_DIR_ENV.contains(&k) && !v.is_empty()).then(|| resolve(OsStr::new(v)))
        })
        .collect();
    let mut paths: Vec<PathBuf> = exe.into_iter().map(canonical_or).collect();
    let mut flag_next = false;
    for arg in cmd {
        let text = arg.to_string_lossy();
        if flag_next {
            target_dirs.push(resolve(arg));
            flag_next = false;
        } else if text == TARGET_DIR_FLAG {
            flag_next = true;
        } else if let Some(v) = text.strip_prefix("--target-dir=") {
            target_dirs.push(resolve(OsStr::new(v)));
        } else if Path::new(arg).is_absolute() {
            paths.push(canonical_or(PathBuf::from(arg)));
        }
    }
    ProcessFacts {
        pid,
        cwd: Some(canonical_or(cwd.to_path_buf())),
        target_dirs,
        paths,
    }
}

/// Whether the processes in `view` use `dir`. A directory that cannot be
/// canonicalized cannot be compared, so it is [`ProcessUse::Unseen`].
#[must_use]
pub fn use_of(view: &ProcessView, dir: &Path) -> ProcessUse {
    let ProcessView::Seen(procs) = view else {
        return ProcessUse::Unseen;
    };
    let Ok(target) = std::fs::canonicalize(dir) else {
        return ProcessUse::Unseen;
    };
    let Some(parent) = target.parent() else {
        return ProcessUse::Unseen;
    };
    if procs.iter().any(|p| p.uses(&target, parent)) {
        ProcessUse::Used
    } else {
        ProcessUse::Free
    }
}

// ── measurement ────────────────────────────────────────────────────────

/// A recognized directory plus what the policy needs to know about it.
#[derive(Debug, Clone)]
pub struct Measured {
    pub repo: PathBuf,
    pub dir: CargoTargetDir,
    pub candidate: Candidate,
}

/// The policy's view of one directory — plain data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Allocated bytes (st_blocks x 512), hard links counted once — what
    /// removing it would actually free, which is what `du` reports.
    pub bytes: u64,
    /// Newest mtime of anything in the tree, the directory included.
    pub newest: SystemTime,
    /// A cargo lock file inside is held by a running build.
    pub lock_held: bool,
    /// What the process table says about it.
    pub process: ProcessUse,
}

/// Walk `dir` without following symlinks: size, newest mtime, lock state.
/// The process signal starts as [`ProcessUse::Unseen`]; [`sweep`] fills it.
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
            process: ProcessUse::Unseen,
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

// ── the policy, in blue ────────────────────────────────────────────────

const POLICY_SOURCE: &str = include_str!("../blue/cargo_target.b");

/// The numbers the policy applies.
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
    /// A running process builds in it or runs from it.
    ProcessUses,
    /// The process table could not be read.
    ProcessesUnseen,
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

    fn from_word(w: &str) -> Option<Self> {
        let active = |signal| Some(Self::Active { signal });
        match w {
            "in-use-lock" => active(ActiveSignal::LockHeld),
            "in-use-process" => active(ActiveSignal::ProcessUses),
            "in-use-unseen" => active(ActiveSignal::ProcessesUnseen),
            "in-use-recent" => active(ActiveSignal::RecentlyWritten),
            "keep" => Some(Self::Keep),
            "remove-idle" => Some(Self::RemoveIdle),
            "remove-over-budget" => Some(Self::RemoveOverBudget),
            _ => None,
        }
    }
}

/// The policy's answer, one verdict per candidate in input order.
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

fn age(c: &Candidate, now: SystemTime) -> Duration {
    now.duration_since(c.newest).unwrap_or(Duration::ZERO)
}

fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn uint(v: &Json) -> Result<u64> {
    v.as_u64()
        .ok_or_else(|| anyhow!("cargo_target.b: expected a count, got {v}"))
}

/// Ask `blue/cargo_target.b` for the plan, then refuse it if it removes a
/// directory any in-use signal holds.
///
/// # Errors
/// The policy failed to load or run, answered in a shape tend does not
/// read, or tried to remove a directory in use.
pub fn decide(
    cands: &[Candidate],
    policy: Policy,
    now: SystemTime,
    pressured: bool,
) -> Result<Plan> {
    let rows: Vec<Json> = cands
        .iter()
        .map(|c| {
            json!([
                int(c.bytes),
                int(age(c, now).as_secs()),
                c.lock_held,
                c.process.word()
            ])
        })
        .collect();
    let numbers = json!([
        int(policy.max_idle.as_secs()),
        int(policy.budget_bytes),
        int(policy.pressure_budget_bytes),
        int(policy.active_window.as_secs())
    ]);
    let out = crate::blue_policy::call(
        POLICY_SOURCE,
        "cargo_target.b",
        "decide",
        &[Json::Array(rows), numbers, json!(pressured)],
    )?;
    let parts = out
        .as_array()
        .filter(|p| p.len() == 5)
        .ok_or_else(|| anyhow!("cargo_target.b: decide() answered {out}"))?;
    let verdicts = parts[0]
        .as_array()
        .filter(|v| v.len() == cands.len())
        .ok_or_else(|| {
            anyhow!(
                "cargo_target.b: one verdict per directory, got {}",
                parts[0]
            )
        })?
        .iter()
        .map(|w| {
            w.as_str()
                .and_then(Verdict::from_word)
                .ok_or_else(|| anyhow!("cargo_target.b: unknown verdict {w}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let plan = Plan {
        verdicts,
        total_bytes: uint(&parts[1])?,
        after_bytes: uint(&parts[2])?,
        effective_budget_bytes: uint(&parts[3])?,
        budget_unreachable: parts[4]
            .as_bool()
            .ok_or_else(|| anyhow!("cargo_target.b: expected a flag, got {}", parts[4]))?,
    };
    guard(cands, &plan.verdicts, policy, now)?;
    Ok(plan)
}

/// The in-use rule, held in Rust whatever the policy answers.
fn guard(cands: &[Candidate], verdicts: &[Verdict], policy: Policy, now: SystemTime) -> Result<()> {
    for (c, v) in cands.iter().zip(verdicts) {
        let in_use =
            c.lock_held || c.process != ProcessUse::Free || age(c, now) < policy.active_window;
        if in_use && v.removes() {
            return Err(anyhow!(
                "cargo_target.b: refused a plan that removes a directory in use ({v:?})"
            ));
        }
    }
    Ok(())
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
/// Re-recognizes first, under the admission it was recognized with, then
/// takes an EXCLUSIVE non-blocking lock on every cargo lock file and holds
/// them across the delete: a build already running makes this refuse, and
/// one starting now waits instead of writing into a tree being deleted.
///
/// # Errors
/// See [`RemoveError`]. Nothing is deleted on any error before the delete
/// starts.
pub fn remove(dir: &CargoTargetDir) -> Result<(), RemoveError> {
    let dir = CargoTargetDir::recognize(dir.path(), dir.admission)
        .map_err(RemoveError::NoLongerRecognized)?;
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
    /// The repo, or the extra root, it was found under.
    pub repo: String,
    pub path: String,
    pub form: Form,
    pub process: ProcessUse,
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

/// A candidate directory that was not recognized.
#[derive(Debug, Clone, Serialize)]
pub struct Refused {
    pub path: String,
    pub reason: NotCargoTarget,
}

/// What the sweep could see of the process table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "sight")]
pub enum Sight {
    Seen { processes: usize },
    Blind { why: String },
}

/// The whole sweep, typed — what the CLI prints and the MCP tool returns.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub mode: Mode,
    pub pressured: bool,
    pub extra_roots: Vec<String>,
    pub processes: Sight,
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
pub(crate) fn to_gib(b: u64) -> f64 {
    b as f64 / GIB as f64
}

/// Discover, measure, decide, and in [`Mode::Apply`] remove.
///
/// `cfg.dry_run` forces a dry run whatever `mode` says: config can switch
/// deletion off, a flag can never switch it back on.
///
/// # Errors
/// Only the policy's (see [`decide`]); nothing is removed then.
pub fn sweep(
    cfg: &CargoTargetConfig,
    repos: &[PathBuf],
    pressured: bool,
    mode: Mode,
    now: SystemTime,
    procs: &dyn ProcessTable,
) -> Result<Report> {
    let mode = if cfg.dry_run { Mode::DryRun } else { mode };
    let admission = cfg.admission();
    let roots = cfg.extra_root_paths();
    let mut found = discover(repos, cfg.search_depth.max(1), admission);
    let extra = discover_roots(&roots, admission);
    found.dirs.extend(extra.dirs);
    found.refused.extend(extra.refused);
    found.dirs.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    found.dirs.dedup_by(|a, b| a.1.path == b.1.path);
    found.refused.sort();
    found.refused.dedup();

    let view = procs.view();
    let mut measured = Vec::new();
    let mut unmeasurable = Vec::new();
    for (repo, dir) in found.dirs {
        let shown = dir.path().display().to_string();
        match measure(&repo, dir) {
            Ok(mut m) => {
                m.candidate.process = use_of(&view, m.dir.path());
                measured.push(m);
            }
            Err(e) => unmeasurable.push(shown + ": " + &e.to_string()),
        }
    }

    let cands: Vec<Candidate> = measured.iter().map(|m| m.candidate).collect();
    let plan = decide(&cands, cfg.policy(), now, pressured)?;

    let mut freed: u64 = 0;
    let mut entries = Vec::with_capacity(measured.len());
    for (m, v) in measured.iter().zip(&plan.verdicts) {
        let idle = age(&m.candidate, now).as_secs_f64() / 86_400.0;
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
            form: m.dir.form(),
            process: m.candidate.process,
            bytes: m.candidate.bytes,
            gib: to_gib(m.candidate.bytes),
            idle_days: (idle * 10.0).round() / 10.0,
            verdict: *v,
            removed,
            error,
        });
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.bytes));

    Ok(Report {
        mode,
        pressured,
        extra_roots: roots.iter().map(|r| r.display().to_string()).collect(),
        processes: match view {
            ProcessView::Seen(ps) => Sight::Seen {
                processes: ps.len(),
            },
            ProcessView::Blind(why) => Sight::Blind { why },
        },
        count: entries.len(),
        total_gib: to_gib(plan.total_bytes),
        reclaim_gib: to_gib(plan.total_bytes.saturating_sub(plan.after_bytes)),
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
    })
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
                signal: ActiveSignal::ProcessUses,
            } => "in use (running process)",
            Self::Active {
                signal: ActiveSignal::ProcessesUnseen,
            } => "in use (processes unseen)",
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
    let schedule = match (cfg.enable, cfg.auto_apply) {
        (true, true) => "",
        (true, false) => "  [pressure relief off]",
        (false, true) => "  [scheduled sweep off; pressure relief on]",
        (false, false) => "  [daemon sweep off]",
    };
    println!(
        "policy        max_idle_days {}  budget {} GiB (pressure {} GiB)  active {} min  adopt_untagged {}{}",
        cfg.max_idle_days,
        cfg.budget_gib,
        cfg.pressure_budget_gib,
        cfg.active_minutes,
        cfg.adopt_untagged,
        schedule
    );
    if !r.extra_roots.is_empty() {
        println!("extra roots   {}", r.extra_roots.join(", "));
    }
    match &r.processes {
        Sight::Seen { processes } => println!("processes     {processes} seen"),
        Sight::Blind { why } => println!("processes     BLIND ({why}): every dir is kept"),
    }
    println!("{:>8}  {:>9}  {:<28}  path", "GiB", "idle days", "verdict");
    for e in &r.entries {
        let outcome = match (e.removed, &e.error) {
            (Some(true), _) => "  [removed]".to_string(),
            (Some(false), Some(err)) => "  [FAILED: ".to_string() + err + "]",
            _ => String::new(),
        };
        let form = match e.form {
            Form::Named => "",
            Form::Layout => "  [by layout]",
            Form::Adopted => "  [adopted, untagged]",
        };
        println!(
            "{:>8.2}  {:>9.1}  {:<28}  {}{}{}",
            e.gib,
            e.idle_days,
            e.verdict.label(),
            e.path,
            form,
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
    const RUSTC_INFO_JSON: &str = r#"{"rustc_fingerprint":8263317859797211930,"outputs":{}}"#;

    const STRICT: Admission = Admission { untagged: false };
    const ADOPT: Admission = Admission { untagged: true };

    struct Fixed(ProcessView);

    impl ProcessTable for Fixed {
        fn view(&self) -> ProcessView {
            self.0.clone()
        }
    }

    fn nobody() -> Fixed {
        Fixed(ProcessView::Seen(Vec::new()))
    }

    /// `<root>/<repo>/target` with a cargo tag and a `debug/` holding `bytes`.
    fn cargo_target(root: &Path, repo: &str, bytes: usize) -> PathBuf {
        let t = root.join(repo).join("target");
        std::fs::create_dir_all(t.join("debug")).unwrap();
        std::fs::write(t.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        std::fs::write(t.join("debug/.cargo-lock"), "").unwrap();
        std::fs::write(t.join("debug/blob"), vec![7u8; bytes]).unwrap();
        t
    }

    /// cargo's layout at `dir`: `.rustc_info.json` and `debug/.fingerprint/`.
    fn layout(dir: &Path) {
        std::fs::create_dir_all(dir.join("debug/.fingerprint")).unwrap();
        std::fs::write(dir.join(RUSTC_INFO), RUSTC_INFO_JSON).unwrap();
        std::fs::write(dir.join("debug/blob"), vec![7u8; 4096]).unwrap();
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
            process: ProcessUse::Free,
        }
    }

    fn reap_all() -> CargoTargetConfig {
        CargoTargetConfig {
            max_idle_days: 1,
            active_minutes: 0,
            ..CargoTargetConfig::default()
        }
    }

    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(30 * 86_400)
    }

    // ── recognition: the safety invariant ─────────────────────────────

    #[test]
    fn a_cargo_tagged_target_is_recognized() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        assert_eq!(
            CargoTargetDir::recognize(&t, STRICT).unwrap().form(),
            Form::Named
        );
    }

    #[test]
    fn a_target_without_a_tag_is_refused() {
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        std::fs::create_dir_all(&t).unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&t, ADOPT),
            Err(NotCargoTarget::NoCachedirTag)
        );
    }

    #[test]
    fn a_non_cargo_cachedir_tag_is_refused() {
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        std::fs::create_dir_all(&t).unwrap();
        std::fs::write(
            t.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# created by virtualenv\n",
        )
        .unwrap();
        layout(&t);
        assert_eq!(
            CargoTargetDir::recognize(&t, ADOPT),
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
            CargoTargetDir::recognize(&t, STRICT),
            Err(NotCargoTarget::ForeignTag)
        );
    }

    #[test]
    fn a_tagged_dir_with_another_name_needs_cargos_layout() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let other = tmp.path().join("agent-target");
        std::fs::rename(&t, &other).unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&other, ADOPT),
            Err(NotCargoTarget::NoCargoLayout)
        );
        layout(&other);
        assert_eq!(
            CargoTargetDir::recognize(&other, STRICT).unwrap().form(),
            Form::Layout
        );
    }

    #[test]
    fn another_name_without_a_tag_is_never_adopted() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path().join("agent-target");
        layout(&d);
        assert_eq!(
            CargoTargetDir::recognize(&d, ADOPT),
            Err(NotCargoTarget::WrongName)
        );
    }

    #[test]
    fn layout_needs_both_the_rustc_info_and_a_fingerprint_dir() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path().join("x-target");
        std::fs::create_dir_all(d.join("debug")).unwrap();
        std::fs::write(d.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        std::fs::write(d.join(RUSTC_INFO), RUSTC_INFO_JSON).unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&d, STRICT),
            Err(NotCargoTarget::NoCargoLayout),
            "no .fingerprint/"
        );
        std::fs::create_dir_all(d.join("wasm32-unknown-unknown/release/.fingerprint")).unwrap();
        assert!(
            CargoTargetDir::recognize(&d, STRICT).is_ok(),
            "<triple>/<profile>"
        );
        std::fs::write(d.join(RUSTC_INFO), r#"{"outputs":{}}"#).unwrap();
        assert_eq!(
            CargoTargetDir::recognize(&d, STRICT),
            Err(NotCargoTarget::NoCargoLayout),
            "a .rustc_info.json with no rustc_fingerprint"
        );
    }

    #[test]
    fn an_untagged_target_with_cargos_layout_is_reported_then_adopted_on_request() {
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        layout(&t);
        assert_eq!(
            CargoTargetDir::recognize(&t, STRICT),
            Err(NotCargoTarget::UntaggedCargoLayout)
        );
        assert_eq!(
            CargoTargetDir::recognize(&t, ADOPT).unwrap().form(),
            Form::Adopted
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
            CargoTargetDir::recognize(&link, ADOPT),
            Err(NotCargoTarget::Symlink)
        );
        let found = discover(&[tmp.path().join("r")], 2, ADOPT);
        assert!(found.dirs.is_empty());
        assert_eq!(found.refused, vec![(link, NotCargoTarget::Symlink)]);
        assert!(real.join("debug/blob").exists());
    }

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
            adopt_untagged: true,
            ..CargoTargetConfig::default()
        };
        let far_future = SystemTime::now() + Duration::from_secs(365 * 86_400);
        let r = sweep(
            &cfg,
            &[tmp.path().join("maven")],
            true,
            Mode::Apply,
            far_future,
            &nobody(),
        )
        .unwrap();
        assert_eq!(r.count, 0);
        assert_eq!(r.refused.len(), 1);
        assert_eq!(r.refused[0].reason, NotCargoTarget::NoCachedirTag);
        assert!(t.join("app.jar").exists());
    }

    #[test]
    fn remove_re_recognizes_before_deleting() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let dir = CargoTargetDir::recognize(&t, STRICT).unwrap();
        std::fs::remove_file(t.join("CACHEDIR.TAG")).unwrap();
        assert!(matches!(
            remove(&dir),
            Err(RemoveError::NoLongerRecognized(
                NotCargoTarget::NoCachedirTag
            ))
        ));
        assert!(t.join("debug/blob").exists());
    }

    #[test]
    fn remove_re_recognizes_an_adopted_dir_by_its_layout() {
        let tmp = TempDir::new().unwrap();
        let t = tmp.path().join("r/target");
        layout(&t);
        let dir = CargoTargetDir::recognize(&t, ADOPT).unwrap();
        std::fs::remove_file(t.join(RUSTC_INFO)).unwrap();
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

        let d1 = discover(std::slice::from_ref(&root), 1, STRICT);
        assert_eq!(d1.dirs.len(), 1, "depth 1 = <repo>/target only");

        let d2 = discover(std::slice::from_ref(&root), 2, STRICT);
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

    #[test]
    fn extra_roots_find_cargo_dirs_of_any_name_one_level_down() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let agent = cache.join("integrate-target");
        layout(&agent);
        std::fs::write(agent.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        let tag_only = cache.join("half-target");
        std::fs::create_dir_all(&tag_only).unwrap();
        std::fs::write(tag_only.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        let pip = cache.join("pip");
        std::fs::create_dir_all(&pip).unwrap();
        std::fs::write(
            pip.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# pip\n",
        )
        .unwrap();
        let deep = cache.join("nested/deep-target");
        layout(&deep);
        std::fs::write(deep.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        std::os::unix::fs::symlink(&agent, cache.join("link-target")).unwrap();

        let found = discover_roots(std::slice::from_ref(&cache), STRICT);
        let dirs: Vec<_> = found
            .dirs
            .iter()
            .map(|(_, d)| d.path().to_path_buf())
            .collect();
        assert_eq!(dirs, vec![agent]);
        assert_eq!(
            found.refused,
            vec![(tag_only, NotCargoTarget::NoCargoLayout)]
        );
    }

    #[test]
    fn extra_root_paths_expand_the_home_and_drop_blanks() {
        let cfg = CargoTargetConfig {
            extra_roots: vec!["~/.cache".into(), "  ".into(), "/abs".into()],
            ..CargoTargetConfig::default()
        };
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            cfg.extra_root_paths(),
            vec![home.join(".cache"), PathBuf::from("/abs")]
        );
    }

    // ── measurement ───────────────────────────────────────────────────

    #[test]
    fn measure_counts_hard_links_once() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 64 * 1024);
        let one = CargoTargetDir::recognize(&t, STRICT).unwrap();
        let before = measure(tmp.path(), one).unwrap().candidate.bytes;
        std::fs::hard_link(t.join("debug/blob"), t.join("debug/blob-link")).unwrap();
        let two = CargoTargetDir::recognize(&t, STRICT).unwrap();
        let after = measure(tmp.path(), two).unwrap().candidate.bytes;
        assert_eq!(before, after, "a hard link frees nothing extra");
        assert!(before >= 64 * 1024);
    }

    #[test]
    fn measure_starts_the_process_signal_unseen() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let m = measure(tmp.path(), CargoTargetDir::recognize(&t, STRICT).unwrap()).unwrap();
        assert_eq!(m.candidate.process, ProcessUse::Unseen);
    }

    #[test]
    fn a_held_cargo_lock_reads_as_in_use() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let lock = File::open(t.join("debug/.cargo-lock")).unwrap();
        lock.lock().unwrap();

        let m = measure(tmp.path(), CargoTargetDir::recognize(&t, STRICT).unwrap()).unwrap();
        assert!(m.candidate.lock_held);
        assert!(matches!(remove(&m.dir), Err(RemoveError::BecameActive)));
        assert!(t.exists(), "a locked target must survive removal");

        drop(lock);
        let m = measure(tmp.path(), CargoTargetDir::recognize(&t, STRICT).unwrap()).unwrap();
        assert!(!m.candidate.lock_held);
        remove(&m.dir).unwrap();
        assert!(!t.exists());
    }

    // ── processes ─────────────────────────────────────────────────────

    #[test]
    fn a_blind_process_table_leaves_every_dir_unseen() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        assert_eq!(
            use_of(&ProcessView::Blind("no table".into()), &t),
            ProcessUse::Unseen
        );
        assert_eq!(
            use_of(&ProcessView::Seen(Vec::new()), &tmp.path().join("gone")),
            ProcessUse::Unseen,
            "a dir that cannot be canonicalized cannot be compared"
        );
    }

    #[test]
    fn a_build_in_the_repo_or_naming_the_dir_uses_it() {
        let tmp = TempDir::new().unwrap();
        let repo_target = cargo_target(tmp.path(), "repo", 10);
        let agent = tmp.path().join("cache/x-target");
        layout(&agent);
        let other = cargo_target(tmp.path(), "other", 10);
        let repo = std::fs::canonicalize(tmp.path().join("repo")).unwrap();

        let in_repo = facts_of(
            1,
            &repo.join("crates/a"),
            &[OsString::from("HOME=/h")],
            &[],
            None,
        );
        let by_env = facts_of(
            2,
            Path::new("/"),
            &[OsString::from(format!(
                "CARGO_TARGET_DIR={}",
                agent.display()
            ))],
            &[],
            None,
        );
        let view = ProcessView::Seen(vec![in_repo, by_env]);
        assert_eq!(use_of(&view, &repo_target), ProcessUse::Used);
        assert_eq!(use_of(&view, &agent), ProcessUse::Used);
        assert_eq!(use_of(&view, &other), ProcessUse::Free);
    }

    #[test]
    fn target_dir_flags_and_argv_paths_are_read() {
        let tmp = TempDir::new().unwrap();
        let cwd = std::fs::canonicalize(tmp.path()).unwrap();
        let ra = cwd.join("w/target/rust-analyzer");
        std::fs::create_dir_all(&ra).unwrap();
        let f = facts_of(
            3,
            &cwd.join("w"),
            &[OsString::from("CARGO_BUILD_TARGET_DIR=rel")],
            &[
                OsString::from("cargo"),
                OsString::from("--target-dir"),
                OsString::from("target/rust-analyzer"),
                OsString::from("--target-dir=/abs/t"),
                OsString::from("/abs/t/debug/deps/x.rlib"),
            ],
            None,
        );
        assert_eq!(
            f.target_dirs,
            vec![cwd.join("w/rel"), ra.clone(), PathBuf::from("/abs/t")]
        );
        assert_eq!(f.paths, vec![PathBuf::from("/abs/t/debug/deps/x.rlib")]);
        let rust_analyzer_child = ProcessView::Seen(vec![ProcessFacts {
            target_dirs: vec![ra],
            ..ProcessFacts::default()
        }]);
        assert_eq!(
            use_of(&rust_analyzer_child, &cwd.join("w/target")),
            ProcessUse::Used,
            "a build into target/rust-analyzer holds target/"
        );
    }

    #[test]
    fn only_build_processes_count_by_working_directory() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 10);
        let shell = ProcessFacts {
            pid: 9,
            cwd: None,
            target_dirs: Vec::new(),
            paths: vec![PathBuf::from("/bin/zsh")],
        };
        assert_eq!(
            use_of(&ProcessView::Seen(vec![shell]), &t),
            ProcessUse::Free
        );
        assert!(is_build_process("cargo"));
        assert!(is_build_process("build-script-build"));
        assert!(is_build_process("rust-analyzer-proc-macro-srv"));
        assert!(!is_build_process("zsh"));
        assert!(!is_build_process("cargokit"));
    }

    #[test]
    fn ps_names_the_owner_of_this_process() {
        match ps_owners() {
            Some(owners) => assert!(owners.contains_key(&std::process::id())),
            None => assert!(
                std::process::Command::new("ps").arg("-p1").output().is_err(),
                "ps runs here, so ps_owners must read it"
            ),
        }
    }

    #[test]
    fn the_host_table_sees_this_test_running_from_its_target_dir() {
        let view = SystemProcessTable.view();
        let ProcessView::Seen(_) = &view else {
            return;
        };
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().and_then(Path::parent).unwrap();
        assert_eq!(use_of(&view, profile_dir), ProcessUse::Used);
    }

    // ── the policy ────────────────────────────────────────────────────

    fn plan(cands: &[Candidate], pressured: bool) -> Plan {
        decide(cands, policy(), now(), pressured).unwrap()
    }

    #[test]
    fn idle_past_the_limit_is_removed_and_fresh_is_kept() {
        let p = plan(&[cand(10, 30), cand(10, 3)], false);
        assert_eq!(p.verdicts, vec![Verdict::RemoveIdle, Verdict::Keep]);
        assert_eq!(p.after_bytes, 10);
    }

    #[test]
    fn exactly_at_the_idle_limit_is_kept() {
        assert_eq!(plan(&[cand(10, 14)], false).verdicts, vec![Verdict::Keep]);
    }

    #[test]
    fn over_budget_removes_least_recently_used_first() {
        let p = plan(&[cand(50, 2), cand(50, 9), cand(50, 5)], false);
        assert_eq!(
            p.verdicts,
            vec![Verdict::Keep, Verdict::RemoveOverBudget, Verdict::Keep]
        );
        assert_eq!(p.after_bytes, 100);
        assert!(!p.budget_unreachable);
    }

    #[test]
    fn idle_removals_count_toward_the_budget() {
        let p = plan(&[cand(80, 30), cand(60, 2)], false);
        assert_eq!(p.verdicts, vec![Verdict::RemoveIdle, Verdict::Keep]);
    }

    #[test]
    fn pressure_tightens_the_budget() {
        let cands = [cand(20, 1), cand(20, 2), cand(20, 3)];
        assert!(plan(&cands, false)
            .verdicts
            .iter()
            .all(|v| *v == Verdict::Keep));
        let tight = plan(&cands, true);
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
    fn every_in_use_signal_keeps_a_dir_idle_and_over_budget() {
        let mut locked = cand(500, 90);
        locked.lock_held = true;
        let mut used = cand(500, 90);
        used.process = ProcessUse::Used;
        let mut unseen = cand(500, 90);
        unseen.process = ProcessUse::Unseen;
        let recent = Candidate {
            newest: now() - Duration::from_secs(600),
            ..cand(500, 0)
        };
        let p = plan(&[locked, used, unseen, recent], true);
        let active = |signal| Verdict::Active { signal };
        assert_eq!(
            p.verdicts,
            vec![
                active(ActiveSignal::LockHeld),
                active(ActiveSignal::ProcessUses),
                active(ActiveSignal::ProcessesUnseen),
                active(ActiveSignal::RecentlyWritten),
            ]
        );
        assert!(p.budget_unreachable, "reported, not resolved by deleting");
    }

    #[test]
    fn a_future_mtime_reads_as_in_use_not_idle() {
        let skewed = Candidate {
            newest: now() + Duration::from_secs(86_400 * 400),
            ..cand(10, 0)
        };
        assert_eq!(
            plan(&[skewed], false).verdicts,
            vec![Verdict::Active {
                signal: ActiveSignal::RecentlyWritten
            }]
        );
    }

    #[test]
    fn rust_refuses_a_plan_that_removes_a_dir_in_use() {
        let mut used = cand(10, 90);
        used.process = ProcessUse::Unseen;
        assert!(guard(&[used], &[Verdict::RemoveIdle], policy(), now()).is_err());
        assert!(guard(&[cand(10, 90)], &[Verdict::RemoveIdle], policy(), now()).is_ok());
    }

    // ── the sweep ─────────────────────────────────────────────────────

    #[test]
    fn dry_run_decides_but_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 4096);
        let r = sweep(
            &reap_all(),
            &[tmp.path().join("r")],
            false,
            Mode::DryRun,
            later(),
            &nobody(),
        )
        .unwrap();
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
            dry_run: true,
            ..reap_all()
        };
        let r = sweep(
            &cfg,
            &[tmp.path().join("r")],
            false,
            Mode::Apply,
            later(),
            &nobody(),
        )
        .unwrap();
        assert_eq!(r.mode, Mode::DryRun);
        assert!(t.exists());
    }

    #[test]
    fn apply_removes_idle_dirs_whole() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 4096);
        let r = sweep(
            &reap_all(),
            &[tmp.path().join("r")],
            false,
            Mode::Apply,
            later(),
            &nobody(),
        )
        .unwrap();
        assert_eq!(r.entries[0].removed, Some(true));
        assert!(r.freed_gib > 0.0);
        assert!(!t.exists());
        assert!(
            tmp.path().join("r").exists(),
            "only target/ goes, never the repo"
        );
    }

    #[test]
    fn a_blind_sweep_removes_nothing() {
        let tmp = TempDir::new().unwrap();
        let t = cargo_target(tmp.path(), "r", 4096);
        let blind = Fixed(ProcessView::Blind(
            "pid 7 (cargo): environment unreadable".into(),
        ));
        let r = sweep(
            &reap_all(),
            &[tmp.path().join("r")],
            true,
            Mode::Apply,
            later(),
            &blind,
        )
        .unwrap();
        assert_eq!(
            r.entries[0].verdict,
            Verdict::Active {
                signal: ActiveSignal::ProcessesUnseen
            }
        );
        assert!(matches!(r.processes, Sight::Blind { .. }));
        assert!(t.exists());
    }

    #[test]
    fn apply_reaps_an_idle_agent_cache_and_keeps_the_one_a_build_uses() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        let idle = cache.join("old-target");
        let busy = cache.join("busy-target");
        for d in [&idle, &busy] {
            layout(d);
            std::fs::write(d.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        }
        let cfg = CargoTargetConfig {
            extra_roots: vec![cache.display().to_string()],
            ..reap_all()
        };
        let build = Fixed(ProcessView::Seen(vec![facts_of(
            4,
            Path::new("/"),
            &[OsString::from(format!(
                "CARGO_TARGET_DIR={}",
                busy.display()
            ))],
            &[],
            None,
        )]));
        let r = sweep(&cfg, &[], false, Mode::Apply, later(), &build).unwrap();
        assert_eq!(r.count, 2);
        assert!(!idle.exists());
        assert!(busy.exists());
        assert!(r.entries.iter().all(|e| e.form == Form::Layout));
    }

    #[test]
    fn config_defaults_and_partial_blocks() {
        let d = CargoTargetConfig::default();
        assert!(!d.enable, "the scheduled sweep is opt-in");
        assert!(d.auto_apply, "pressure relief is on unless switched off");
        assert!(!d.adopt_untagged);
        assert!(d.extra_roots.is_empty());
        assert_eq!(d.max_idle_days, 14);
        assert_eq!(d.budget_gib, 50);
        assert_eq!(d.pressure_budget_gib, 10);
        assert_eq!(d.active_minutes, 60);
        assert_eq!(d.interval_minutes, 60);
        assert_eq!(d.pressure_interval_minutes, 5);
        assert_eq!(d.search_depth, 2);
        assert!(!d.dry_run);

        let partial: CargoTargetConfig = serde_yaml_ng::from_str(
            "enable: true\nbudget_gib: 40\nextra_roots: [\"~/.cache\"]\nauto_apply: false\n",
        )
        .unwrap();
        assert!(partial.enable);
        assert!(!partial.auto_apply);
        assert_eq!(partial.budget_gib, 40);
        assert_eq!(partial.extra_roots, vec!["~/.cache".to_string()]);
        assert_eq!(partial.max_idle_days, 14);
    }
}
