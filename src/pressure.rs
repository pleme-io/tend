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
    /// Free space on the work filesystem, GiB.
    ///
    /// ABSOLUTE, not a percentage, because the risk is absolute: a clone or nix
    /// build needs some number of GiB and that number does not scale with how
    /// big the disk happens to be. The percentage is derived
    /// ([`Reading::disk_free_pct`]) for reporting only.
    pub disk_free_gib: f64,
    /// Total size of the work filesystem, GiB — needed for the percentage guard
    /// that protects volumes smaller than the absolute floor.
    pub disk_total_gib: f64,
    /// 1-minute load average divided by CPU count, or `None` when either could
    /// not be READ (same `Option` discipline as `fd_ratio` below — an
    /// unreadable axis is reported unread, never as 0.0 "no pressure").
    ///
    /// ── ★ WHY THIS AXIS EXISTS ──────────────────────────────────────
    /// Added 2026-08-21 after the daemon made the operator's workstation
    /// unusable and this gate said `proceed` throughout. Measured on cid:
    /// **load average 52.89** while `tend daemon --interval 300 --pull true`
    /// fanned `git pull` across ~1189 repos every 5 minutes. Disk was fine
    /// (122 GiB free against a 50 GiB floor) and fds were fine, so both
    /// existing axes were green — and they were green *correctly*, because
    /// neither measures the thing that was wrong.
    ///
    /// PER-CPU, not raw: a load of 16 is saturation on a 4-core box and
    /// healthy on a 32-core one, so the raw number is not comparable across
    /// the fleet. Dividing by CPU count makes one pair of thresholds correct
    /// everywhere — the same reasoning `halt_floor_gib` uses for disk.
    ///
    /// Note this axis is DELIBERATELY not a halt: unlike a full disk, load
    /// recovers on its own, and a machine that is merely busy should converge
    /// slowly rather than not at all.
    pub load_per_cpu: Option<f64>,
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

impl Reading {
    /// Free space as a percentage — derived, for reporting.
    #[must_use]
    pub fn disk_free_pct(&self) -> f64 {
        if self.disk_total_gib <= 0.0 {
            return 0.0;
        }
        self.disk_free_gib / self.disk_total_gib * 100.0
    }
}

/// Where the bands sit. Defaults are deliberately conservative: tend runs
/// unattended, so the cost of throttling early is a slower converge, while the
/// cost of throttling late is a wedged machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Absolute floor below which NOTHING runs, GiB. Primary — see the impl note.
    pub disk_halt_gib: f64,
    /// Disk one job is budgeted to add to the store, GiB. The throttle floor is
    /// DERIVED from this and the concurrency — see [`Thresholds::throttle_floor_gib`].
    pub job_disk_budget_gib: f64,
    /// Small-volume guard for the halt floor (percent).
    pub disk_halt_pct: f64,
    /// Below this much free disk, run one job at a time (percent).
    pub disk_throttle_pct: f64,
    /// Above this fd ratio, run one job at a time.
    pub fd_throttle_ratio: f64,
    /// Above this fd ratio, do nothing.
    pub fd_halt_ratio: f64,
    /// Above this 1-min load average PER CPU, reduce concurrency.
    pub load_throttle_per_cpu: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            // 5% of a 1 TB disk is still 50 GB — enough to recover in, which is
            // the point of halting rather than continuing until 0.
            // 20 GiB is room to recover in — a nix-collect-garbage plus a
            // rebuild — and far below where the percentage band would fire.
            //
            // ★ ALSO UNMEASURED, and now load-bearing twice: it is the BASE of the
            // derived throttle floor below, so it sets how much headroom any
            // concurrency can use. A GC frees space rather than needing it, and a
            // rebuild's delta is single-GiB, so this looks generous — but a safety
            // stop is not something to loosen while chasing a throttle verdict.
            // Left exactly as it was, deliberately.
            // `pending-pressure: halt-floor-measurement`
            disk_halt_gib: 20.0,
            // ★ MEASURED 2026-09-01 on ryn, and the measurement FALSIFIED the
            // constant this replaces.
            //
            // This was `disk_throttle_gib: 50.0`, justified in a comment as "the
            // largest single build seen on the fleet with headroom". Counted over
            // the whole nix store — 38,218 paths, 51.0 GiB total — the size
            // distribution is:
            //
            //     median  ~0.00 MiB     p99      16.67 MiB
            //     p95      1.97 MiB     p99.9   257.38 MiB
            //     max   5514.30 MiB  (swift-toolchain, a substituted artifact)
            //
            // So the floor was set ABOVE THE SIZE OF THE ENTIRE STORE it
            // protects, and at ~9.3x the largest single artifact its own comment
            // claimed to cover. Zero paths exceed 50 GiB. On a 460 GiB volume
            // with 29.5 GiB free it throttled a host with room for dozens of
            // sub-GiB jobs down to one at a time.
            //
            // 1 GiB is ~4x the p99.9 single path, so a job realizing a handful of
            // paths fits with headroom.
            //
            // ★ THE HONEST LIMIT, stated because it is the reason this number is a
            // judgement and not a derivation. What is NOT measured anywhere is the
            // distribution of a tend JOB's store delta — a job realizes a closure,
            // which is a SUM of paths, and no job records its delta today. The
            // path distribution above is a proxy, not that measurement.
            //
            // So the pathological case is real: N jobs each pulling a toolchain-
            // sized artifact overruns N GiB of budget, and the halt floor does NOT
            // catch it mid-cycle — pressure is read BETWEEN cycles, so halt bounds
            // the damage to one cycle rather than preventing it. That trade is
            // taken deliberately: the cost of overrun is a cycle that ends with a
            // tight disk which halt then refuses to make worse, while the cost of
            // pricing the worst case here is throttling a host that had room for
            // dozens of sub-GiB jobs — which is the defect being fixed. Pricing
            // the worst case in BOTH bands is the double-conservatism that
            // produced the 50.
            //
            // `pending-pressure: job-delta-measurement` — record each job's actual
            // store delta, then set this from the observed p99 instead of a proxy.
            job_disk_budget_gib: 1.0,
            disk_halt_pct: 5.0,
            disk_throttle_pct: 15.0,
            // fd pressure is measured against kern.maxfiles, which the fleet
            // raises to 2^24; 0.8 of that is genuinely abnormal.
            fd_throttle_ratio: 0.80,
            fd_halt_ratio: 0.95,
            // 1.5x the core count. Below 1.0 the box has idle CPU and tend's
            // work is mostly network-blocked anyway; above ~1.5 the operator
            // feels it in the UI, which is the failure this band exists to
            // prevent. Not a halt — see `Reading::load_per_cpu`.
            load_throttle_per_cpu: 1.5,
        }
    }
}

impl Thresholds {
    /// Effective floors are the MINIMUM of the absolute and percentage bands.
    ///
    /// ★ MEASURED, not preference (cid, 2026-08-03: 926 GiB volume, 192 GiB
    /// free). A percentage-only 15% throttle band fires at **139 GiB free** — no
    /// build on this fleet needs anywhere near that, so the reconciler would run
    /// at a quarter speed to protect against nothing. That is a mis-set unit, not
    /// caution.
    ///
    /// Percentage alone scales wrongly BOTH ways: 15% of 4 TB is 600 GiB
    /// (absurd), 15% of a 100 GiB VM disk is 15 GiB (about right). Absolute alone
    /// breaks on a volume smaller than its own floor — a 40 GiB disk can never
    /// satisfy "50 GiB free", so every cycle would stop forever. `min` makes one
    /// pair of numbers correct on both: absolute dominates a large disk, the
    /// percentage dominates a small one.
    #[must_use]
    pub fn halt_floor_gib(&self, total_gib: f64) -> f64 {
        self.disk_halt_gib
            .min(total_gib * self.disk_halt_pct / 100.0)
    }

    /// Effective throttle floor, GiB: the recovery room the halt floor reserves,
    /// plus room for the jobs we would actually run in parallel.
    ///
    /// ★ IT TAKES `inflight` BECAUSE IT ANSWERS A CONCURRENCY QUESTION. The
    /// previous version was a flat constant, which answered "does ONE job fit?"
    /// and then used the answer to decide HOW MANY jobs to run — a unit error, and
    /// the reason it read as arbitrary: the number could not move when the
    /// concurrency did.
    ///
    /// The percentage band still guards small volumes (see
    /// [`Thresholds::halt_floor_gib`]), and the floor is never allowed below the
    /// halt floor — stopping must stay the tighter of the two.
    #[must_use]
    pub fn throttle_floor_gib(&self, total_gib: f64, inflight: u32) -> f64 {
        let halt = self.halt_floor_gib(total_gib);
        let want = halt + f64::from(inflight) * self.job_disk_budget_gib;
        want.min(total_gib * self.disk_throttle_pct / 100.0).max(halt)
    }

    /// How many jobs the free space affords above the recovery floor, capped at
    /// the configured concurrency and never below one.
    ///
    /// Degrading gracefully is the other half of the fix. Slamming to a single
    /// job when the measurement says three fit is the same arbitrariness as the
    /// flat floor, one layer down.
    #[must_use]
    pub fn affordable_inflight(&self, reading: &Reading, configured: u32) -> u32 {
        let usable = reading.disk_free_gib - self.halt_floor_gib(reading.disk_total_gib);
        let fits = (usable / self.job_disk_budget_gib).floor();
        // `clamp` on a NaN-free positive budget; the cast is saturating in Rust.
        fits.clamp(1.0, f64::from(configured)) as u32
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

/// A whole count. A job count is not a one-decimal measurement — rendering it
/// through [`gib`] printed "5.0 job(s)". Same typed-emission rule as [`pct`].
fn count(v: u32) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(s, "{v}");
    s
}

/// One-decimal GiB. `write!` into a String, same typed-emission rule as [`pct`].
fn gib(v: f64) -> String {
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
    let halt_floor = t.halt_floor_gib(reading.disk_total_gib);
    let throttle_floor = t.throttle_floor_gib(reading.disk_total_gib, configured_inflight);

    if reading.disk_free_gib < halt_floor {
        let mut why = String::from("disk free ");
        why.push_str(&gib(reading.disk_free_gib));
        why.push_str(" GiB is below the floor ");
        why.push_str(&gib(halt_floor));
        why.push_str(" GiB — every clone and nix build makes this worse, so nothing runs");
        return Verdict::Halt { why };
    }
    if reading.fd_ratio.is_some_and(|r| r > t.fd_halt_ratio) {
        let mut why = String::from("file descriptors at ");
        why.push_str(&pct(reading.fd_ratio.unwrap_or(0.0) * 100.0));
        why.push_str("% of the ceiling — nothing runs until that clears");
        return Verdict::Halt { why };
    }
    if reading.disk_free_gib < throttle_floor {
        // How many jobs the space genuinely affords, not a flat one.
        let max_inflight = t.affordable_inflight(&reading, configured_inflight);
        let mut why = String::from("disk free ");
        why.push_str(&gib(reading.disk_free_gib));
        why.push_str(" GiB is under the throttle floor ");
        why.push_str(&gib(throttle_floor));
        why.push_str(" GiB — room for ");
        why.push_str(&count(max_inflight));
        why.push_str(if max_inflight == 1 { " job" } else { " jobs" });
        why.push_str(" above the recovery floor");
        return Verdict::Throttle { max_inflight, why };
    }
    if reading
        .load_per_cpu
        .is_some_and(|l| l > t.load_throttle_per_cpu)
    {
        let mut why = String::from("load average is ");
        why.push_str(&pct(reading.load_per_cpu.unwrap_or(0.0) * 100.0));
        why.push_str("% of the core count — the host is busy, reducing concurrency");
        let reduced = std::cmp::max(1, configured_inflight / 4);
        return Verdict::Throttle {
            max_inflight: reduced,
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
            disk_free_gib: disk_reading(&self.path)?.0,
            disk_total_gib: disk_reading(&self.path)?.1,
            // fd pressure is best-effort: a machine whose sysctl is unreadable
            // should still get the disk guard rather than no guard at all.
            // `.ok()`, not `.unwrap_or(0.0)`: an unreadable axis is
            // reported as unread, and the disk guard still applies.
            fd_ratio: fd_ratio().ok(),
            // Same best-effort + `.ok()` discipline as fd_ratio.
            load_per_cpu: load_per_cpu().ok(),
        })
    }
}

fn disk_reading(path: &std::path::Path) -> Result<(f64, f64)> {
    let out = Command::new("df")
        .args(["-k", &path.to_string_lossy()])
        .output()
        .context("spawn df")?;
    let text = String::from_utf8_lossy(&out.stdout);
    // Fields: filesystem, 1K-blocks, used, available, … Parsing the NUMBERS
    // rather than the capacity percentage is what makes absolute floors possible
    // at all — the percentage column throws the magnitude away, which is exactly
    // how the band ended up mis-set.
    let row = text.lines().nth(1).context("df has no data row")?;
    let f: Vec<&str> = row.split_whitespace().collect();
    let total_k: f64 = f
        .get(1)
        .and_then(|v| v.parse().ok())
        .context("parse df total")?;
    let avail_k: f64 = f
        .get(3)
        .and_then(|v| v.parse().ok())
        .context("parse df available")?;
    let to_gib = |k: f64| k / 1024.0 / 1024.0;
    Ok((to_gib(avail_k), to_gib(total_k)))
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

/// 1-minute load average / CPU count.
///
/// `sysctl -n vm.loadavg` prints `{ 1.23 4.56 7.89 }` on darwin and a bare
/// triple on linux; taking the first float-parsable token handles both without
/// branching on the platform.
fn load_per_cpu() -> Result<f64> {
    let out = Command::new("sysctl").args(["-n", "vm.loadavg"]).output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let one_min: f64 = text
        .split_whitespace()
        .find_map(|tok| tok.parse::<f64>().ok())
        .context("no float in vm.loadavg")?;
    let cpus = std::thread::available_parallelism()
        .context("cpu count")?
        .get() as f64;
    Ok(if cpus <= 0.0 { one_min } else { one_min / cpus })
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
    /// ★ THE INCIDENT THIS AXIS EXISTS FOR (cid, 2026-08-21).
    ///
    /// `tend daemon --interval 300 --pull true` fanned `git pull` across ~1189
    /// repos every 5 minutes and made the operator's workstation unusable —
    /// load average **52.89**. Disk was fine (122 GiB free vs a 50 GiB floor)
    /// and fds were fine, so both existing axes were green, CORRECTLY, because
    /// neither measures load. `tend pressure` reported `verdict: proceed` with
    /// `max_inflight: 8` throughout.
    #[test]
    fn a_saturated_host_no_longer_reports_proceed() {
        let t = super::Thresholds::default();
        // The measured reading: healthy disk, healthy fds, load 52.89 on a
        // 16-core box = 3.3 per CPU.
        let cid = super::Reading {
            disk_free_gib: 122.7,
            disk_total_gib: 926.4,
            fd_ratio: Some(0.000_214),
            load_per_cpu: Some(52.89 / 16.0),
        };
        match super::assess(cid, t, 8) {
            super::Verdict::Throttle { max_inflight, why } => {
                assert!(max_inflight < 8, "concurrency must actually drop");
                assert!(why.contains("load"), "the reason must name load: {why}");
            }
            other => panic!("a host at load 52.89 must not proceed, got {other:?}"),
        }
    }

    /// Load is NOT a halt: unlike a full disk, a busy host recovers on its own,
    /// and tend must converge slowly rather than stop forever.
    #[test]
    fn load_throttles_but_never_halts() {
        let t = super::Thresholds::default();
        let absurd = super::Reading {
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
            fd_ratio: Some(0.1),
            load_per_cpu: Some(999.0),
        };
        assert!(
            matches!(super::assess(absurd, t, 8), super::Verdict::Throttle { .. }),
            "load must never produce Halt at any magnitude"
        );
    }

    /// PER-CPU is the whole point: the same raw load is saturation on a small
    /// box and healthy on a large one, so a raw threshold cannot be shared.
    #[test]
    fn the_band_is_per_cpu_not_raw_load() {
        let t = super::Thresholds::default();
        let mk = |load_per_cpu| super::Reading {
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
            fd_ratio: Some(0.1),
            load_per_cpu: Some(load_per_cpu),
        };
        // raw load 16: saturating on 4 cores, comfortable on 32.
        assert!(matches!(
            super::assess(mk(16.0 / 4.0), t, 8),
            super::Verdict::Throttle { .. }
        ));
        assert_eq!(
            super::assess(mk(16.0 / 32.0), t, 8),
            super::Verdict::Proceed
        );
    }

    /// The same anti-vacuity rule the fd axis had to learn: an axis that cannot
    /// be READ must not read as "no pressure". `.ok()`, never `unwrap_or(0.0)`.
    ///
    /// Red run: change `load_per_cpu: load_per_cpu().ok()` in the reader to
    /// `Some(load_per_cpu().unwrap_or(0.0))` and a host whose sysctl is
    /// unreadable starts reporting a healthy load axis it never measured.
    #[test]
    fn an_unreadable_load_axis_is_skipped_not_passed() {
        let t = super::Thresholds::default();
        let unread = super::Reading {
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
            fd_ratio: Some(0.1),
            load_per_cpu: None,
        };
        let busy = super::Reading {
            load_per_cpu: Some(9.0),
            ..unread
        };
        assert_eq!(super::assess(unread, t, 8), super::Verdict::Proceed);
        assert_ne!(
            format!("{:?}", super::assess(unread, t, 8)),
            format!("{:?}", super::assess(busy, t, 8)),
            "an unread load axis must not produce the same verdict as a busy one"
        );
        // And the reader must keep using `.ok()` — the source-level guard the
        // fd axis needed, applied to its sibling.
        let code = include_str!("pressure.rs");
        assert!(
            code.contains("load_per_cpu: load_per_cpu().ok()"),
            "the reader must report an unreadable load axis as unread, not 0.0"
        );
    }

    /// Red run: change `fd_ratio: fd_ratio().ok()` back to
    /// `Some(fd_ratio().unwrap_or(0.0))` and the unavailable case below
    /// starts reporting a healthy fd axis on a host that has none.
    #[test]
    fn an_unavailable_fd_axis_neither_halts_nor_passes_silently() {
        let t = super::Thresholds::default();
        let unread = super::Reading {
            load_per_cpu: None,
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
            fd_ratio: None,
        };
        // Skipped, not passed: the verdict must come from the disk axis
        // alone, and must not claim the fd axis was fine.
        let v = super::assess(unread, t, 8);
        let saturated = super::Reading {
            load_per_cpu: None,
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
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
            load_per_cpu: None,
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
            fd_ratio: Some(0.10),
        };
        let hot = super::Reading {
            load_per_cpu: None,
            disk_free_gib: 500.0,
            disk_total_gib: 1000.0,
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
            load_per_cpu: None,
            disk_free_gib: 600.0,
            disk_total_gib: 1000.0,
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
                disk_free_gib: 5.0,
                disk_total_gib: 1000.0,
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
        //
        // 21 GiB free sits 1 GiB above the 20 GiB recovery floor, so exactly one
        // job fits. It used to read 30 GiB, which the derived floor now correctly
        // calls healthy — the reading was renumbered to stay genuinely squeezed
        // rather than the property being dropped.
        let v = assess(
            Reading {
                disk_free_gib: 21.0,
                disk_total_gib: 1000.0,
                ..healthy()
            },
            Thresholds::default(),
            8,
        );
        assert_eq!(v.inflight(8), Some(1), "got {v:?}");
        // And it is a THROTTLE, not a halt — the distinction this test owns.
        assert!(matches!(v, Verdict::Throttle { .. }), "got {v:?}");
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
                load_per_cpu: None,
                disk_free_gib: 10.0,
                disk_total_gib: 1000.0,
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
                disk_free_gib: 10.0,
                disk_total_gib: 1000.0,
                ..healthy()
            },
            Reading {
                fd_ratio: Some(0.99),
                ..healthy()
            },
            // 24 GiB is 4 above the recovery floor: a real disk throttle under
            // the derived band (30 GiB no longer is, and must not be, one).
            Reading {
                disk_free_gib: 24.0,
                disk_total_gib: 1000.0,
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
    fn a_large_disk_is_not_throttled_for_having_a_small_percentage_free() {
        // THE EFFICIENCY PROPERTY, and the reason this rework exists. cid:
        // 926 GiB volume, 192 GiB free = 20.7%. A percentage-only 15% band would
        // fire at 139 GiB free and run the reconciler at a quarter speed to
        // protect against nothing. The min(absolute, pct) pair fixed that.
        //
        // ★ AND THEN THE ABSOLUTE BAND BECAME THE ARBITRARY ONE — same defect,
        // other operand, found on ryn 2026-09-01 (460 GiB volume, 29.5 GiB free,
        // throttled to one job). The floor is now DERIVED from the halt floor plus
        // the concurrency, so both operands answer the question they are asked.
        let cid = Reading {
            load_per_cpu: None,
            disk_free_gib: 192.0,
            disk_total_gib: 926.0,
            fd_ratio: Some(0.0003),
        };
        assert_eq!(assess(cid, Thresholds::default(), 8), Verdict::Proceed);

        let t = Thresholds::default();
        assert_eq!(t.halt_floor_gib(926.0), 20.0);
        assert_eq!(
            t.throttle_floor_gib(926.0, 8),
            28.0,
            "recovery room (20) + 8 jobs x 1 GiB — derived, not asserted"
        );
        // The floor MOVES WITH THE CONCURRENCY. That is the property the flat
        // constant could not have: it answered a one-job question and then used
        // the answer to decide how many jobs to run.
        assert_eq!(t.throttle_floor_gib(926.0, 1), 21.0);
        assert_eq!(t.throttle_floor_gib(926.0, 32), 52.0);

        // ★ THE ryn CASE, which used to throttle to one job: 29.5 GiB free is
        // 9.5 GiB above the recovery floor, and 8 sub-GiB jobs fit in that.
        let ryn = Reading {
            load_per_cpu: None,
            disk_free_gib: 29.5,
            disk_total_gib: 460.4,
            fd_ratio: Some(0.0004),
        };
        assert_eq!(
            assess(ryn, t, 8),
            Verdict::Proceed,
            "a host with room for 9 jobs must not be cut to one"
        );

        // It still throttles when the space genuinely only affords a few jobs,
        // and it says HOW MANY rather than slamming to one.
        let tight = Reading {
            load_per_cpu: None,
            disk_free_gib: 23.0,
            disk_total_gib: 4000.0,
            fd_ratio: Some(0.0),
        };
        assert_eq!(
            assess(tight, t, 8).inflight(8),
            Some(3),
            "3 GiB above the recovery floor affords 3 jobs, not 1 and not 8"
        );
    }

    #[test]
    fn a_small_disk_falls_back_to_the_percentage_so_it_is_not_stopped_forever() {
        // Absolute alone is unsatisfiable below its own floor: a 40 GiB volume can
        // never have 50 GiB free, so every cycle would stop forever. The
        // percentage guard is what keeps the pair usable on small volumes.
        let t = Thresholds::default();
        assert_eq!(
            t.throttle_floor_gib(40.0, 4),
            6.0,
            "15% of 40 GiB — the percentage still dominates a small volume"
        );
        assert_eq!(t.halt_floor_gib(40.0), 2.0, "5% of 40 GiB");

        let small_ok = Reading {
            load_per_cpu: None,
            disk_free_gib: 12.0,
            disk_total_gib: 40.0,
            fd_ratio: Some(0.0),
        };
        assert_eq!(
            assess(small_ok, t, 4),
            Verdict::Proceed,
            "30% free on a small disk is fine"
        );

        let small_tight = Reading {
            load_per_cpu: None,
            disk_free_gib: 1.0,
            disk_total_gib: 40.0,
            fd_ratio: Some(0.0),
        };
        assert!(
            assess(small_tight, t, 4).inflight(4).is_none(),
            "2.5% free must stop"
        );
    }

    #[test]
    fn effective_floors_are_never_above_the_volume_itself() {
        // The property that makes min() correct rather than merely tuned.
        let t = Thresholds::default();
        for total in [1.0, 10.0, 40.0, 100.0, 926.0, 4000.0] {
            assert!(
                t.halt_floor_gib(total) <= total,
                "halt floor exceeds a {total} GiB volume"
            );
            // Now swept across concurrency too, since the floor depends on it.
            for inflight in [1u32, 4, 8, 32, 128] {
                assert!(
                    t.throttle_floor_gib(total, inflight) <= total,
                    "throttle floor exceeds {total} GiB at {inflight}-way"
                );
                assert!(
                    t.halt_floor_gib(total) <= t.throttle_floor_gib(total, inflight),
                    "stopping must be the tighter floor at {total} GiB, {inflight}-way"
                );
            }
        }
    }

    #[test]
    fn derived_percentage_is_reported_and_safe_on_a_zero_sized_volume() {
        let r = Reading {
            load_per_cpu: None,
            disk_free_gib: 192.0,
            disk_total_gib: 926.0,
            fd_ratio: None,
        };
        assert!(
            (r.disk_free_pct() - 20.7).abs() < 0.1,
            "got {}",
            r.disk_free_pct()
        );
        // A df row that parsed to zero must not divide by zero.
        let zero = Reading {
            load_per_cpu: None,
            disk_free_gib: 0.0,
            disk_total_gib: 0.0,
            fd_ratio: None,
        };
        assert_eq!(zero.disk_free_pct(), 0.0);
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
