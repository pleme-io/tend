use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::scan::LocalRepo;

const POLICY_SOURCE: &str = include_str!("../blue/wip.b");
const REMOTE_PREFIX: &str = "refs/remotes/origin/";
const WIP_REFS: &str = "refs/remotes/origin/wip/";
const BASES: [&str; 2] = ["main", "master"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WipConfig {
    pub enabled: bool,
    pub linger_hours: u64,
    pub max_lines: usize,
}

impl Default for WipConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            linger_hours: 24,
            max_lines: 5,
        }
    }
}

impl WipConfig {
    #[must_use]
    pub fn threshold(&self) -> Duration {
        Duration::from_secs(self.linger_hours.saturating_mul(3600))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WipBranch {
    pub workspace: String,
    pub repo: String,
    pub branch: String,
    pub committed_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
}

impl WipBranch {
    pub(crate) fn committed(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.committed_unix)
    }

    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.committed()).unwrap_or_default()
    }

    pub(crate) fn label(&self) -> String {
        format!("{}/{} {}", self.workspace, self.repo, self.branch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Verdict {
    Active,
    Lingering,
    Landed,
}

impl Verdict {
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "active" => Some(Self::Active),
            "lingering" => Some(Self::Lingering),
            "landed" => Some(Self::Landed),
            _ => None,
        }
    }

    pub(crate) fn lingers(self) -> bool {
        matches!(self, Self::Lingering | Self::Landed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Judged {
    #[serde(flatten)]
    pub branch: WipBranch,
    pub age_secs: u64,
    pub verdict: Verdict,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git {args:?} in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_count(word: &str) -> Option<u32> {
    word.parse().ok()
}

pub(crate) fn observe(repo: &LocalRepo) -> Result<Vec<WipBranch>> {
    let mut patterns = vec![WIP_REFS.to_owned()];
    patterns.extend(BASES.iter().map(|b| format!("{REMOTE_PREFIX}{b}")));
    let mut args = vec!["for-each-ref", "--format=%(refname)"];
    args.extend(patterns.iter().map(String::as_str));
    let listed = git(&repo.path, &args)?;
    let refs: Vec<&str> = listed.lines().collect();
    if !refs.iter().any(|r| r.starts_with(WIP_REFS)) {
        return Ok(Vec::new());
    }
    let base = BASES
        .iter()
        .find(|b| refs.contains(&format!("{REMOTE_PREFIX}{b}").as_str()))
        .copied();
    let format = match base {
        Some(b) => format!(
            "--format=%(refname)%09%(committerdate:unix)%09%(ahead-behind:{REMOTE_PREFIX}{b})"
        ),
        None => "--format=%(refname)%09%(committerdate:unix)".to_owned(),
    };
    let detailed = git(&repo.path, &["for-each-ref", &format, WIP_REFS]).or_else(|_| {
        git(
            &repo.path,
            &[
                "for-each-ref",
                "--format=%(refname)%09%(committerdate:unix)",
                WIP_REFS,
            ],
        )
    })?;
    detailed
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next().unwrap_or_default();
            let branch = name
                .strip_prefix(REMOTE_PREFIX)
                .ok_or_else(|| anyhow!("unexpected ref {name}"))?
                .to_owned();
            let committed_unix = fields
                .next()
                .and_then(|t| t.parse().ok())
                .ok_or_else(|| anyhow!("{name}: no committer date in {line:?}"))?;
            let mut counts = fields.next().unwrap_or_default().split(' ');
            let ahead = counts.next().and_then(parse_count);
            let behind = counts.next().and_then(parse_count);
            Ok(WipBranch {
                workspace: repo.workspace.clone(),
                repo: repo.name.clone(),
                branch,
                committed_unix,
                base: base.filter(|_| ahead.is_some()).map(ToOwned::to_owned),
                ahead,
                behind,
            })
        })
        .collect()
}

pub(crate) struct Collected {
    pub branches: Vec<WipBranch>,
    pub unchecked: usize,
}

fn shared_refs(repo: &LocalRepo) -> std::path::PathBuf {
    crate::sync::git_dir(&repo.path)
        .map(|g| crate::sync::common_dir(&g))
        .and_then(|c| c.canonicalize().ok())
        .unwrap_or_else(|| repo.path.clone())
}

pub(crate) fn collect(repos: &[LocalRepo], workers: usize, deadline: Option<Instant>) -> Collected {
    let mut seen = std::collections::BTreeSet::new();
    let distinct: Vec<LocalRepo> = repos
        .iter()
        .filter(|r| seen.insert(shared_refs(r)))
        .cloned()
        .collect();
    let results = crate::scan::parallel(&distinct, workers, deadline, observe);
    let mut branches = Vec::new();
    let mut unchecked = 0usize;
    for result in results {
        match result {
            Some(Ok(found)) => branches.extend(found),
            Some(Err(_)) | None => unchecked += 1,
        }
    }
    Collected {
        branches,
        unchecked,
    }
}

fn int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub(crate) fn judge(
    branches: &[WipBranch],
    threshold: Duration,
    now: SystemTime,
) -> Result<Vec<Judged>> {
    if branches.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<Json> = branches
        .iter()
        .map(|b| json!([int(b.age(now).as_secs()), b.ahead.map_or(-1, i64::from)]))
        .collect();
    let to = blue_lang_runtime::json::json_to_value;
    let mut program = crate::blue_policy::load_pure(None, POLICY_SOURCE, "wip.b")?;
    let out: blue_lang_host::Value = program
        .call(
            "decide",
            (to(&Json::Array(rows)), to(&json!(int(threshold.as_secs())))),
            &mut (),
        )
        .map_err(|e| anyhow!("wip.b: decide(): {e}"))?;
    let out = blue_lang_runtime::json::value_to_json(&out);
    let parts = out
        .as_array()
        .filter(|p| p.len() == 2)
        .ok_or_else(|| anyhow!("wip.b: decide() answered {out}"))?;
    let verdicts = parts[0]
        .as_array()
        .filter(|v| v.len() == branches.len())
        .ok_or_else(|| anyhow!("wip.b: one verdict per branch, got {}", parts[0]))?
        .iter()
        .map(|w| {
            w.as_str()
                .and_then(Verdict::from_word)
                .ok_or_else(|| anyhow!("wip.b: unknown verdict {w}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let order = parts[1]
        .as_array()
        .filter(|o| o.len() == branches.len())
        .ok_or_else(|| anyhow!("wip.b: one position per branch, got {}", parts[1]))?
        .iter()
        .map(|i| {
            i.as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .filter(|i| *i < branches.len())
                .ok_or_else(|| anyhow!("wip.b: bad position {i}"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(order
        .into_iter()
        .map(|i| Judged {
            branch: branches[i].clone(),
            age_secs: branches[i].age(now).as_secs(),
            verdict: verdicts[i],
        })
        .collect())
}

fn counts(b: &WipBranch) -> String {
    match (b.ahead, b.behind, b.base.as_deref()) {
        (Some(0), _, Some(base)) => format!("nothing ahead of {base}"),
        (Some(a), Some(z), Some(base)) => format!("{a} ahead, {z} behind {base}"),
        _ => "no main to compare".to_owned(),
    }
}

fn action(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Active => "being worked",
        Verdict::Lingering => "finish, land, delete",
        Verdict::Landed => "landed; delete it",
    }
}

pub(crate) fn sentence(judged: &[Judged], config: &WipConfig, unchecked: usize) -> Option<String> {
    if judged.is_empty() {
        return None;
    }
    let lingering: Vec<&Judged> = judged.iter().filter(|j| j.verdict.lingers()).collect();
    let active: Vec<&Judged> = judged.iter().filter(|j| !j.verdict.lingers()).collect();
    let mut lines = Vec::new();
    let age = |j: &Judged| crate::reach::human_age(j.age_secs);
    if let Some(oldest) = lingering.first() {
        let n = lingering.len();
        lines.push(format!(
            "tend: {n} wip branch{} linger{} (oldest {}: {} {}); finish, land, delete.",
            if n == 1 { "" } else { "es" },
            if n == 1 { "s" } else { "" },
            age(oldest),
            oldest.branch.repo,
            oldest.branch.branch
        ));
    }
    if !active.is_empty() {
        let n = active.len();
        lines.push(format!(
            "tend: {n} wip branch{} active (younger than {}h): {}.",
            if n == 1 { "" } else { "es" },
            config.linger_hours,
            active
                .iter()
                .map(|j| j.branch.label())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let room = config.max_lines.saturating_sub(lines.len());
    let listed = lingering.len().min(room);
    for j in lingering.iter().take(listed) {
        lines.push(format!(
            "- {}: {} old, {}; {}.",
            j.branch.label(),
            age(j),
            counts(&j.branch),
            action(j.verdict)
        ));
    }
    if lingering.len() > listed && config.max_lines > lines.len() {
        lines.push(format!(
            "- and {} more (tend status lists them).",
            lingering.len() - listed
        ));
    }
    if unchecked > 0 && config.max_lines > lines.len() {
        lines.push(format!(
            "- {unchecked} repo{} not read for wip branches.",
            if unchecked == 1 { "" } else { "s" }
        ));
    }
    lines.truncate(config.max_lines.max(1));
    Some(lines.join("\n"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "from", rename_all = "kebab-case")]
pub(crate) enum Source {
    Snapshot { age_secs: u64 },
    Live { unchecked: usize },
}

pub(crate) struct Report {
    pub source: Source,
    pub judged: Vec<Judged>,
}

pub(crate) fn report(
    snapshot: Option<&crate::scan::Snapshot>,
    workspaces: &[&crate::config::Workspace],
    config: &WipConfig,
    workers: usize,
    deadline: Option<Instant>,
    now: SystemTime,
) -> Result<Report> {
    let names: Vec<&str> = workspaces.iter().map(|w| w.name.as_str()).collect();
    let observed = snapshot.and_then(|s| s.wip.as_ref().map(|w| (s.age(now).as_secs(), w)));
    let (source, branches) = match observed {
        Some((age_secs, branches)) => (
            Source::Snapshot { age_secs },
            branches
                .iter()
                .filter(|b| names.contains(&b.workspace.as_str()))
                .cloned()
                .collect(),
        ),
        None => {
            let repos = crate::scan::local_repos(workspaces);
            let collected = collect(&repos, workers, deadline);
            (
                Source::Live {
                    unchecked: collected.unchecked,
                },
                collected.branches,
            )
        }
    };
    Ok(Report {
        source,
        judged: judge(&branches, config.threshold(), now)?,
    })
}

impl Report {
    pub(crate) fn unchecked(&self) -> usize {
        match self.source {
            Source::Live { unchecked } => unchecked,
            Source::Snapshot { .. } => 0,
        }
    }

    pub(crate) fn to_json(&self, config: &WipConfig) -> Json {
        json!({
            "ok": true,
            "enabled": config.enabled,
            "linger_hours": config.linger_hours,
            "source": self.source,
            "lingering": self.judged.iter().filter(|j| j.verdict.lingers()).count(),
            "branches": self.judged,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        git_env(dir, args, &[]);
    }

    fn git_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) {
        let out = Command::new("git")
            .args(args)
            .envs(env.iter().copied())
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo_with_wip(base: &Path, name: &str, wip: &[(&str, u64)]) -> LocalRepo {
        let upstream = base.join(".upstreams").join(format!("{name}.git"));
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "--bare", "-b", "main"]);
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        git(&path, &["config", "user.email", "t@t"]);
        git(&path, &["config", "user.name", "t"]);
        git(&path, &["config", "commit.gpgsign", "false"]);
        git(
            &path,
            &["remote", "add", "origin", &upstream.to_string_lossy()],
        );
        std::fs::write(path.join("f"), "base\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-q", "--no-verify", "-m", "base"]);
        git(&path, &["push", "-q", "-u", "origin", "main"]);
        for (branch, committed_unix) in wip {
            let date = format!("@{committed_unix} +0000");
            git(&path, &["checkout", "-q", "-b", branch]);
            std::fs::write(path.join("f"), format!("{branch}\n")).unwrap();
            git(&path, &["add", "."]);
            git_env(
                &path,
                &["commit", "-q", "--no-verify", "-m", branch],
                &[("GIT_COMMITTER_DATE", &date), ("GIT_AUTHOR_DATE", &date)],
            );
            git(
                &path,
                &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")],
            );
            git(&path, &["checkout", "-q", "main"]);
            git(&path, &["branch", "-q", "-D", branch]);
        }
        git(&path, &["fetch", "-q", "--prune", "origin"]);
        LocalRepo {
            workspace: "ws".into(),
            name: name.into(),
            path,
        }
    }

    const NOW: u64 = 2_000_000_000;
    const HOUR: u64 = 3600;

    #[test]
    fn a_repo_without_wip_branches_reads_as_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_wip(tmp.path(), "plain", &[]);
        assert!(observe(&repo).unwrap().is_empty());
    }

    #[test]
    fn wip_branches_are_read_from_the_last_fetch_with_date_and_counts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_wip(
            tmp.path(),
            "magma",
            &[("wip/refresh-speed", NOW - 72 * HOUR)],
        );
        let found = observe(&repo).unwrap();
        assert_eq!(found.len(), 1);
        let b = &found[0];
        assert_eq!(b.branch, "wip/refresh-speed");
        assert_eq!(b.committed_unix, NOW - 72 * HOUR);
        assert_eq!(b.base.as_deref(), Some("main"));
        assert_eq!((b.ahead, b.behind), (Some(1), Some(0)));
    }

    #[test]
    fn a_branch_older_than_the_threshold_lingers_and_a_young_one_is_active() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_wip(
            tmp.path(),
            "magma",
            &[
                ("wip/blue-namespaces", NOW - 2 * HOUR),
                ("wip/refresh-speed", NOW - 72 * HOUR),
            ],
        );
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        let config = WipConfig::default();
        let judged = judge(&observe(&repo).unwrap(), config.threshold(), now).unwrap();
        assert_eq!(judged[0].branch.branch, "wip/refresh-speed");
        assert_eq!(judged[0].verdict, Verdict::Lingering);
        assert_eq!(judged[1].verdict, Verdict::Active);
        let text = sentence(&judged, &config, 0).unwrap();
        assert!(
            text.starts_with(
                "tend: 1 wip branch lingers (oldest 3d: magma wip/refresh-speed); finish, land, delete."
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "tend: 1 wip branch active (younger than 24h): ws/magma wip/blue-namespaces."
            ),
            "{text}"
        );
        assert!(
            text.contains("- ws/magma wip/refresh-speed: 3d old, 1 ahead, 0 behind main; finish, land, delete."),
            "{text}"
        );
    }

    fn branch(repo: &str, age_hours: u64, ahead: Option<u32>) -> WipBranch {
        WipBranch {
            workspace: "pleme-io".into(),
            repo: repo.into(),
            branch: format!("wip/{repo}-work"),
            committed_unix: NOW - age_hours * HOUR,
            base: Some("main".into()),
            ahead,
            behind: Some(0),
        }
    }

    #[test]
    fn a_landed_branch_past_the_threshold_is_told_to_be_deleted() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        let judged = judge(
            &[branch("theory", 30, Some(0))],
            WipConfig::default().threshold(),
            now,
        )
        .unwrap();
        assert_eq!(judged[0].verdict, Verdict::Landed);
        let text = sentence(&judged, &WipConfig::default(), 0).unwrap();
        assert!(
            text.contains("nothing ahead of main; landed; delete it."),
            "{text}"
        );
    }

    #[test]
    fn max_lines_bounds_the_warning() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        let branches: Vec<WipBranch> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, r)| branch(r, 30 + i as u64, Some(1)))
            .collect();
        let config = WipConfig {
            max_lines: 3,
            ..WipConfig::default()
        };
        let judged = judge(&branches, config.threshold(), now).unwrap();
        let text = sentence(&judged, &config, 0).unwrap();
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(
            text.starts_with("tend: 6 wip branches linger (oldest 1d: f wip/f-work)"),
            "{text}"
        );
    }

    #[test]
    fn a_worktree_of_a_repo_does_not_count_its_wip_branches_twice() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = repo_with_wip(tmp.path(), "magma", &[("wip/refresh-speed", NOW - 72 * HOUR)]);
        let linked = tmp.path().join("magma-wt-guard");
        git(&repo.path, &["worktree", "add", "-q", "--detach", &linked.to_string_lossy()]);
        let worktree = LocalRepo {
            workspace: "ws".into(),
            name: "magma-wt-guard".into(),
            path: linked,
        };
        let collected = collect(&[repo, worktree], 2, None);
        assert_eq!(collected.unchecked, 0);
        assert_eq!(collected.branches.len(), 1);
        assert_eq!(collected.branches[0].repo, "magma");
    }

    #[test]
    fn no_wip_branches_say_nothing() {
        assert!(sentence(&[], &WipConfig::default(), 0).is_none());
    }
}
