use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{Config, PushPolicy, Workspace};
use crate::scan::{self, LocalRepo, ScanRow, Snapshot, StatusSnapshotConfig};
use crate::sync::StateWord;

pub(crate) const BUDGET: Duration = Duration::from_millis(1200);
pub(crate) const MAX_LISTED: usize = 15;
const MAX_WORKERS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    Stop,
    SessionStart,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Payload {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub stop_hook_active: bool,
}

impl Payload {
    pub(crate) fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        if raw.trim().is_empty() {
            Ok(Self::default())
        } else {
            serde_json::from_str(raw)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Policy {
    pub push: PushPolicy,
    pub pr_skill: Option<String>,
}

pub(crate) struct Policies(HashMap<String, Policy>);

impl Policies {
    pub(crate) fn of(workspaces: &[&Workspace]) -> Self {
        Self(
            workspaces
                .iter()
                .map(|ws| {
                    (
                        ws.name.clone(),
                        Policy {
                            push: ws.push_policy,
                            pr_skill: ws.pr_skill.clone(),
                        },
                    )
                })
                .collect(),
        )
    }

    pub(crate) fn get(&self, workspace: &str) -> Policy {
        self.0.get(workspace).cloned().unwrap_or(Policy {
            push: PushPolicy::Main,
            pr_skill: None,
        })
    }
}

pub(crate) fn run(event: Event, config: anyhow::Result<Config>, raw: &str) -> Option<Value> {
    let payload = match Payload::parse(raw) {
        Ok(payload) => payload,
        Err(e) => {
            return Some(
                json!({ "systemMessage": format!("tend hook: unreadable hook payload: {e}") }),
            )
        }
    };
    let config = match config {
        Ok(config) => config,
        Err(e) => {
            return Some(json!({ "systemMessage": format!("tend hook: config unreadable: {e:#}") }))
        }
    };
    let workspaces: Vec<&Workspace> = config.workspaces.iter().collect();
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(MAX_WORKERS);
    match event {
        Event::Stop => stop(
            &workspaces,
            &payload,
            BUDGET,
            workers,
            payload
                .session_id
                .as_deref()
                .and_then(reported_path)
                .as_deref(),
        ),
        Event::SessionStart => session_start(
            &workspaces,
            &config.status_snapshot,
            &scan::snapshot_path(),
            &payload,
            BUDGET,
            workers,
            SystemTime::now(),
        ),
    }
}

pub(crate) fn session_started(transcript: Option<&Path>) -> Option<SystemTime> {
    let meta = std::fs::metadata(transcript?).ok()?;
    meta.created().or_else(|_| meta.modified()).ok()
}

pub(crate) fn blocks_stop(row: &ScanRow) -> bool {
    row.state == StateWord::Stuck || row.dirty_count.unwrap_or(0) > 0 || row.ahead.unwrap_or(0) > 0
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub(crate) fn describe(row: &ScanRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if row.state == StateWord::Stuck {
        parts.push("mid rebase or merge".to_owned());
    }
    if let Some(n) = row.dirty_count.filter(|n| *n > 0) {
        parts.push(format!("{n} uncommitted path{}", plural(n as u64)));
    }
    match (row.ahead, row.behind) {
        (Some(a), Some(b)) if a > 0 && b > 0 => {
            parts.push(format!("diverged, {a} ahead and {b} behind its upstream"));
        }
        (Some(a), Some(_)) if a > 0 => {
            parts.push(format!(
                "{a} commit{} ahead of its upstream",
                plural(a.into())
            ));
        }
        (Some(_), Some(b)) if b > 0 => {
            parts.push(format!(
                "{b} commit{} behind its upstream",
                plural(b.into())
            ));
        }
        (_, Some(_)) => {}
        (ahead, None) => match row.state {
            StateWord::NoRemote => parts.push("no remote configured".to_owned()),
            StateWord::Unborn => parts.push("no commits yet".to_owned()),
            _ => {
                if let Some(ahead) = ahead {
                    let tip = if row.branch.is_some() {
                        "branch tracks no upstream"
                    } else {
                        "detached HEAD"
                    };
                    if ahead > 0 {
                        parts.push(format!(
                            "{tip}, {ahead} commit{} on no remote",
                            plural(ahead.into())
                        ));
                    } else {
                        parts.push(tip.to_owned());
                    }
                }
            }
        },
    }
    parts.join(", ")
}

pub(crate) fn instruction(policy: &Policy, row: &ScanRow) -> String {
    match row.state {
        StateWord::NoRemote if row.dirty_count.unwrap_or(0) > 0 => {
            "commit the authorized work; it has no remote to push to".to_owned()
        }
        StateWord::NoRemote => "give it a remote and push, or retire the clone".to_owned(),
        StateWord::Behind => "fast-forward it with git pull --ff-only".to_owned(),
        _ => match (policy.push, &policy.pr_skill) {
            (PushPolicy::Main, _) => "commit the authorized work and push main".to_owned(),
            (PushPolicy::Pr, Some(skill)) => {
                format!("commit on a branch and open a PR per the {skill} skill; never push main")
            }
            (PushPolicy::Pr, None) => {
                "commit on a branch and open a PR; never push main".to_owned()
            }
        },
    }
}

pub(crate) fn sort_worst_first(rows: &mut [ScanRow]) {
    let rank = |r: &ScanRow| r.state.problem_rank().unwrap_or(u8::MAX);
    let weight = |r: &ScanRow| {
        r.dirty_count.unwrap_or(0) as u64
            + u64::from(r.ahead.unwrap_or(0))
            + u64::from(r.behind.unwrap_or(0))
    };
    rows.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then(weight(b).cmp(&weight(a)))
            .then_with(|| a.label().cmp(&b.label()))
    });
}

pub(crate) fn touched_repos(
    workspaces: &[&Workspace],
    cwd: Option<&Path>,
    since: Option<SystemTime>,
) -> Vec<LocalRepo> {
    let cwd_repo = cwd.and_then(|c| scan::enclosing_repo(workspaces, c));
    let mut touched: Vec<(Option<SystemTime>, LocalRepo)> = match since {
        Some(since) => scan::local_repos(workspaces)
            .into_iter()
            .filter(|repo| cwd_repo.as_ref().is_none_or(|c| c.path != repo.path))
            .filter(|repo| scan::touched_since(&repo.path, since))
            .map(|repo| (scan::last_activity(&repo.path), repo))
            .collect(),
        None => Vec::new(),
    };
    touched.sort_by(|a, b| b.0.cmp(&a.0));
    cwd_repo
        .into_iter()
        .chain(touched.into_iter().map(|(_, repo)| repo))
        .collect()
}

pub(crate) fn reported_path(session_id: &str) -> Option<PathBuf> {
    let safe = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| {
        crate::cache::tend_cache_root()
            .join("hook")
            .join("stop")
            .join(format!("{session_id}.json"))
    })
}

fn fingerprint(row: &ScanRow) -> String {
    format!("{} {}", row.label(), describe(row))
}

fn read_reported(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_reported(path: &Path, reported: &BTreeSet<String>) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string(reported) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub(crate) fn stop(
    workspaces: &[&Workspace],
    payload: &Payload,
    budget: Duration,
    workers: usize,
    memory: Option<&Path>,
) -> Option<Value> {
    let started = Instant::now();
    let since = session_started(payload.transcript_path.as_deref());
    let candidates = touched_repos(workspaces, payload.cwd.as_deref(), since);
    let result = scan::classify(
        candidates,
        scan::unsettled_probe,
        workers,
        Some(started + budget),
    );
    let mut findings: Vec<ScanRow> = result.rows().into_iter().filter(blocks_stop).collect();
    if let Some(memory) = memory {
        let mut reported = read_reported(memory);
        findings.retain(|row| reported.insert(fingerprint(row)));
        write_reported(memory, &reported);
    }
    sort_worst_first(&mut findings);
    render_stop(
        &findings,
        result.unchecked.len() + result.failed(),
        payload.stop_hook_active,
        &Policies::of(workspaces),
    )
}

pub(crate) fn render_stop(
    findings: &[ScanRow],
    unchecked: usize,
    stop_hook_active: bool,
    policies: &Policies,
) -> Option<Value> {
    let unchecked_note = if unchecked > 0 {
        format!(
            " {unchecked} other touched repo{} could not be checked within the hook's time budget.",
            plural(unchecked as u64)
        )
    } else {
        String::new()
    };
    if findings.is_empty() {
        return (unchecked > 0)
            .then(|| json!({ "systemMessage": format!("tend:{unchecked_note}") }));
    }
    if stop_hook_active {
        let list = findings
            .iter()
            .map(|r| format!("{} ({})", r.label(), describe(r)))
            .collect::<Vec<_>>()
            .join("; ");
        return Some(json!({
            "systemMessage": format!(
                "tend: still uncommitted or unpushed after the stop hook: {list}.{unchecked_note}"
            )
        }));
    }
    let items = findings
        .iter()
        .map(|r| {
            format!(
                "{} ({}): {}",
                r.label(),
                describe(r),
                instruction(&policies.get(&r.workspace), r)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let n = findings.len();
    let (noun, verb) = if n == 1 {
        ("repo", "holds")
    } else {
        ("repos", "hold")
    };
    Some(json!({
        "decision": "block",
        "reason": format!(
            "tend: {n} {noun} touched this session {verb} uncommitted or unpushed work. \
             If this is your work, finish it before stopping: {items}.{unchecked_note} \
             If a repo holds another session's work, leave it alone and say so in your final message."
        ),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Coverage {
    Snapshot {
        age: Duration,
        rechecked: usize,
        unchecked: usize,
    },
    Live {
        checked: usize,
        total: usize,
    },
}

impl Coverage {
    pub(crate) fn complete(&self) -> bool {
        match self {
            Self::Snapshot { unchecked, .. } => *unchecked == 0,
            Self::Live { checked, total } => checked >= total,
        }
    }

    fn sentence(&self) -> String {
        match self {
            Self::Snapshot {
                age,
                rechecked,
                unchecked,
            } => {
                let mut s = format!(
                    "from the tend daemon snapshot taken {} ago, {rechecked} re-checked live",
                    human_age(*age)
                );
                if *unchecked > 0 {
                    let _ = write!(s, ", {unchecked} not checked within the hook budget");
                }
                s
            }
            Self::Live { checked, total } => format!(
                "no fresh daemon snapshot, checked live {checked} of {total}, most recently active first"
            ),
        }
    }
}

fn human_age(age: Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h{}m", secs / 3600, (secs % 3600) / 60),
    }
}

pub(crate) fn session_start(
    workspaces: &[&Workspace],
    snapshot_config: &StatusSnapshotConfig,
    snapshot_path: &Path,
    payload: &Payload,
    budget: Duration,
    workers: usize,
    now: SystemTime,
) -> Option<Value> {
    let deadline = Some(Instant::now() + budget);
    let repos = scan::local_repos(workspaces);
    let total = repos.len();
    let cwd_repo = payload
        .cwd
        .as_deref()
        .and_then(|c| scan::enclosing_repo(workspaces, c));
    let snapshot = scan::read_snapshot(snapshot_path)
        .filter(|s| snapshot_config.enable && s.age(now) <= snapshot_config.max_age());
    let (rows, coverage) = match snapshot {
        Some(snapshot) => from_snapshot(snapshot, repos, cwd_repo, workers, deadline, now),
        None => live(repos, cwd_repo, workers, deadline),
    };
    render_session_start(&rows, total, &coverage, &Policies::of(workspaces))
}

fn from_snapshot(
    snapshot: Snapshot,
    repos: Vec<LocalRepo>,
    cwd_repo: Option<LocalRepo>,
    workers: usize,
    deadline: Option<Instant>,
    now: SystemTime,
) -> (Vec<ScanRow>, Coverage) {
    let since = snapshot.taken_at();
    let age = snapshot.age(now);
    let forced = cwd_repo.as_ref().map(|r| r.path.clone());
    let mut known: HashMap<PathBuf, ScanRow> = snapshot
        .rows
        .into_iter()
        .map(|row| (row.path.clone(), row))
        .collect();
    let mut stale: HashMap<PathBuf, ScanRow> = HashMap::new();
    let mut rows = Vec::new();
    let mut candidates = Vec::new();
    if let Some(cwd) = cwd_repo {
        if !repos.iter().any(|r| r.path == cwd.path) {
            candidates.push(cwd);
        }
    }
    for repo in repos {
        match known.remove(&repo.path) {
            Some(row)
                if row.state.problem_rank().is_none()
                    && forced.as_ref() != Some(&repo.path)
                    && !scan::touched_since(&repo.path, since) =>
            {
                rows.push(row);
            }
            Some(row) => {
                stale.insert(repo.path.clone(), row);
                candidates.push(repo);
            }
            None => candidates.push(repo),
        }
    }
    let result = scan::classify(candidates, scan::full_probe, workers, deadline);
    let rechecked = result.scanned.len();
    let mut unchecked = 0usize;
    for scanned in &result.scanned {
        match &scanned.outcome {
            Ok(Some((status, facts))) => rows.push(ScanRow::new(&scanned.repo, status, facts)),
            Ok(None) => {}
            Err(_) => match stale.remove(&scanned.repo.path) {
                Some(row) => rows.push(row),
                None => unchecked += 1,
            },
        }
    }
    for repo in &result.unchecked {
        match stale.remove(&repo.path) {
            Some(row) => rows.push(row),
            None => unchecked += 1,
        }
    }
    (
        rows,
        Coverage::Snapshot {
            age,
            rechecked,
            unchecked,
        },
    )
}

fn live(
    repos: Vec<LocalRepo>,
    cwd_repo: Option<LocalRepo>,
    workers: usize,
    deadline: Option<Instant>,
) -> (Vec<ScanRow>, Coverage) {
    let mut ordered = repos;
    ordered.sort_by_cached_key(|r| std::cmp::Reverse(scan::last_activity(&r.path)));
    if let Some(cwd) = cwd_repo {
        ordered.retain(|r| r.path != cwd.path);
        ordered.insert(0, cwd);
    }
    let total = ordered.len();
    let result = scan::classify(ordered, scan::full_probe, workers, deadline);
    (
        result.rows(),
        Coverage::Live {
            checked: result.scanned.len(),
            total,
        },
    )
}

pub(crate) fn render_session_start(
    rows: &[ScanRow],
    total: usize,
    coverage: &Coverage,
    policies: &Policies,
) -> Option<Value> {
    let mut problems: Vec<ScanRow> = rows
        .iter()
        .filter(|r| r.state.problem_rank().is_some())
        .cloned()
        .collect();
    if problems.is_empty() && coverage.complete() {
        return None;
    }
    sort_worst_first(&mut problems);
    let mut text = String::new();
    if problems.is_empty() {
        let _ = write!(
            text,
            "tend: no workspace repo needs attention among those checked ({total} on disk; {}).",
            coverage.sentence()
        );
    } else {
        let _ = writeln!(
            text,
            "tend: {} of {total} workspace repos need attention ({}); {}.",
            problems.len(),
            state_counts(&problems),
            coverage.sentence()
        );
        for row in problems.iter().take(MAX_LISTED) {
            let _ = writeln!(
                text,
                "- {} [{}]: {}. To do: {}.",
                row.label(),
                row.state,
                describe(row),
                instruction(&policies.get(&row.workspace), row)
            );
        }
        if problems.len() > MAX_LISTED {
            let _ = writeln!(
                text,
                "- and {} more (tend status --problems lists the tracked ones).",
                problems.len() - MAX_LISTED
            );
        }
        text.push_str(
            "If any of these is unfinished work from an earlier session of yours, finish it; \
             another session may own the rest.",
        );
    }
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": text,
        }
    }))
}

fn state_counts(problems: &[ScanRow]) -> String {
    [
        StateWord::Stuck,
        StateWord::Dirty,
        StateWord::Ahead,
        StateWord::NoUpstream,
        StateWord::NoRemote,
        StateWord::Behind,
    ]
    .into_iter()
    .filter_map(|word| {
        let n = problems.iter().filter(|r| r.state == word).count();
        (n > 0).then(|| format!("{n} {word}"))
    })
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn backed_repo(base: &Path, name: &str) -> PathBuf {
        let upstream = base.join(".upstreams").join(format!("{name}.git"));
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "--bare", "-b", "main"]);
        let repo = base.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(
            &repo,
            &["remote", "add", "origin", &upstream.to_string_lossy()],
        );
        std::fs::write(repo.join("f"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "--no-verify", "-m", "base"]);
        git(&repo, &["push", "-q", "-u", "origin", "main"]);
        repo
    }

    fn workspace(name: &str, base: &Path, push: PushPolicy, pr_skill: Option<&str>) -> Workspace {
        let mut ws = Workspace::test_default(name);
        ws.base_dir = base.to_string_lossy().into_owned();
        ws.push_policy = push;
        ws.pr_skill = pr_skill.map(ToOwned::to_owned);
        ws
    }

    fn row(workspace: &str, name: &str, state: StateWord) -> ScanRow {
        ScanRow {
            workspace: workspace.into(),
            name: name.into(),
            path: PathBuf::from(format!("/w/{workspace}/{name}")),
            state,
            branch: Some("main".into()),
            ahead: Some(0),
            behind: Some(0),
            dirty_count: Some(0),
        }
    }

    fn policies() -> Policies {
        let main = workspace("pleme-io", Path::new("/w/pleme-io"), PushPolicy::Main, None);
        let pr = workspace(
            "akeylesslabs",
            Path::new("/w/akeylesslabs"),
            PushPolicy::Pr,
            Some("akeyless-pr-standards"),
        );
        Policies::of(&[&main, &pr])
    }

    fn reason(value: &Value) -> &str {
        value["reason"].as_str().expect("reason")
    }

    fn transcript_now(dir: &Path) -> PathBuf {
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        path
    }

    #[test]
    fn the_stop_payload_parses_and_ignores_fields_it_does_not_use() {
        let raw = r#"{"session_id":"abc","transcript_path":"/t/x.jsonl","cwd":"/w/r","permission_mode":"default","hook_event_name":"Stop","stop_hook_active":true}"#;
        let payload = Payload::parse(raw).unwrap();
        assert_eq!(payload.transcript_path, Some(PathBuf::from("/t/x.jsonl")));
        assert_eq!(payload.cwd, Some(PathBuf::from("/w/r")));
        assert!(payload.stop_hook_active);
        assert!(!Payload::parse("").unwrap().stop_hook_active);
        assert!(Payload::parse("not json").is_err());
    }

    #[test]
    fn a_dirty_repo_blocks_with_main_wording_and_the_ownership_hedge() {
        let mut dirty = row("pleme-io", "tend", StateWord::Dirty);
        dirty.dirty_count = Some(3);
        dirty.ahead = Some(1);

        let out = render_stop(&[dirty], 0, false, &policies()).unwrap();
        assert_eq!(out["decision"], "block");
        let reason = reason(&out);
        assert!(reason.contains("pleme-io/tend@main"), "{reason}");
        assert!(
            reason.contains("3 uncommitted paths, 1 commit ahead of its upstream"),
            "{reason}"
        );
        assert!(
            reason.contains("commit the authorized work and push main"),
            "{reason}"
        );
        assert!(reason.contains("If this is your work"), "{reason}");
        assert!(!reason.contains("never push main"), "{reason}");
        assert!(out.get("systemMessage").is_none());
    }

    #[test]
    fn a_pr_only_org_is_told_to_branch_and_open_a_pr_never_to_push_main() {
        let mut feature = row(
            "akeylesslabs",
            "akeyless-environments",
            StateWord::NoUpstream,
        );
        feature.branch = Some("asm-1".into());
        feature.behind = None;
        feature.ahead = Some(2);

        let out = render_stop(&[feature], 0, false, &policies()).unwrap();
        let reason = reason(&out);
        assert!(
            reason.contains(
                "commit on a branch and open a PR per the akeyless-pr-standards skill; never push main"
            ),
            "{reason}"
        );
        assert!(
            reason.contains("branch tracks no upstream, 2 commits on no remote"),
            "{reason}"
        );
        assert!(!reason.contains("and push main"), "{reason}");
    }

    #[test]
    fn a_second_stop_never_blocks_again_and_tells_the_user_instead() {
        let mut dirty = row("pleme-io", "tend", StateWord::Dirty);
        dirty.dirty_count = Some(1);

        let out = render_stop(&[dirty], 0, true, &policies()).unwrap();
        assert!(out.get("decision").is_none(), "{out}");
        let message = out["systemMessage"].as_str().unwrap();
        assert!(
            message.contains("pleme-io/tend@main (1 uncommitted path)"),
            "{message}"
        );
        assert!(!message.contains('\n'), "one line: {message}");
    }

    #[test]
    fn nothing_found_and_nothing_missed_prints_nothing() {
        assert!(render_stop(&[], 0, false, &policies()).is_none());
        assert!(render_stop(&[], 0, true, &policies()).is_none());
        let missed = render_stop(&[], 2, false, &policies()).unwrap();
        assert!(missed.get("decision").is_none());
        assert!(missed["systemMessage"]
            .as_str()
            .unwrap()
            .contains("2 other touched repos"));
    }

    #[test]
    fn behind_alone_does_not_block_a_stop_but_unpushed_commits_do() {
        let mut behind = row("pleme-io", "a", StateWord::Behind);
        behind.behind = Some(4);
        assert!(!blocks_stop(&behind));

        let mut no_upstream_but_pushed_elsewhere = row("pleme-io", "b", StateWord::NoUpstream);
        no_upstream_but_pushed_elsewhere.ahead = Some(0);
        no_upstream_but_pushed_elsewhere.behind = None;
        assert!(!blocks_stop(&no_upstream_but_pushed_elsewhere));

        let mut ahead = row("pleme-io", "c", StateWord::Ahead);
        ahead.ahead = Some(1);
        assert!(blocks_stop(&ahead));
        assert!(blocks_stop(&row("pleme-io", "d", StateWord::Stuck)));
    }

    #[test]
    fn stop_blocks_only_on_repos_whose_git_state_moved_this_session() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("ws");
        let untouched = backed_repo(&base, "untouched");
        let touched = backed_repo(&base, "touched");
        let transcript = transcript_now(tmp.path());

        std::fs::write(untouched.join("f"), "edited by someone else\n").unwrap();
        std::fs::write(touched.join("f"), "edited this session\n").unwrap();
        git(&touched, &["add", "f"]);

        let ws = workspace("ws", &base, PushPolicy::Main, None);
        let payload = Payload {
            session_id: None,
            transcript_path: Some(transcript),
            cwd: Some(tmp.path().to_path_buf()),
            stop_hook_active: false,
        };
        let out = stop(&[&ws], &payload, Duration::from_secs(30), 2, None).expect("blocks");
        let reason = reason(&out);
        assert!(reason.contains("ws/touched@main"), "{reason}");
        assert!(
            !reason.contains("untouched"),
            "a dirty repo this session never touched must not block: {reason}"
        );
    }

    #[test]
    fn a_finding_blocks_a_session_once_until_its_state_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("ws");
        let repo = backed_repo(&base, "shared");
        let transcript = transcript_now(tmp.path());
        let memory = tmp.path().join("reported").join("session.json");

        std::fs::write(repo.join("f"), "another session's edit\n").unwrap();
        git(&repo, &["add", "f"]);

        let ws = workspace("ws", &base, PushPolicy::Main, None);
        let payload = Payload {
            session_id: Some("session".into()),
            transcript_path: Some(transcript),
            cwd: Some(tmp.path().to_path_buf()),
            stop_hook_active: false,
        };
        let stop_now = || stop(&[&ws], &payload, Duration::from_secs(30), 2, Some(&memory));

        assert!(reason(&stop_now().expect("the first stop blocks")).contains("ws/shared@main"));
        assert!(
            stop_now().is_none(),
            "the same finding must not block this session twice"
        );

        std::fs::write(repo.join("g"), "a second edit\n").unwrap();
        git(&repo, &["add", "g"]);
        assert!(
            reason(&stop_now().expect("a changed state blocks again")).contains("ws/shared@main")
        );
    }

    #[test]
    fn a_session_id_that_is_not_a_plain_name_has_no_memory() {
        assert!(reported_path("../escape").is_none());
        assert!(reported_path("").is_none());
        assert!(reported_path("f0ff6f65-669e-4812-814d-19e15bce4b8c").is_some());
    }

    #[test]
    fn the_cwd_repo_is_always_a_candidate_even_without_a_git_state_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("ws");
        let repo = backed_repo(&base, "here");
        let transcript = transcript_now(tmp.path());
        std::fs::write(repo.join("new-file"), "untracked\n").unwrap();
        std::fs::create_dir_all(repo.join("sub")).unwrap();

        let ws = workspace("ws", &base, PushPolicy::Main, None);
        let elsewhere = Payload {
            session_id: None,
            transcript_path: Some(transcript.clone()),
            cwd: Some(tmp.path().to_path_buf()),
            stop_hook_active: false,
        };
        assert!(stop(&[&ws], &elsewhere, Duration::from_secs(30), 2, None).is_none());

        let inside = Payload {
            session_id: None,
            transcript_path: Some(transcript),
            cwd: Some(repo.join("sub")),
            stop_hook_active: false,
        };
        let out = stop(&[&ws], &inside, Duration::from_secs(30), 2, None).expect("blocks");
        assert!(reason(&out).contains("ws/here@main (1 uncommitted path)"));
    }

    #[test]
    fn touched_repos_come_cwd_first_then_most_recently_touched_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("ws");
        let older = backed_repo(&base, "older");
        let newer = backed_repo(&base, "newer");
        let here = backed_repo(&base, "here");
        let _untouched = backed_repo(&base, "untouched");
        let transcript = transcript_now(tmp.path());
        std::fs::write(older.join("f"), "1\n").unwrap();
        git(&older, &["add", "f"]);
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(newer.join("f"), "2\n").unwrap();
        git(&newer, &["add", "f"]);

        let ws = workspace("ws", &base, PushPolicy::Main, None);
        let since = session_started(Some(&transcript));
        let order: Vec<String> = touched_repos(&[&ws], Some(&here), since)
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(order, vec!["here", "newer", "older"]);
    }

    #[test]
    fn a_clean_touched_repo_lets_the_session_stop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("ws");
        let repo = backed_repo(&base, "done");
        let transcript = transcript_now(tmp.path());
        std::fs::write(repo.join("f"), "finished\n").unwrap();
        git(&repo, &["commit", "-q", "--no-verify", "-am", "finish"]);
        git(&repo, &["push", "-q"]);

        let ws = workspace("ws", &base, PushPolicy::Main, None);
        let payload = Payload {
            session_id: None,
            transcript_path: Some(transcript),
            cwd: Some(repo.clone()),
            stop_hook_active: false,
        };
        assert!(stop(&[&ws], &payload, Duration::from_secs(30), 2, None).is_none());
    }

    #[test]
    fn session_start_lists_problems_worst_first_and_caps_the_list() {
        let mut rows = Vec::new();
        let mut behind = row("pleme-io", "behind", StateWord::Behind);
        behind.behind = Some(1);
        rows.push(behind);
        let mut dirty = row("pleme-io", "dirty", StateWord::Dirty);
        dirty.dirty_count = Some(2);
        rows.push(dirty);
        rows.push(row("pleme-io", "clean", StateWord::Clean));
        rows.push(row("pleme-io", "unborn", StateWord::Unborn));
        for i in 0..20 {
            let mut ahead = row("akeylesslabs", &format!("ahead-{i:02}"), StateWord::Ahead);
            ahead.ahead = Some(1);
            rows.push(ahead);
        }
        let coverage = Coverage::Snapshot {
            age: Duration::from_secs(240),
            rechecked: 3,
            unchecked: 0,
        };

        let out = render_session_start(&rows, 100, &coverage, &policies()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
        let text = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            text.starts_with(
                "tend: 22 of 100 workspace repos need attention (1 dirty, 20 ahead, 1 behind)"
            ),
            "{text}"
        );
        assert!(
            text.contains("snapshot taken 4m ago, 3 re-checked live"),
            "{text}"
        );
        let listed: Vec<&str> = text.lines().filter(|l| l.starts_with("- ")).collect();
        assert_eq!(listed.len(), MAX_LISTED + 1, "{text}");
        assert!(
            listed[0].starts_with("- pleme-io/dirty@main [dirty]"),
            "{text}"
        );
        assert!(text.contains("and 7 more"), "{text}");
        assert!(
            !text.contains("pleme-io/behind"),
            "behind ranks last and falls past the cap: {text}"
        );
        assert!(!text.contains("unborn"), "{text}");
        assert!(text.contains("never push main"), "{text}");
    }

    #[test]
    fn session_start_is_silent_on_a_clean_fully_checked_workspace() {
        let rows = vec![
            row("pleme-io", "a", StateWord::Clean),
            row("pleme-io", "b", StateWord::Unborn),
        ];
        let live = Coverage::Live {
            checked: 2,
            total: 2,
        };
        assert!(render_session_start(&rows, 2, &live, &policies()).is_none());

        let partial = Coverage::Live {
            checked: 2,
            total: 9,
        };
        let out = render_session_start(&rows, 9, &partial, &policies()).unwrap();
        let text = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(text.contains("checked live 2 of 9"), "{text}");
    }

    #[test]
    fn session_start_trusts_a_fresh_snapshot_and_rechecks_only_what_may_have_moved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join("ws");
        let quiet = backed_repo(&base, "quiet");
        let noisy = backed_repo(&base, "noisy");
        std::fs::write(noisy.join("f"), "dirty\n").unwrap();
        let ws = workspace("ws", &base, PushPolicy::Main, None);
        let snapshot_path = tmp.path().join("snapshot.json");
        let now = SystemTime::now();
        let taken = now + Duration::from_secs(5);
        scan::refresh_snapshot(&[&ws], 2, &snapshot_path, taken).unwrap();

        std::fs::write(quiet.join("f"), "dirty after the snapshot, no git touch\n").unwrap();

        let config = StatusSnapshotConfig::default();
        let payload = Payload::default();
        let out = session_start(
            &[&ws],
            &config,
            &snapshot_path,
            &payload,
            Duration::from_secs(30),
            2,
            taken + Duration::from_secs(60),
        )
        .unwrap();
        let text = out["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(text.contains("ws/noisy@main [dirty]"), "{text}");
        assert!(
            !text.contains("ws/quiet"),
            "a repo the snapshot saw clean and git never touched is trusted: {text}"
        );
        assert!(text.contains("1 re-checked live"), "{text}");

        let stale = session_start(
            &[&ws],
            &config,
            &snapshot_path,
            &payload,
            Duration::from_secs(30),
            2,
            taken + config.max_age() + Duration::from_secs(1),
        )
        .unwrap();
        let text = stale["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("ws/quiet@main [dirty]"),
            "a stale snapshot is not trusted: {text}"
        );
        assert!(text.contains("checked live 2 of 2"), "{text}");
    }

    #[test]
    fn a_broken_payload_or_config_is_reported_never_blocked() {
        let empty = <Config as shikumi::TieredConfig>::prescribed_default();
        let bad_payload = run(Event::Stop, Ok(empty), "{nope").unwrap();
        assert!(bad_payload.get("decision").is_none());
        assert!(bad_payload["systemMessage"]
            .as_str()
            .unwrap()
            .contains("unreadable hook payload"));

        let bad_config = run(Event::SessionStart, Err(anyhow::anyhow!("boom")), "{}").unwrap();
        assert!(bad_config["systemMessage"]
            .as_str()
            .unwrap()
            .contains("config unreadable: boom"));
    }
}
