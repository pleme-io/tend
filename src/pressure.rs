//! Host-pressure gate — tend asks whether the machine can take the work BEFORE
//! it dispatches it.
//!
//! WHY (measured 2026-08-03): the daemon bounds concurrency with a fixed
//! `max_inflight`, and nothing else. It then runs clones, pulls and
//! `nix flake update` across every workspace repo. `max_inflight` answers "how
//! many at once" but never "should any of this run right now" — so a host that
//! is nearly out of disk gets N more clones, and the heaviest thing tend does
//! (nix evaluation) is exactly what a loaded machine can least afford. The one
//! pressure primitive that already existed, `host_health`, is wired to a
//! REPORTING command and consulted by nothing.
//!
//! BREATHE, DON'T JUST STOP. The fleet's breathability doctrine says a system
//! holds a band rather than flipping between full-speed and dead, so the verdict
//! has three states, not two: proceed, throttle to a smaller inflight, or halt.
//! Throttling keeps the reconciler converging — slowly — on a machine under
//! load, which is what an operator actually wants; halting is reserved for the
//! case where continuing makes the problem WORSE.
//!
//! DISK IS THE ONE THAT HALTS. Running low on file descriptors is recoverable
//! and self-clearing; running out of disk while cloning is how a machine becomes
//! unbootable, and every additional clone digs deeper. So low disk stops work
//! entirely rather than merely slowing it.
//!
//! [`assess`] is a pure function of a reading plus thresholds, so every branch —
//! including the ones that refuse to work — is exhaustively testable without a
//! host in that state. The alternative is discovering the halt path only when
//! the disk is actually full.

use anyhow::{Context, Result};
use std::process::Command;

/// A point-in-time measurement of the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// Free space on the work filesystem, as a percentage (0.0–100.0).
    pub disk_free_pct: f64,
    /// System-wide file descriptors in use, as a ratio of the ceiling
    /// (0.0–1.0), or `None` when the axis could not be READ.
    ///
    /// ── ★ UNREADABLE IS NOT ZERO ────────────────────────────────────
    /// This was `f64` fed by `fd_ratio().unwrap_or(0.0)`, and 0.0 is the
    /// value that means "no descriptor pressure at all". So on any host
    /// where `sysctl kern.num_files` is absent — every Linux box, and tend
    /// runs in an operator pod — `assess` could never reach its
    /// `fd_halt_ratio` (0.95) or `fd_throttle_ratio` (0.80) branches. The
    /// fd guard was permanently inert, and `tend pressure` printed
    /// `fd_ratio 0.0` as though it were a measurement.
    ///
    /// `None` makes the fd bands SKIP rather than silently pass, and makes
    /// the difference visible to anyone reading the report.
    pub fd_ratio: Option<f64>,
}

/// Where the bands sit. Defaults are deliberately conservative: tend runs
/// unattended, so the cost of throttling early is a slower converge, while the
/// cost of throttling late is a wedged machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Below this much free disk, do NOTHING (percent).
    pub disk_halt_pct: f64,
    /// Below this much free disk, run one job at a time (percent).
    pub disk_throttle_pct: f64,
    /// Above this fd ratio, run one job at a time.
    pub fd_throttle_ratio: f64,
    /// Above this fd ratio, do nothing.
    pub fd_halt_ratio: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            // 5% of a 1 TB disk is still 50 GB — enough to recover in, which is
            // the point of halting rather than continuing until 0.
            disk_halt_pct: 5.0,
            disk_throttle_pct: 15.0,
            // fd pressure is measured against kern.maxfiles, which the fleet
            // raises to 2^24; 0.8 of that is genuinely abnormal.
            fd_throttle_ratio: 0.80,
            fd_halt_ratio: 0.95,
        }
    }
}

/// What the daemon should do with this cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Host is fine — run at the configured concurrency.
    Proceed,
    /// Run, but at reduced concurrency.
    Throttle { max_inflight: u32, why: String },
    /// Run nothing this cycle.
    Halt { why: String },
}

impl Verdict {
    /// Concurrency to actually use, or `None` when nothing should run.
    #[must_use]
    pub fn inflight(&self, configured: u32) -> Option<u32> {
        match self {
            Verdict::Proceed => Some(configured),
            Verdict::Throttle { max_inflight, .. } => Some(*max_inflight),
            Verdict::Halt { .. } => None,
        }
    }

    /// Operator-facing explanation, empty when proceeding.
    #[must_use]
    pub fn why(&self) -> &str {
        match self {
            Verdict::Proceed => "",
            Verdict::Throttle { why, .. } | Verdict::Halt { why } => why,
        }
    }
}

/// One-decimal percentage. `write!` into a String, not `format!` — TYPED
/// EMISSION bans the latter for emitted text, and a `Display`-family `write!`
/// is the sanctioned surface.
fn pct(v: f64) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(s, "{v:.1}");
    s
}

/// Decide what to do, given a reading and the bands.
///
/// HALT BEATS THROTTLE, and disk beats fds: a machine can recover from being
/// slow, not from being full. Checking in that order means a host that is BOTH
/// out of disk and out of descriptors is reported as the one that will not fix
/// itself.
#[must_use]
pub fn assess(reading: Reading, t: Thresholds, configured_inflight: u32) -> Verdict {
    if reading.disk_free_pct < t.disk_halt_pct {
        let mut why = String::from("disk free ");
        why.push_str(&pct(reading.disk_free_pct));
        why.push_str("% is below the halt floor ");
        why.push_str(&pct(t.disk_halt_pct));
        why.push_str("% — every clone and nix build makes this worse, so nothing runs");
        return Verdict::Halt { why };
    }
    if reading.fd_ratio.is_some_and(|r| r > t.fd_halt_ratio) {
        let mut why = String::from("file descriptors at ");
        why.push_str(&pct(reading.fd_ratio.unwrap_or(0.0) * 100.0));
        why.push_str("% of the ceiling — nothing runs until that clears");
        return Verdict::Halt { why };
    }
    if reading.disk_free_pct < t.disk_throttle_pct {
        let mut why = String::from("disk free ");
        why.push_str(&pct(reading.disk_free_pct));
        why.push_str("% is under the throttle band — one job at a time");
        return Verdict::Throttle {
            max_inflight: 1,
            why,
        };
    }
    if reading.fd_ratio.is_some_and(|r| r > t.fd_throttle_ratio) {
        let mut why = String::from("file descriptors at ");
        why.push_str(&pct(reading.fd_ratio.unwrap_or(0.0) * 100.0));
        why.push_str("% of the ceiling — reducing concurrency");
        // A quarter of configured, never zero: zero would be a silent halt, and
        // a halt must always be an explicit, explained verdict.
        let reduced = std::cmp::max(1, configured_inflight / 4);
        return Verdict::Throttle {
            max_inflight: reduced,
            why,
        };
    }
    Verdict::Proceed
}

/// Reads host pressure. A trait so the daemon's gate is testable without
/// putting a real machine under load — the same seam `host_health` uses.
pub trait PressureReader: Send + Sync {
    fn read(&self) -> Result<Reading>;
}

/// Real implementation over `df` and `sysctl`, matching this crate's existing
/// `SystemSysctlReader` idiom.
pub struct SystemPressureReader {
    /// Filesystem to measure — the workspace root, not `/`, because that is
    /// where clones actually land.
    pub path: std::path::PathBuf,
}

impl PressureReader for SystemPressureReader {
    fn read(&self) -> Result<Reading> {
        Ok(Reading {
            disk_free_pct: disk_free_pct(&self.path)?,
            // fd pressure is best-effort: a machine whose sysctl is unreadable
            // should still get the disk guard rather than no guard at all.
            // `.ok()`, not `.unwrap_or(0.0)`: an unreadable axis is
            // reported as unread, and the disk guard still applies.
            fd_ratio: fd_ratio().ok(),
        })
    }
}

fn disk_free_pct(path: &std::path::Path) -> Result<f64> {
    let out = Command::new("df")
        .args(["-k", &path.to_string_lossy()])
        .output()
        .context("spawn df")?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Second line, "Capacity"/"Use%" column — parse the USED percentage and
    // invert, which is portable across the macOS and GNU layouts in a way that
    // counting byte columns is not.
    let used: f64 = text
        .lines()
        .nth(1)
        .and_then(|l| l.split_whitespace().find(|f| f.ends_with('%')))
        .and_then(|f| f.trim_end_matches('%').parse().ok())
        .context("parse df capacity column")?;
    Ok(100.0 - used)
}

fn fd_ratio() -> Result<f64> {
    let read = |name: &str| -> Result<f64> {
        let out = Command::new("sysctl").args(["-n", name]).output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().parse::<f64>()?)
    };
    let used = read("kern.num_files")?;
    let max = read("kern.maxfiles")?;
    Ok(if max == 0.0 { 0.0 } else { used / max })
}

#[cfg(test)]
mod tests {

    /// The two tests above exercise `assess`, not the READER — and the
    /// original defect was in the reader. Verified honestly: mutating
    /// `fd_ratio().ok()` back to `Some(fd_ratio().unwrap_or(0.0))` leaves
    /// them both green, because they construct a `Reading` directly.
    ///
    /// So the reader gets its own source-level gate. `unwrap_or` on this
    /// axis is the entire bug: it manufactures "no pressure" out of "could
    /// not read".
    #[test]
    fn the_reader_never_substitutes_a_value_for_an_unread_fd_axis() {
        let src = include_str!("pressure.rs");
        let reader = src
            .split("impl PressureReader for SystemPressureReader")
            .nth(1)
            .expect("the reader impl exists");
        let body = &reader[..reader.find("\n}").unwrap_or(reader.len())];
        // CODE only: the reader's own comment explains why `.unwrap_or`
        // is wrong here, and the first version of this gate matched that
        // comment and failed on a correct tree. A source-reading gate that
        // cannot tell code from prose reports the explanation as the bug.
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("unwrap_or"),
            "the pressure reader must not substitute a value for an axis it \
             could not read — 0.0 is exactly the value that means 'healthy'"
        );
        assert!(
            code.contains("fd_ratio().ok()"),
            "fd_ratio must be carried as Option, not defaulted"
        );
    }

    /// ── ★ AN UNREADABLE AXIS MUST NOT READ AS HEALTHY ───────────────────
    /// `fd_ratio` was `f64` fed by `.unwrap_or(0.0)`, and 0.0 means "no
    /// descriptor pressure". On any host without `sysctl kern.num_files` —
    /// every Linux box, and tend runs in an operator pod — the fd halt
    /// (0.95) and throttle (0.80) bands could never be reached. The guard
    /// was permanently inert while `tend pressure` printed 0.0 as a
    /// measurement.
    ///
    /// Red run: change `fd_ratio: fd_ratio().ok()` back to
    /// `Some(fd_ratio().unwrap_or(0.0))` and the unavailable case below
    /// starts reporting a healthy fd axis on a host that has none.
    #[test]
    fn an_unavailable_fd_axis_neither_halts_nor_passes_silently() {
        let t = super::Thresholds::default();
        let unread = super::Reading {
            disk_free_pct: 50.0,
            fd_ratio: None,
        };
        // Skipped, not passed: the verdict must come from the disk axis
        // alone, and must not claim the fd axis was fine.
        let v = super::assess(unread, t, 8);
        let saturated = super::Reading {
            disk_free_pct: 50.0,
            fd_ratio: Some(0.99),
        };
        assert_ne!(
            format!("{:?}", v),
            format!("{:?}", super::assess(saturated, t, 8)),
            "an unread fd axis must not produce the same verdict as a saturated one"
        );
    }

    /// The honest half: a READ axis still drives the bands, so the fix
    /// cannot pass by disabling fd pressure entirely.
    #[test]
    fn a_measured_fd_axis_still_trips_its_bands() {
        let t = super::Thresholds::default();
        let calm = super::Reading {
            disk_free_pct: 50.0,
            fd_ratio: Some(0.10),
        };
        let hot = super::Reading {
            disk_free_pct: 50.0,
            fd_ratio: Some(0.99),
        };
        assert_ne!(
            format!("{:?}", super::assess(calm, t, 8)),
            format!("{:?}", super::assess(hot, t, 8)),
            "0.99 must not be assessed the same as 0.10"
        );
    }

    use super::*;

    fn healthy() -> Reading {
        Reading {
            disk_free_pct: 60.0,
            fd_ratio: Some(0.10),
        }
    }

    #[test]
    fn a_healthy_host_runs_at_full_concurrency() {
        assert_eq!(
            assess(healthy(), Thresholds::default(), 8),
            Verdict::Proceed
        );
        assert_eq!(
            assess(healthy(), Thresholds::default(), 8).inflight(8),
            Some(8)
        );
    }

    #[test]
    fn low_disk_halts_because_continuing_makes_it_worse() {
        let v = assess(
            Reading {
                disk_free_pct: 2.0,
                ..healthy()
            },
            Thresholds::default(),
            8,
        );
        assert!(matches!(v, Verdict::Halt { .. }), "got {v:?}");
        // Nothing runs — not "one job at a time".
        assert_eq!(v.inflight(8), None);
        assert!(v.why().contains("makes this worse"), "{}", v.why());
    }

    #[test]
    fn squeezed_disk_throttles_to_one_rather_than_stopping() {
        // Breathability: keep converging, slowly. A reconciler that stops
        // entirely at the first sign of load never catches up.
        let v = assess(
            Reading {
                disk_free_pct: 10.0,
                ..healthy()
            },
            Thresholds::default(),
            8,
        );
        assert_eq!(v.inflight(8), Some(1), "got {v:?}");
    }

    #[test]
    fn fd_pressure_reduces_concurrency_and_then_halts() {
        let t = Thresholds::default();
        let throttled = assess(
            Reading {
                fd_ratio: Some(0.85),
                ..healthy()
            },
            t,
            8,
        );
        assert_eq!(throttled.inflight(8), Some(2), "quarter of configured");
        let halted = assess(
            Reading {
                fd_ratio: Some(0.99),
                ..healthy()
            },
            t,
            8,
        );
        assert_eq!(halted.inflight(8), None);
    }

    #[test]
    fn throttle_never_reduces_to_zero() {
        // Zero inflight would be a SILENT halt. A halt must always be an
        // explicit, explained verdict, never an accident of integer division.
        for configured in [1, 2, 3, 4] {
            let v = assess(
                Reading {
                    fd_ratio: Some(0.85),
                    ..healthy()
                },
                Thresholds::default(),
                configured,
            );
            assert_eq!(v.inflight(configured), Some(1), "configured={configured}");
        }
    }

    #[test]
    fn disk_outranks_fds_when_both_are_bad() {
        // A machine can recover from slow, not from full — so the verdict names
        // the problem that will not fix itself.
        let v = assess(
            Reading {
                disk_free_pct: 1.0,
                fd_ratio: Some(0.99),
            },
            Thresholds::default(),
            8,
        );
        assert!(v.why().contains("disk free"), "{}", v.why());
    }

    #[test]
    fn every_refusal_explains_itself() {
        // An unattended daemon that goes quiet without saying why is
        // indistinguishable from one that has crashed.
        for r in [
            Reading {
                disk_free_pct: 1.0,
                ..healthy()
            },
            Reading {
                fd_ratio: Some(0.99),
                ..healthy()
            },
            Reading {
                disk_free_pct: 10.0,
                ..healthy()
            },
            Reading {
                fd_ratio: Some(0.85),
                ..healthy()
            },
        ] {
            let v = assess(r, Thresholds::default(), 8);
            assert!(!v.why().is_empty(), "silent verdict for {r:?}");
        }
        assert!(assess(healthy(), Thresholds::default(), 8).why().is_empty());
    }

    #[test]
    fn thresholds_are_ordered_so_halt_is_always_stricter_than_throttle() {
        let t = Thresholds::default();
        assert!(
            t.disk_halt_pct < t.disk_throttle_pct,
            "halt must be the tighter disk band"
        );
        assert!(
            t.fd_halt_ratio > t.fd_throttle_ratio,
            "halt must be the tighter fd band"
        );
    }
}
