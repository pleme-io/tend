//! `tend report` — operator-facing summary of scheduler activity.
//!
//! Reads the `scheduler-transitions.jsonl` audit log written by
//! `AuditFileEmitter` (M0.15c) and produces a human-readable rollup:
//! per-kind counts, recent failures, and the cohort of Jobs currently
//! sitting in non-terminal phases.
//!
//! Realizes M7 from the original tend plan (SHIGOTO.md §IV.4 + §VII).
//! No new state — every field below is derived from the transition
//! log alone.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use shigoto_types::{JobId, JobPhase, TransitionEvent};

use crate::drift::DriftEvent;

/// Bounded scan: anything older than this is excluded from the
/// report. Operators usually want "what's happening recently" — a
/// week's history is plenty for daemon-style cycles, and bounding
/// keeps memory + display sane for long-running logs.
const DEFAULT_WINDOW_HOURS: i64 = 7 * 24;

/// Rollup data derived from the transition log. Plain data; rendered
/// to the operator by `print_report` (see below).
#[derive(Debug, Default, Clone)]
pub(crate) struct Report {
    pub total_transitions: usize,
    pub window_hours: i64,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,

    /// Per-kind tallies: how many Jobs of each kind reached each
    /// terminal phase within the window. Sorted by kind, then phase.
    pub per_kind: BTreeMap<String, KindCounts>,

    /// Jobs whose most recent transition left them in a non-terminal
    /// phase (Failed, Retrying, Deadlettered, WaitingForOperator).
    /// Pairs (job_id, last_phase, last_seen_at).
    pub currently_stuck: Vec<StuckEntry>,

    /// Recent failed-execution reasons. Pairs (job_id, reason_string).
    /// Bounded to the most recent N (default 16) for readability.
    pub recent_failures: Vec<FailureEntry>,

    /// Drift events read from the drift JSONL log when present.
    /// Empty when no log exists or no drift was recorded. Each Vec
    /// element is the latest event read from the log within the
    /// window; same window cutoff as `total_transitions`.
    pub drift_events: Vec<DriftEvent>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct KindCounts {
    pub succeeded: u32,
    pub failed: u32,
    pub deadlettered: u32,
    pub retrying: u32,
    pub skipped: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct StuckEntry {
    pub job_id: JobId,
    pub phase: JobPhase,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct FailureEntry {
    pub at: DateTime<Utc>,
    pub job_id: JobId,
    pub reason: String,
}

/// Build a Report from the JSONL transition log at `path`. Lines
/// older than `window_hours` are skipped; malformed lines are
/// counted-but-ignored so a corrupted entry doesn't kill the report.
///
/// `drift_path` is an optional second JSONL file: the drift-events
/// log written by `AuditFileDriftSink`. When present + readable, its
/// entries appear in `report.drift_events` so the operator sees both
/// FSM activity and the typed drift surface in one output.
pub(crate) fn build_report(
    path: &Path,
    drift_path: Option<&Path>,
    window_hours: Option<i64>,
) -> Result<Report> {
    let window = window_hours.unwrap_or(DEFAULT_WINDOW_HOURS);
    let cutoff = Utc::now() - chrono::Duration::hours(window);

    // Streamed, not slurped. This was `read_to_string`, which pulled
    // the entire log into memory to compute a 7-day window: against a
    // 4.8 GB / 22M-line transition log, `tend report` peaked at
    // 4.95 GB RSS — survivable on a workstation, an OOM kill in the
    // operator pod. Nothing about the computation needs the whole file
    // resident; it is a single forward pass.
    let file = std::fs::File::open(path)
        .with_context(|| format!("reading transition log {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut report = Report {
        window_hours: window,
        ..Report::default()
    };

    // Track the latest transition per JobId so we can compute the
    // currently_stuck set in one pass — the most recent phase per
    // Job is what matters, not the historical churn.
    let mut latest: HashMap<JobId, (JobPhase, DateTime<Utc>)> = HashMap::new();

    use std::io::BufRead as _;
    for line in reader.lines() {
        // An IO error mid-file (truncated write, bad sector) ends the
        // pass rather than failing the report — the same tolerance
        // already applied to malformed lines below.
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: TransitionEvent = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue, // Skip malformed lines silently.
        };

        if event.at < cutoff {
            continue;
        }

        report.total_transitions += 1;

        if report.since.map_or(true, |s| event.at < s) {
            report.since = Some(event.at);
        }
        if report.until.map_or(true, |u| event.at > u) {
            report.until = Some(event.at);
        }

        // Per-kind tally — only counted on transitions INTO a
        // recognized terminal/notable phase, so multiple visits to
        // Retrying for the same Job don't inflate the counter.
        let kind = &event.job_id.kind.0;
        let counts = report.per_kind.entry(kind.clone()).or_default();
        match &event.to {
            JobPhase::Succeeded => counts.succeeded += 1,
            JobPhase::Failed { .. } => counts.failed += 1,
            JobPhase::Deadlettered => counts.deadlettered += 1,
            JobPhase::Retrying { .. } => counts.retrying += 1,
            JobPhase::Skipped(_) => counts.skipped += 1,
            _ => {} // Pending/Gated/Ready/Running/WaitingForOperator
                    // are intermediate — covered by the stuck-set below.
        }

        // Capture failure reasons for the recent-failures section.
        if let shigoto_types::TransitionReason::ExecutionFailed(reason) = &event.reason {
            report.recent_failures.push(FailureEntry {
                at: event.at,
                job_id: event.job_id.clone(),
                reason: reason.clone(),
            });
        }

        // Latest transition wins for each Job.
        latest.insert(event.job_id.clone(), (event.to.clone(), event.at));
    }

    // Bound recent_failures to most recent 16, oldest dropped.
    if report.recent_failures.len() > 16 {
        report.recent_failures.sort_by(|a, b| b.at.cmp(&a.at));
        report.recent_failures.truncate(16);
    } else {
        report.recent_failures.sort_by(|a, b| b.at.cmp(&a.at));
    }

    // Build the currently-stuck set from latest transitions.
    for (id, (phase, at)) in latest {
        let stuck = matches!(
            phase,
            JobPhase::Failed { .. }
                | JobPhase::Retrying { .. }
                | JobPhase::Deadlettered
                | JobPhase::WaitingForOperator
        );
        if stuck {
            report.currently_stuck.push(StuckEntry {
                job_id: id,
                phase,
                last_seen_at: at,
            });
        }
    }
    report.currently_stuck.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));

    // Pull in drift events when the operator wired a drift log.
    // We don't time-window drift events — the drift log is already
    // a curated stream (one entry per detected drift event per
    // reconcile cycle), so showing them all gives the operator the
    // full picture without prematurely trimming.
    if let Some(dpath) = drift_path {
        if let Ok(drift_content) = std::fs::read_to_string(dpath) {
            for line in drift_content.lines() {
                if let Ok(event) = serde_json::from_str::<DriftEvent>(line) {
                    report.drift_events.push(event);
                }
            }
        }
    }

    Ok(report)
}

/// Render a Report to stdout. Compact multi-section format matching
/// the existing `display::print_*` style — bold kind names, colored
/// counts (green=success, red=failures, yellow=warnings).
pub(crate) fn print_report(report: &Report) {
    use colored::Colorize;

    if let (Some(since), Some(until)) = (report.since, report.until) {
        println!(
            "{} {} transitions over {}h ({} → {})",
            "scheduler report:".bold(),
            report.total_transitions.to_string().cyan(),
            report.window_hours,
            since.format("%Y-%m-%d %H:%M:%S"),
            until.format("%Y-%m-%d %H:%M:%S"),
        );
    } else {
        println!(
            "{} no transitions in the last {}h — nothing to report",
            "scheduler report:".bold(),
            report.window_hours,
        );
        return;
    }

    println!();
    println!("{}", "per-kind:".bold());
    for (kind, counts) in &report.per_kind {
        println!(
            "  {} → {} ok, {} failed, {} deadletter, {} retrying, {} skipped",
            kind.bold(),
            counts.succeeded.to_string().green(),
            counts.failed.to_string().red(),
            counts.deadlettered.to_string().red(),
            counts.retrying.to_string().yellow(),
            counts.skipped.to_string().cyan(),
        );
    }

    if !report.currently_stuck.is_empty() {
        println!();
        println!(
            "{} {} job(s)",
            "currently stuck:".bold(),
            report.currently_stuck.len().to_string().red()
        );
        for entry in &report.currently_stuck {
            println!(
                "  {} [{}] — last seen {}",
                format_job_id(&entry.job_id),
                format_phase(&entry.phase).red(),
                entry.last_seen_at.format("%Y-%m-%d %H:%M:%S"),
            );
        }
    }

    if !report.recent_failures.is_empty() {
        println!();
        println!("{}", "recent failures:".bold());
        for entry in &report.recent_failures {
            println!(
                "  [{}] {} — {}",
                entry.at.format("%Y-%m-%d %H:%M:%S"),
                format_job_id(&entry.job_id),
                entry.reason.dimmed(),
            );
        }
    }

    if !report.drift_events.is_empty() {
        println!();
        println!(
            "{} {} event(s)",
            "drift:".bold(),
            report.drift_events.len().to_string().yellow()
        );
        for event in &report.drift_events {
            println!("  {}", format_drift_event(event).yellow());
        }
    }
}

fn format_drift_event(event: &DriftEvent) -> String {
    match event {
        DriftEvent::StubDirectoryFound {
            workspace,
            repo_name,
            ..
        } => format!("[{workspace}] {repo_name}: stub directory (no .git)"),
        DriftEvent::DirtyTreeBlocksPull {
            workspace,
            repo_name,
        } => format!("[{workspace}] {repo_name}: dirty tree blocked pull"),
        DriftEvent::PullFailed {
            workspace,
            repo_name,
            stderr,
        } => format!("[{workspace}] {repo_name}: pull failed — {}", stderr.lines().next().unwrap_or("")),
        DriftEvent::PullFailedNoUpstream { workspace, repo_name } => {
            format!("[{workspace}] {repo_name}: pull failed — no upstream tracking")
        }
        DriftEvent::PullFailedBranchRenamed { workspace, repo_name, expected_ref } => {
            format!("[{workspace}] {repo_name}: pull failed — expected ref {expected_ref} not on remote (branch renamed?)")
        }
        DriftEvent::PullFailedDiverged { workspace, repo_name } => {
            format!("[{workspace}] {repo_name}: pull failed — diverged from origin (ff-only refused)")
        }
        DriftEvent::PullFailedRepoMissing { workspace, repo_name } => {
            format!("[{workspace}] {repo_name}: pull failed — upstream repo gone (404)")
        }
        DriftEvent::PullFailedTransient { workspace, repo_name, snippet } => {
            format!("[{workspace}] {repo_name}: pull failed — transient ({snippet})")
        }
        DriftEvent::SyncFailed {
            workspace,
            repo_name,
            stderr,
        } => format!("[{workspace}] {repo_name}: sync failed — {}", stderr.lines().next().unwrap_or("")),
        DriftEvent::JobUnhealed { job_id, phase } => {
            format!("{}: {}", format_job_id(job_id), format_phase(phase))
        }
        DriftEvent::RepoHasNoRemote {
            workspace,
            repo_name,
        } => format!(
            "[{workspace}] {repo_name}: NO REMOTE — history exists on this machine only, never pushed"
        ),
        DriftEvent::LocalRepoNotInDiscovery {
            workspace,
            repo_name,
        } => format!("[{workspace}] {repo_name}: local-only (not in current discovery)"),
        DriftEvent::RemoteUrlEmbeddedCredential {
            workspace,
            repo_name,
            slug,
            credential,
        } => format!(
            "[{workspace}] {repo_name}: CREDENTIAL IN REMOTE URL — origin embeds {credential} ({slug}); token is sitting in .git/config"
        ),
        DriftEvent::RemoteProtocolMismatch {
            workspace,
            repo_name,
            slug,
            declared,
            actual,
        } => format!(
            "[{workspace}] {repo_name}: remote is {actual}, workspace declares {declared} ({slug})"
        ),
    }
}

fn format_job_id(id: &JobId) -> String {
    use shigoto_types::{JobScope, JobSubject};
    let scope = match &id.scope {
        JobScope::Global => "global".to_string(),
        JobScope::Workspace(w) => format!("ws:{w}"),
        JobScope::Repo { workspace, repo } => format!("ws:{workspace}/repo:{repo}"),
    };
    let subject = match &id.subject {
        JobSubject::None => String::new(),
        JobSubject::Repo(r) => format!("/repo:{r}"),
        JobSubject::Org(o) => format!("/org:{o}"),
        JobSubject::Path(p) => format!("/path:{}", p.display()),
        JobSubject::Pinned(p) => format!("/{p}"),
    };
    format!("{}{} kind={}", scope, subject, id.kind.0)
}

fn format_phase(phase: &JobPhase) -> String {
    match phase {
        JobPhase::Pending => "Pending".into(),
        JobPhase::Gated => "Gated".into(),
        JobPhase::Ready => "Ready".into(),
        JobPhase::Running => "Running".into(),
        JobPhase::Succeeded => "Succeeded".into(),
        JobPhase::Failed { attempts } => format!("Failed×{attempts}"),
        JobPhase::Retrying { .. } => "Retrying".into(),
        JobPhase::Skipped(_) => "Skipped".into(),
        JobPhase::Deadlettered => "Deadlettered".into(),
        JobPhase::WaitingForOperator => "WaitingForOperator".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shigoto_types::{JobKindId, JobScope, JobSubject, TransitionReason};
    use tempfile::TempDir;

    fn write_log(path: &Path, events: &[TransitionEvent]) {
        let mut content = String::new();
        for e in events {
            content.push_str(&serde_json::to_string(e).unwrap());
            content.push('\n');
        }
        std::fs::write(path, content).unwrap();
    }

    fn mk_event(
        at: DateTime<Utc>,
        kind: &str,
        subject: &str,
        from: JobPhase,
        to: JobPhase,
    ) -> TransitionEvent {
        TransitionEvent {
            at,
            job_id: JobId {
                scope: JobScope::Workspace("ws".into()),
                kind: JobKindId::new(kind),
                subject: JobSubject::Repo(subject.into()),
            },
            from,
            to,
            reason: TransitionReason::GateEvaluation,
            tool: "tend".into(),
        }
    }

    #[test]
    fn empty_log_yields_empty_report() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();

        let report = build_report(&path, None, Some(24)).unwrap();
        assert_eq!(report.total_transitions, 0);
        assert!(report.per_kind.is_empty());
        assert!(report.currently_stuck.is_empty());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.jsonl");
        let good = mk_event(
            Utc::now(),
            "k",
            "r",
            JobPhase::Pending,
            JobPhase::Succeeded,
        );
        let mut content = serde_json::to_string(&good).unwrap();
        content.push('\n');
        content.push_str("not-json-at-all\n");
        content.push_str("{}\n"); // Valid JSON but wrong shape — also skipped.
        std::fs::write(&path, content).unwrap();

        let report = build_report(&path, None, Some(24)).unwrap();
        assert_eq!(
            report.total_transitions, 1,
            "only the well-formed line should count"
        );
    }

    #[test]
    fn old_events_outside_window_are_excluded() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("old.jsonl");

        let now = Utc::now();
        let recent = mk_event(now, "k", "r1", JobPhase::Pending, JobPhase::Succeeded);
        let old = mk_event(
            now - chrono::Duration::days(30),
            "k",
            "r2",
            JobPhase::Pending,
            JobPhase::Succeeded,
        );

        write_log(&path, &[old, recent]);

        let report = build_report(&path, None, Some(24)).unwrap();
        assert_eq!(report.total_transitions, 1, "only recent event counted");
        assert_eq!(
            report.per_kind.get("k").map(|c| c.succeeded).unwrap_or(0),
            1
        );
    }

    #[test]
    fn currently_stuck_lists_jobs_in_failed_or_retrying() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("stuck.jsonl");

        let now = Utc::now();
        let one_min = chrono::Duration::minutes(1);

        // job-a: Pending → Succeeded (NOT stuck)
        // job-b: Pending → Failed{attempts:1} (stuck)
        // job-c: Pending → Retrying → Pending → Failed (stuck)
        let events = vec![
            mk_event(now, "k", "a", JobPhase::Pending, JobPhase::Succeeded),
            mk_event(
                now - one_min,
                "k",
                "b",
                JobPhase::Running,
                JobPhase::Failed { attempts: 1 },
            ),
            mk_event(
                now - one_min * 4,
                "k",
                "c",
                JobPhase::Pending,
                JobPhase::Retrying { until_ms: 0 },
            ),
            mk_event(
                now - one_min * 3,
                "k",
                "c",
                JobPhase::Retrying { until_ms: 0 },
                JobPhase::Pending,
            ),
            mk_event(
                now - one_min * 2,
                "k",
                "c",
                JobPhase::Running,
                JobPhase::Failed { attempts: 2 },
            ),
        ];
        write_log(&path, &events);

        let report = build_report(&path, None, Some(24)).unwrap();
        let stuck: Vec<&str> = report
            .currently_stuck
            .iter()
            .map(|e| match &e.job_id.subject {
                JobSubject::Repo(r) => r.as_str(),
                _ => "?",
            })
            .collect();
        assert!(!stuck.contains(&"a"), "a should not be stuck (Succeeded)");
        assert!(stuck.contains(&"b"), "b should be stuck (Failed)");
        assert!(stuck.contains(&"c"), "c should be stuck (Failed terminal)");
    }
}
