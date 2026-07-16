//! Host resource-hygiene guard surfaced on `tend status`.
//!
//! Mirrors the 2026-07-12 fd-exhaustion incident on `ryn`: 62 pre-fix
//! `frost` processes (PPID == 1 -- reparented to init, no owning terminal
//! session) accumulated silently, each holding ~25k fds, until the kernel's
//! system-wide file table hit its ceiling and unrelated tools (cargo, bash,
//! ld) started failing with `ENFILE`. Nothing watched for the class itself;
//! it was discovered only once something else broke. This module makes two
//! things visible on the routine `tend status` pass instead, both logged to
//! `~/.local/share/tend/audit.jsonl` so a recurring pattern is diagnosable
//! from history, not just from whatever is running right now:
//!
//! - **Named orphans** -- processes matching a configured watchlist
//!   (`host_health.watched_commands`, default `["frost"]`) with PPID == 1.
//!   Confirmed-orphan is a narrow, safe-to-reap class (no session owns them
//!   any more), so by default (`host_health.fix`, default true) they are
//!   SIGKILLed and the outcome logged -- detection alone would rediscover
//!   the same processes on every run without fixing anything.
//! - **System-wide fd pressure** -- `kern.num_files / kern.maxfiles` versus
//!   a configured threshold (default 0.5), independent of any specific
//!   process. This is detect-only: there is no safe universal remediation
//!   for "fd usage is high" beyond the named-orphan reap above, since which
//!   *other* processes are safe to touch is not knowable in general.
//!
//! Tier (UNREPRESENTABILITY ledger): only-mitigated, C4 (external-world
//! observation + a runtime clamp). Neither check makes its failure mode
//! unrepresentable by construction -- `tend` does not supervise how `frost`
//! (or any watched tool) spawns its PTY children, and no type can bound how
//! many fds the live kernel table has left. The real seal against orphaning
//! lives inside `frost` itself (`install_signal_traps`, the 2026-07-10 fix);
//! this module is the fleet-wide, cross-tool detection-and-reap layer on
//! top of that, nothing stronger.
//!
//! Orphan signal: PPID == 1 is the primary, empirically-validated gate (the
//! confirmed signature of the actual incident). `tty` is carried as
//! corroborating diagnostic context (an orphaned test/dev process has no
//! controlling terminal, shown as `??`) but is not required for the gate --
//! requiring both would risk false negatives for no proven benefit over
//! PPID alone.

use anyhow::{Context, Result};
use std::process::Command;

use crate::audit::AuditLog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedProcess {
    pub pid: u32,
    pub etime: String,
    pub tty: String,
    pub command: String,
}

/// Abstracts the system process table so the orphan-detection logic is
/// unit-testable without shelling out.
pub trait ProcessLister: Send + Sync {
    /// Returns one line per process: `pid ppid etime tty comm`.
    fn list(&self) -> Result<String>;
}

pub struct SystemProcessLister;

impl ProcessLister for SystemProcessLister {
    fn list(&self) -> Result<String> {
        let output = Command::new("ps")
            .args(["-eo", "pid=,ppid=,etime=,tty=,comm="])
            .output()
            .context("spawn ps -eo pid=,ppid=,etime=,tty=,comm=")?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Processes whose command matches one of `name_fragments` and whose PPID
/// is 1 -- reparented to init, meaning the session that owned them is gone.
/// A live, still-attached test/dev process is never PPID == 1, so this
/// cannot flag normal in-progress work.
pub fn find_orphaned(
    lister: &dyn ProcessLister,
    name_fragments: &[&str],
) -> Result<Vec<OrphanedProcess>> {
    let raw = lister.list()?;
    let mut found = Vec::new();
    for line in raw.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid_s), Some(ppid_s), Some(etime), Some(tty)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        let command: String = fields.collect::<Vec<_>>().join(" ");
        if command.is_empty() || ppid_s != "1" {
            continue;
        }
        if !name_fragments.iter().any(|f| command.contains(f)) {
            continue;
        }
        if let Ok(pid) = pid_s.parse::<u32>() {
            found.push(OrphanedProcess {
                pid,
                etime: etime.to_string(),
                tty: tty.to_string(),
                command,
            });
        }
    }
    Ok(found)
}

/// Runs the check and appends one audit event per orphan found.
pub fn find_and_log_orphaned(
    lister: &dyn ProcessLister,
    name_fragments: &[&str],
    audit: &AuditLog,
) -> Result<Vec<OrphanedProcess>> {
    let found = find_orphaned(lister, name_fragments)?;
    for o in &found {
        audit.orphaned_process_detected(o.pid, &o.etime, &o.tty, &o.command);
    }
    Ok(found)
}

/// Abstracts sending SIGKILL so the reap logic is unit-testable.
pub trait ProcessKiller: Send + Sync {
    /// Attempts SIGKILL; returns `(exit_success, stderr)` verbatim so the
    /// caller classifies the outcome the same way for real and fake impls.
    fn kill9(&self, pid: u32) -> Result<(bool, String)>;
}

pub struct SystemProcessKiller;

impl ProcessKiller for SystemProcessKiller {
    fn kill9(&self, pid: u32) -> Result<(bool, String)> {
        let output = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .context("spawn kill -9")?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapOutcome {
    /// SIGKILL succeeded.
    Killed,
    /// The process was already gone by the time we tried -- not a failure,
    /// just a race between detection and reap (matches what actually
    /// happened manually on 2026-07-12: all 7 targeted pids had already
    /// exited by the time `kill -9` ran).
    AlreadyGone,
    /// SIGKILL failed for some other reason (permissions, etc).
    Failed(String),
}

impl ReapOutcome {
    fn label(&self) -> &'static str {
        match self {
            ReapOutcome::Killed => "killed",
            ReapOutcome::AlreadyGone => "already_gone",
            ReapOutcome::Failed(_) => "failed",
        }
    }
}

/// SIGKILLs every given orphan and logs one audit event per outcome. Safe
/// to call on an empty slice. Only ever called on processes that already
/// passed the `find_orphaned` gate (PPID==1 + watchlist match) -- never on
/// an arbitrary pid.
pub fn reap_orphaned(
    killer: &dyn ProcessKiller,
    orphans: &[OrphanedProcess],
    audit: &AuditLog,
) -> Result<Vec<(OrphanedProcess, ReapOutcome)>> {
    let mut results = Vec::with_capacity(orphans.len());
    for o in orphans {
        let (success, stderr) = killer.kill9(o.pid)?;
        let outcome = if success {
            ReapOutcome::Killed
        } else if stderr.to_lowercase().contains("no such process") {
            ReapOutcome::AlreadyGone
        } else {
            ReapOutcome::Failed(stderr.trim().to_string())
        };
        audit.orphaned_process_reaped(o.pid, &o.command, outcome.label());
        results.push((o.clone(), outcome));
    }
    Ok(results)
}

/// Abstracts reading a `sysctl` value so fd-pressure logic is unit-testable.
pub trait SysctlReader: Send + Sync {
    fn read(&self, name: &str) -> Result<u64>;
}

pub struct SystemSysctlReader;

impl SysctlReader for SystemSysctlReader {
    fn read(&self, name: &str) -> Result<u64> {
        let output = Command::new("sysctl")
            .args(["-n", name])
            .output()
            .with_context(|| format!("spawn sysctl -n {name}"))?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .with_context(|| format!("parse sysctl -n {name} output"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FdPressure {
    pub used: u64,
    pub max: u64,
}

impl FdPressure {
    #[must_use]
    pub fn ratio(&self) -> f64 {
        if self.max == 0 {
            0.0
        } else {
            self.used as f64 / self.max as f64
        }
    }
}

/// Reads system-wide fd usage against the kernel ceiling. Detect-only --
/// there is no safe generic remediation for "fd usage is high" (which
/// *other* processes are safe to touch is not knowable here).
pub fn check_fd_pressure(sysctl: &dyn SysctlReader) -> Result<FdPressure> {
    Ok(FdPressure {
        used: sysctl.read("kern.num_files")?,
        max: sysctl.read("kern.maxfiles")?,
    })
}

/// Everything one `tend status` pass learned about host resource health.
pub struct HostHealthReport {
    pub orphans: Vec<OrphanedProcess>,
    pub reap_outcomes: Vec<(OrphanedProcess, ReapOutcome)>,
    pub fd_pressure: Option<FdPressure>,
    pub fd_pressure_exceeded: bool,
}

/// The single entrypoint `tend status` calls: detect named orphans (and
/// reap them if `fix`), then check system-wide fd pressure. Individual
/// step failures (e.g. `sysctl` missing on a non-Darwin host) degrade to
/// "no signal" rather than failing the whole `tend status` invocation --
/// this is best-effort extra information, not the core status contract.
pub fn run_host_health_check(
    lister: &dyn ProcessLister,
    killer: &dyn ProcessKiller,
    sysctl: &dyn SysctlReader,
    watched_commands: &[String],
    fd_pressure_threshold: f64,
    fix: bool,
    audit: &AuditLog,
) -> HostHealthReport {
    let fragments: Vec<&str> = watched_commands.iter().map(String::as_str).collect();
    let orphans = find_and_log_orphaned(lister, &fragments, audit).unwrap_or_default();
    let reap_outcomes = if fix && !orphans.is_empty() {
        reap_orphaned(killer, &orphans, audit).unwrap_or_default()
    } else {
        Vec::new()
    };
    let fd_pressure = check_fd_pressure(sysctl).ok();
    let fd_pressure_exceeded = fd_pressure
        .map(|p| p.ratio() >= fd_pressure_threshold)
        .unwrap_or(false);
    if fd_pressure_exceeded {
        if let Some(p) = fd_pressure {
            audit.fd_pressure_detected(p.used, p.max, p.ratio());
        }
    }
    HostHealthReport {
        orphans,
        reap_outcomes,
        fd_pressure,
        fd_pressure_exceeded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeLister(&'static str);
    impl ProcessLister for FakeLister {
        fn list(&self) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    fn temp_audit() -> (std::path::PathBuf, AuditLog) {
        let dir = std::env::temp_dir().join(format!(
            "tend-host-health-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("audit.jsonl");
        (dir, AuditLog::new(path))
    }

    const FROST: &[&str] = &["frost"];

    #[test]
    fn flags_orphaned_watched_command() {
        let lister = FakeLister("76677 1 01:14:22 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n");
        let found = find_orphaned(&lister, FROST).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 76677);
        assert_eq!(found[0].etime, "01:14:22");
        assert_eq!(found[0].tty, "??");
    }

    #[test]
    fn ignores_non_orphaned_watched_command() {
        let lister =
            FakeLister("76677 54321 00:00:41 ttys007 /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n");
        assert!(find_orphaned(&lister, FROST).unwrap().is_empty());
    }

    #[test]
    fn ignores_orphaned_unwatched_command() {
        let lister = FakeLister("999 1 02:00:00 ?? /usr/bin/some-other-tool\n");
        assert!(find_orphaned(&lister, FROST).unwrap().is_empty());
    }

    #[test]
    fn handles_multiple_lines_and_malformed_rows() {
        let lister = FakeLister(
            "1 0 10:00:00 ?? /sbin/launchd\n\
             76677 1 01:14:22 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n\
             garbage-row-too-short\n\
             76678 1 00:05:00 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n",
        );
        let found = find_orphaned(&lister, FROST).unwrap();
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn empty_process_table_yields_no_orphans() {
        let lister = FakeLister("");
        assert!(find_orphaned(&lister, FROST).unwrap().is_empty());
    }

    #[test]
    fn configurable_watchlist_covers_a_second_pattern() {
        let lister = FakeLister("42 1 00:00:10 ?? /usr/local/bin/some-other-pty-tool\n");
        let watchlist = &["frost", "some-other-pty-tool"];
        let found = find_orphaned(&lister, watchlist).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn find_and_log_appends_one_audit_event_per_orphan() {
        use std::io::BufRead;
        let lister = FakeLister(
            "76677 1 01:14:22 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n\
             76678 1 00:05:00 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n",
        );
        let (dir, audit) = temp_audit();
        let found = find_and_log_orphaned(&lister, FROST, &audit).unwrap();
        assert_eq!(found.len(), 2);

        let file = std::fs::File::open(audit.path()).unwrap();
        let lines: Vec<String> = std::io::BufReader::new(file)
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["event"], "orphaned_process_detected");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct FakeKiller {
        /// pids that should report "no such process" instead of succeeding.
        already_gone: Vec<u32>,
    }
    impl ProcessKiller for FakeKiller {
        fn kill9(&self, pid: u32) -> Result<(bool, String)> {
            if self.already_gone.contains(&pid) {
                Ok((false, format!("kill: {pid}: No such process")))
            } else {
                Ok((true, String::new()))
            }
        }
    }

    #[test]
    fn reap_classifies_killed_and_already_gone() {
        let orphans = vec![
            OrphanedProcess {
                pid: 1,
                etime: "00:01:00".into(),
                tty: "??".into(),
                command: "frost".into(),
            },
            OrphanedProcess {
                pid: 2,
                etime: "00:02:00".into(),
                tty: "??".into(),
                command: "frost".into(),
            },
        ];
        let killer = FakeKiller {
            already_gone: vec![2],
        };
        let (dir, audit) = temp_audit();
        let results = reap_orphaned(&killer, &orphans, &audit).unwrap();
        assert_eq!(results[0].1, ReapOutcome::Killed);
        assert_eq!(results[1].1, ReapOutcome::AlreadyGone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reap_on_empty_slice_is_a_noop() {
        let killer = FakeKiller {
            already_gone: vec![],
        };
        let (dir, audit) = temp_audit();
        let results = reap_orphaned(&killer, &[], &audit).unwrap();
        assert!(results.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct FakeSysctl(std::collections::HashMap<&'static str, u64>);
    impl SysctlReader for FakeSysctl {
        fn read(&self, name: &str) -> Result<u64> {
            self.0
                .get(name)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unknown sysctl {name}"))
        }
    }

    #[test]
    fn fd_pressure_ratio_computed_correctly() {
        let sysctl = FakeSysctl(
            [("kern.num_files", 16_776_780u64), ("kern.maxfiles", 16_777_216u64)]
                .into_iter()
                .collect(),
        );
        let pressure = check_fd_pressure(&sysctl).unwrap();
        assert!((pressure.ratio() - 0.99997).abs() < 0.0001);
    }

    #[test]
    fn fd_pressure_below_threshold_is_healthy() {
        let sysctl = FakeSysctl(
            [("kern.num_files", 8_047u64), ("kern.maxfiles", 16_777_216u64)]
                .into_iter()
                .collect(),
        );
        let pressure = check_fd_pressure(&sysctl).unwrap();
        assert!(pressure.ratio() < 0.5);
    }

    #[test]
    fn run_host_health_check_reaps_by_default_and_logs_everything() {
        let lister = FakeLister("76677 1 01:14:22 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n");
        let killer = FakeKiller {
            already_gone: vec![76677],
        };
        let sysctl = FakeSysctl(
            [("kern.num_files", 8_047u64), ("kern.maxfiles", 16_777_216u64)]
                .into_iter()
                .collect(),
        );
        let (dir, audit) = temp_audit();
        let report = run_host_health_check(
            &lister,
            &killer,
            &sysctl,
            &["frost".to_string()],
            0.5,
            true,
            &audit,
        );
        assert_eq!(report.orphans.len(), 1);
        assert_eq!(report.reap_outcomes.len(), 1);
        assert_eq!(report.reap_outcomes[0].1, ReapOutcome::AlreadyGone);
        assert!(!report.fd_pressure_exceeded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_host_health_check_skips_reap_when_fix_disabled() {
        let lister = FakeLister("76677 1 01:14:22 ?? /nix/store/ka03l-rust_frost-0.1.0/bin/frost\n");
        let killer = FakeKiller {
            already_gone: vec![],
        };
        let sysctl = FakeSysctl(
            [("kern.num_files", 8_047u64), ("kern.maxfiles", 16_777_216u64)]
                .into_iter()
                .collect(),
        );
        let (dir, audit) = temp_audit();
        let report = run_host_health_check(
            &lister,
            &killer,
            &sysctl,
            &["frost".to_string()],
            0.5,
            false,
            &audit,
        );
        assert_eq!(report.orphans.len(), 1);
        assert!(report.reap_outcomes.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
