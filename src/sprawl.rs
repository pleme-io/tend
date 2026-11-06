use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::config::Workspace;
use crate::scan::LocalRepo;

const POLICY_SOURCE: &str = include_str!("../blue/sprawl.b");
const SHOWN: &str = "sprawl.b";
const BASES: [&str; 2] = ["main", "master"];
const SEP: char = '\u{1f}';
const HEADS: &str = "refs/heads/";
const ORIGIN: &str = "refs/remotes/origin/";
const BACKUP: &str = "refs/backup/";
const STASH: &str = "refs/stash";
const CHUNK: usize = 400;
const LIST_FORMAT: &str = "--format=%(refname)%1f%(objectname)%1f%(committerdate:unix)%1f%(authorname)%1f%(authoremail:trim)%1f%(upstream:track,nobracket)%1f%(worktreepath)%1f%(symref)";

/// The integration loop's alert: an own branch, local or on origin, whose
/// newest commit is `max_age_hours` old and is not landed on its default
/// branch is an `overdue-branch` alert, named per branch. Off, such a branch
/// lingers as it did before the alert and young work stays active for 72 hours.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct OpenBranchConfig {
    pub enabled: bool,
    pub max_age_hours: u64,
}

impl Default for OpenBranchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_age_hours: 48,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SprawlConfig {
    pub enabled: bool,
    pub max_age_minutes: u64,
    pub workers: usize,
    pub deep_repo_seconds: u64,
    pub scan_minutes: u64,
    pub own_authors: Vec<String>,
    pub report_only_orgs: Vec<String>,
    pub rules: Vec<Json>,
    pub hook_session_start: bool,
    pub hook_stop: bool,
    pub status_line: bool,
    pub daemon: bool,
    pub open_branch: OpenBranchConfig,
    pub alert_lines: usize,
}

impl Default for SprawlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_age_minutes: 30,
            workers: 4,
            deep_repo_seconds: 20,
            scan_minutes: 20,
            own_authors: Vec::new(),
            report_only_orgs: Vec::new(),
            rules: Vec::new(),
            hook_session_start: true,
            hook_stop: true,
            status_line: true,
            daemon: true,
            open_branch: OpenBranchConfig::default(),
            alert_lines: 12,
        }
    }
}

impl SprawlConfig {
    #[must_use]
    pub fn max_age(&self) -> Duration {
        Duration::from_secs(self.max_age_minutes.saturating_mul(60))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Fact {
    pub kind: String,
    pub org: String,
    pub workspace: String,
    pub repo: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_gone: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merges: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_empty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_out: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_merge_base: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    pub fact: Fact,
    pub verdict: String,
    pub rule: String,
    pub severity: String,
    pub next: String,
    pub report_only: bool,
    pub rank: u32,
    pub announce: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_hours: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Blind {
    pub workspace: String,
    pub org: String,
    pub repo: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub taken_at_unix: u64,
    pub scan_secs: f64,
    pub on_disk: usize,
    pub total: usize,
    pub read: usize,
    pub workers: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttled: Option<String>,
    pub deep_unknown: usize,
    pub blind: Vec<Blind>,
    pub rows: Vec<Row>,
    pub classes: Json,
    pub refusals: Vec<String>,
    pub off: Vec<String>,
    pub digest: u64,
}

impl Snapshot {
    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        let taken = SystemTime::UNIX_EPOCH + Duration::from_secs(self.taken_at_unix);
        now.duration_since(taken).unwrap_or_default()
    }
}

pub(crate) fn snapshot_path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("sprawl")
        .join("snapshot.json")
}

pub(crate) fn read_snapshot(path: &Path) -> Option<Snapshot> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(snapshot)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn git_command(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(repo)
        .args(args);
    cmd
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git_command(repo, args)
        .output()
        .with_context(|| format!("running git in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_count(repo: &Path, args: &[&str]) -> Option<u32> {
    git(repo, args).ok()?.trim().parse().ok()
}

fn non_empty(word: &str) -> Option<String> {
    (!word.is_empty()).then(|| word.to_owned())
}

struct RefLine {
    name: String,
    sha: String,
    committed: Option<u64>,
    author: Option<String>,
    email: Option<String>,
    track: String,
    worktree: String,
    symref: String,
}

fn list_refs(repo: &Path) -> Result<Vec<RefLine>> {
    let out = git(
        repo,
        &[
            "for-each-ref",
            LIST_FORMAT,
            "refs/heads",
            "refs/remotes/origin",
            "refs/backup",
            STASH,
        ],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut f = line.split(SEP);
            let mut next = || f.next().unwrap_or_default();
            RefLine {
                name: next().to_owned(),
                sha: next().to_owned(),
                committed: next().parse().ok(),
                author: non_empty(next()),
                email: non_empty(next()),
                track: next().to_owned(),
                worktree: next().to_owned(),
                symref: next().to_owned(),
            }
        })
        .collect())
}

fn base_of(refs: &[RefLine]) -> Option<(String, String)> {
    let named = |name: &str| refs.iter().find(|r| r.name == format!("{ORIGIN}{name}"));
    let head = refs
        .iter()
        .find(|r| r.name == format!("{ORIGIN}HEAD"))
        .and_then(|r| r.symref.strip_prefix(ORIGIN))
        .and_then(|short| named(short).map(|r| (short.to_owned(), r.sha.clone())));
    head.or_else(|| {
        BASES
            .iter()
            .find_map(|b| named(b).map(|r| ((*b).to_owned(), r.sha.clone())))
    })
}

fn ahead_behind(repo: &Path, base: &str) -> HashMap<String, (u32, u32)> {
    let format = format!("--format=%(refname)%1f%(ahead-behind:{ORIGIN}{base})");
    let Ok(out) = git(
        repo,
        &[
            "for-each-ref",
            &format,
            "refs/heads",
            "refs/remotes/origin",
            "refs/backup",
        ],
    ) else {
        return HashMap::new();
    };
    out.lines()
        .filter_map(|line| {
            let (name, counts) = line.split_once(SEP)?;
            let (a, b) = counts.split_once(' ')?;
            Some((name.to_owned(), (a.parse().ok()?, b.parse().ok()?)))
        })
        .collect()
}

type DeepFacts = (Option<u32>, Option<u32>, Option<bool>);

pub(crate) type Memo = HashMap<(String, String, String, String), DeepFacts>;

pub(crate) fn memo_of(snapshot: Option<&Snapshot>) -> Memo {
    snapshot
        .map(|s| {
            s.rows
                .iter()
                .filter_map(|r| {
                    let f = &r.fact;
                    Some((
                        (
                            f.path.clone(),
                            f.reference.clone(),
                            f.sha.clone()?,
                            f.base_sha.clone()?,
                        ),
                        (f.unique, f.merges, f.diff_empty),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn deep_facts(repo: &Path, base: &str, sha: &str) -> DeepFacts {
    let range = format!("{ORIGIN}{base}...{sha}");
    let two_dot = format!("{ORIGIN}{base}..{sha}");
    let unique = git_count(
        repo,
        &[
            "rev-list",
            "--count",
            "--cherry-pick",
            "--right-only",
            "--no-merges",
            &range,
        ],
    );
    let merges = git_count(repo, &["rev-list", "--count", "--merges", &two_dot]);
    let diff_empty = git_command(repo, &["diff", "--quiet", &range])
        .output()
        .ok()
        .and_then(|o| match o.status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        });
    (unique, merges, diff_empty)
}

fn default_name(refs: &[RefLine], base: Option<&str>) -> Option<String> {
    base.map(ToOwned::to_owned).or_else(|| {
        BASES
            .iter()
            .find(|b| refs.iter().any(|r| r.name == format!("{HEADS}{b}")))
            .map(|b| (*b).to_owned())
    })
}

struct Place<'a> {
    org: &'a str,
    repo: &'a LocalRepo,
}

impl Place<'_> {
    fn fact(&self, kind: &str, reference: String, branch: String) -> Fact {
        Fact {
            kind: kind.to_owned(),
            org: self.org.to_owned(),
            workspace: self.repo.workspace.clone(),
            repo: self.repo.name.clone(),
            path: self.repo.path.to_string_lossy().into_owned(),
            reference,
            branch,
            ..Fact::default()
        }
    }
}

fn ref_facts(
    place: &Place,
    refs: &[RefLine],
    base: Option<&(String, String)>,
    memo: &Memo,
    deep_until: Instant,
) -> Vec<Fact> {
    let repo = &place.repo.path;
    let base_name = base.map(|(n, _)| n.as_str());
    let default = default_name(refs, base_name);
    let kept: Vec<&RefLine> = refs
        .iter()
        .filter(|r| {
            let local = r.name.strip_prefix(HEADS);
            let remote = r.name.strip_prefix(ORIGIN);
            r.symref.is_empty()
                && r.name != STASH
                && remote != Some("HEAD")
                && !(local.is_some() && local == default.as_deref())
                && !(remote.is_some() && remote == default.as_deref())
        })
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }
    let counts = base_name.map(|b| ahead_behind(repo, b)).unwrap_or_default();
    let on_origin: std::collections::HashSet<&str> = refs
        .iter()
        .filter_map(|r| r.name.strip_prefix(ORIGIN))
        .collect();
    let path = repo.to_string_lossy().into_owned();
    let mut seen: HashMap<&str, DeepFacts> = HashMap::new();
    kept.iter()
        .map(|r| {
            let (kind, short) = if let Some(s) = r.name.strip_prefix(HEADS) {
                ("branch", s)
            } else if let Some(s) = r.name.strip_prefix(ORIGIN) {
                ("remote-branch", s)
            } else {
                ("backup-ref", r.name.strip_prefix(BACKUP).unwrap_or(&r.name))
            };
            let shown = match kind {
                "branch" => short.to_owned(),
                "remote-branch" => format!("origin/{short}"),
                _ => r.name.clone(),
            };
            let mut fact = place.fact(kind, shown, short.to_owned());
            fact.sha = non_empty(&r.sha);
            fact.committed_unix = r.committed;
            fact.author = r.author.clone();
            fact.email = r.email.clone();
            fact.upstream_gone = (kind == "branch").then_some(r.track == "gone");
            fact.checked_out = (kind == "branch").then_some(!r.worktree.is_empty());
            fact.pushed = match kind {
                "branch" => Some(on_origin.contains(short)),
                "remote-branch" => Some(true),
                _ => None,
            };
            if let Some((name, sha)) = base {
                fact.base = Some(format!("origin/{name}"));
                fact.base_sha = Some(sha.clone());
                if let Some((a, b)) = counts.get(&r.name) {
                    fact.ahead = Some(*a);
                    fact.behind = Some(*b);
                    if *a == 0 {
                        fact.unique = Some(0);
                        fact.merges = Some(0);
                        fact.diff_empty = Some(true);
                    } else if kind != "backup-ref" {
                        let key = (
                            path.clone(),
                            fact.reference.clone(),
                            r.sha.clone(),
                            sha.clone(),
                        );
                        let known = seen
                            .get(r.sha.as_str())
                            .copied()
                            .or_else(|| memo.get(&key).copied().filter(|m| m.0.is_some()))
                            .or_else(|| {
                                (Instant::now() < deep_until)
                                    .then(|| deep_facts(repo, name, &r.sha))
                            });
                        if let Some(known) = known {
                            seen.insert(r.sha.as_str(), known);
                            (fact.unique, fact.merges, fact.diff_empty) = known;
                        }
                    }
                }
            }
            fact
        })
        .collect()
}

fn stash_facts(place: &Place) -> Result<Vec<Fact>> {
    let out = git(
        &place.repo.path,
        &["stash", "list", "--format=%gd%x1f%ct%x1f%an%x1f%ae%x1f%gs"],
    )?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut f = line.split(SEP);
            let mut next = || f.next().unwrap_or_default();
            let reference = next().to_owned();
            let committed = next().parse().ok();
            let author = non_empty(next());
            let email = non_empty(next());
            let message = next().to_owned();
            let mut fact = place.fact("stash", reference, String::new());
            fact.committed_unix = committed;
            fact.author = author;
            fact.email = email;
            fact.message = Some(message);
            fact
        })
        .collect())
}

fn worktree_facts(place: &Place) -> Result<Vec<Fact>> {
    let repo = &place.repo.path;
    let linked = crate::sync::git_dir(repo)
        .map(|g| crate::sync::common_dir(&g).join("worktrees"))
        .is_some_and(|w| w.is_dir());
    if !linked {
        return Ok(Vec::new());
    }
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    Ok(out
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .skip(1)
        .filter_map(|block| {
            let field = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(|v| v.trim().to_owned())
            };
            let path = field("worktree ")?;
            let exists = Path::new(&path).exists();
            let branch = field("branch ")
                .map(|b| b.strip_prefix(HEADS).unwrap_or(&b).to_owned())
                .unwrap_or_default();
            let mut fact = place.fact("worktree", path.clone(), branch);
            fact.exists = Some(exists);
            fact.sha = field("HEAD ");
            if let Some(sha) = fact.sha.clone() {
                fact.committed_unix = git(repo, &["log", "-1", "--format=%ct", &sha])
                    .ok()
                    .and_then(|t| t.trim().parse().ok());
            }
            if exists {
                fact.dirty = git(Path::new(&path), &["status", "--porcelain"])
                    .ok()
                    .map(|t| !t.trim().is_empty());
            }
            Some(fact)
        })
        .collect())
}

fn default_branch_fact(
    place: &Place,
    refs: &[RefLine],
    base: Option<&(String, String)>,
) -> Option<Fact> {
    let (name, remote_sha) = base?;
    let local = refs.iter().find(|r| r.name == format!("{HEADS}{name}"))?;
    if &local.sha == remote_sha {
        return None;
    }
    let status = git_command(&place.repo.path, &["merge-base", &local.sha, remote_sha])
        .output()
        .ok()?
        .status
        .code();
    if status == Some(0) {
        return None;
    }
    let mut fact = place.fact("default-branch", name.clone(), name.clone());
    fact.sha = non_empty(&local.sha);
    fact.committed_unix = local.committed;
    fact.author = local.author.clone();
    fact.email = local.email.clone();
    fact.base = Some(format!("origin/{name}"));
    fact.base_sha = Some(remote_sha.clone());
    fact.no_merge_base = (status == Some(1)).then_some(true);
    Some(fact)
}

pub(crate) fn observe(
    repo: &LocalRepo,
    org: &str,
    memo: &Memo,
    deep_until: Instant,
) -> Result<Vec<Fact>> {
    let place = Place { org, repo };
    let refs = list_refs(&repo.path)?;
    let base = base_of(&refs);
    let mut facts = ref_facts(&place, &refs, base.as_ref(), memo, deep_until);
    if refs.iter().any(|r| r.name == STASH) {
        facts.extend(stash_facts(&place)?);
    }
    facts.extend(worktree_facts(&place)?);
    facts.extend(default_branch_fact(&place, &refs, base.as_ref()));
    Ok(facts)
}

fn orgs_of(workspaces: &[&Workspace]) -> BTreeMap<String, String> {
    workspaces
        .iter()
        .map(|w| {
            (
                w.name.clone(),
                w.org.clone().unwrap_or_else(|| w.name.clone()),
            )
        })
        .collect()
}

pub(crate) struct Scan {
    pub facts: Vec<Fact>,
    pub blind: Vec<Blind>,
    pub on_disk: usize,
    pub total: usize,
    pub read: usize,
}

pub(crate) fn scan(
    workspaces: &[&Workspace],
    config: &SprawlConfig,
    workers: usize,
    memo: &Memo,
    deadline: Option<Instant>,
) -> Scan {
    let orgs = orgs_of(workspaces);
    let on_disk = crate::scan::local_repos(workspaces);
    let repos = crate::wip::distinct(&on_disk);
    let org_of = |r: &LocalRepo| {
        orgs.get(&r.workspace)
            .cloned()
            .unwrap_or_else(|| r.workspace.clone())
    };
    let deep = Duration::from_secs(config.deep_repo_seconds);
    let results = crate::scan::parallel(&repos, workers, deadline, |repo| {
        observe(repo, &org_of(repo), memo, Instant::now() + deep)
    });
    let mut facts = Vec::new();
    let mut blind = Vec::new();
    let mut read = 0usize;
    for (repo, result) in repos.iter().zip(results) {
        let mut unread = |why: String| {
            blind.push(Blind {
                workspace: repo.workspace.clone(),
                org: org_of(repo),
                repo: repo.name.clone(),
                why,
            });
        };
        match result {
            Some(Ok(found)) => {
                read += 1;
                facts.extend(found);
            }
            Some(Err(e)) => unread(format!("{e:#}")),
            None => unread("not reached before the scan budget".to_owned()),
        }
    }
    Scan {
        facts,
        blind,
        on_disk: on_disk.len(),
        total: repos.len(),
        read,
    }
}

fn identity() -> Vec<String> {
    ["user.name", "user.email"]
        .iter()
        .filter_map(|key| {
            let out = Command::new("git")
                .args(["config", "--global", key])
                .output()
                .ok()?;
            non_empty(String::from_utf8_lossy(&out.stdout).trim())
        })
        .collect()
}

pub(crate) struct Policy {
    program: blue_lang_host::Program<()>,
    standing: Json,
    ctx: Json,
    pub refusals: Vec<String>,
    pub off: Vec<String>,
}

fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn call(program: &mut blue_lang_host::Program<()>, name: &str, args: &[Json]) -> Result<Json> {
    crate::blue_policy::call_on(program, SHOWN, name, args)
}

impl Policy {
    pub(crate) fn load(config: &SprawlConfig, now: SystemTime) -> Result<Self> {
        let mut program = crate::blue_policy::load_pure(None, POLICY_SOURCE, SHOWN)?;
        let standing = call(
            &mut program,
            "wire_rules",
            &[Json::Array(config.rules.clone()), json!(config.open_branch)],
        )?;
        let mut own = config.own_authors.clone();
        own.extend(identity());
        let ctx = call(
            &mut program,
            "context",
            &[
                json!(crate::heartbeat::unix(now)),
                json!(own),
                json!(config.report_only_orgs),
            ],
        )?;
        let refusals = strings(&standing["refusals"]);
        let off = strings(&standing["off"]);
        Ok(Self {
            program,
            standing,
            ctx,
            refusals,
            off,
        })
    }

    pub(crate) fn judge(&mut self, facts: Vec<Fact>) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(facts.len());
        for chunk in facts.chunks(CHUNK) {
            let items = serde_json::to_value(chunk)?;
            let out = call(
                &mut self.program,
                "judge_items",
                &[items, self.standing["rules"].clone(), self.ctx.clone()],
            )?;
            let answers = out
                .as_array()
                .filter(|a| a.len() == chunk.len())
                .ok_or_else(|| anyhow!("{SHOWN}: one row per item, got {out}"))?;
            for (fact, answer) in chunk.iter().zip(answers) {
                rows.push(decode(fact.clone(), answer)?);
            }
        }
        rows.sort_by_key(|r| (r.rank, std::cmp::Reverse(r.age_hours)));
        Ok(rows)
    }

    pub(crate) fn classes(&mut self, rows: &[Row]) -> Result<Json> {
        let small: Vec<Json> = rows
            .iter()
            .map(|r| {
                json!([
                    r.verdict,
                    r.severity,
                    r.rank,
                    r.announce,
                    format!("{}/{}", r.fact.workspace, r.fact.repo)
                ])
            })
            .collect();
        call(&mut self.program, "classes", &[Json::Array(small)])
    }

    pub(crate) fn validate(&mut self, config: &SprawlConfig) -> Result<Json> {
        call(
            &mut self.program,
            "validate",
            &[Json::Array(config.rules.clone()), json!(config.open_branch)],
        )
    }
}

fn decode(fact: Fact, answer: &Json) -> Result<Row> {
    let a = answer
        .as_array()
        .filter(|a| a.len() == 9)
        .ok_or_else(|| anyhow!("{SHOWN}: bad row {answer}"))?;
    let text = |i: usize| a[i].as_str().unwrap_or_default().to_owned();
    Ok(Row {
        fact,
        verdict: text(0),
        rule: text(1),
        severity: text(2),
        next: text(3),
        report_only: a[4].as_bool().unwrap_or(true),
        rank: a[5]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(2),
        announce: a[6].as_bool().unwrap_or(true),
        age_hours: a[7].as_i64(),
        landed: a[8].as_bool(),
    })
}

pub(crate) fn digest(config: &SprawlConfig) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    POLICY_SOURCE.hash(&mut hasher);
    serde_json::to_string(&config.rules)
        .unwrap_or_default()
        .hash(&mut hasher);
    config.own_authors.hash(&mut hasher);
    config.report_only_orgs.hash(&mut hasher);
    config.open_branch.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn refresh(
    workspaces: &[&Workspace],
    config: &SprawlConfig,
    workers: usize,
    throttled: Option<String>,
    path: &Path,
    now: SystemTime,
) -> Result<Snapshot> {
    let started = Instant::now();
    let previous = read_snapshot(path);
    let memo = memo_of(previous.as_ref());
    let deadline = started + Duration::from_secs(config.scan_minutes.saturating_mul(60));
    let scanned = scan(workspaces, config, workers, &memo, Some(deadline));
    let mut policy = Policy::load(config, now)?;
    let deep_unknown = scanned
        .facts
        .iter()
        .filter(|f| f.ahead.is_some_and(|a| a > 0) && f.kind != "backup-ref" && f.unique.is_none())
        .count();
    let rows = policy.judge(scanned.facts)?;
    let classes = policy.classes(&rows)?;
    let snapshot = Snapshot {
        taken_at_unix: crate::heartbeat::unix(now),
        scan_secs: started.elapsed().as_secs_f64(),
        on_disk: scanned.on_disk,
        total: scanned.total,
        read: scanned.read,
        workers,
        throttled,
        deep_unknown,
        blind: scanned.blind,
        rows,
        classes,
        refusals: policy.refusals.clone(),
        off: policy.off.clone(),
        digest: digest(config),
    };
    write_snapshot(path, &snapshot)?;
    Ok(snapshot)
}

pub(crate) fn admitted_workers(
    config: &SprawlConfig,
    probe: &Path,
) -> std::result::Result<(usize, Option<String>), String> {
    let reader = crate::pressure::SystemPressureReader {
        path: probe.to_path_buf(),
    };
    let Ok(reading) = crate::pressure::PressureReader::read(&reader) else {
        return Ok((config.workers, None));
    };
    let wanted = u32::try_from(config.workers).unwrap_or(u32::MAX);
    let verdict = crate::pressure::assess(reading, crate::pressure::Thresholds::default(), wanted);
    match verdict.inflight(wanted) {
        Some(n) if n == wanted => Ok((config.workers, None)),
        Some(n) => Ok((n as usize, Some(verdict.why().to_owned()))),
        None => Err(verdict.why().to_owned()),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Filters {
    pub org: Option<String>,
    pub repo: Option<String>,
    pub verdict: Option<String>,
    pub limit: Option<usize>,
}

impl Filters {
    fn admits(&self, row: &Row) -> bool {
        self.org.as_ref().is_none_or(|o| *o == row.fact.org)
            && self.repo.as_ref().is_none_or(|r| *r == row.fact.repo)
            && self.verdict.as_ref().is_none_or(|v| *v == row.verdict)
    }

    fn sees(&self, blind: &Blind) -> bool {
        self.org.as_ref().is_none_or(|o| *o == blind.org)
            && self.repo.as_ref().is_none_or(|r| *r == blind.repo)
    }
}

pub(crate) struct Freshness {
    pub stale: bool,
    pub why: String,
}

pub(crate) fn freshness(
    snapshot: &Snapshot,
    config: &SprawlConfig,
    now: SystemTime,
    liveness: impl FnOnce() -> crate::heartbeat::Liveness,
) -> Freshness {
    if snapshot.age(now) > config.max_age() {
        Freshness {
            stale: true,
            why: liveness().sentence(),
        }
    } else {
        Freshness {
            stale: false,
            why: String::new(),
        }
    }
}

fn live_liveness(now: SystemTime) -> crate::heartbeat::Liveness {
    crate::heartbeat::observe(
        &crate::heartbeat::path(),
        now,
        crate::heartbeat::process_named_tend,
    )
}

pub(crate) fn report(
    config: &SprawlConfig,
    snapshot: Option<Snapshot>,
    filters: &Filters,
    default_limit: usize,
    now: SystemTime,
    liveness: impl FnOnce() -> crate::heartbeat::Liveness,
) -> Result<Json> {
    let mut program = crate::blue_policy::load_pure(None, POLICY_SOURCE, SHOWN)?;
    let Some(mut snapshot) = snapshot else {
        let outcome = call(&mut program, "outcome", &[json!(0), json!(0), json!(false)])?;
        return Ok(json!({
            "ok": true,
            "enabled": config.enabled,
            "outcome": outcome,
            "why": if config.enabled {
                "no sprawl snapshot on this host; run `tend sprawl refresh`"
            } else {
                "sprawl.enabled is false"
            },
        }));
    };
    let mut policy = Policy::load(config, now)?;
    if snapshot.digest != digest(config) {
        let facts = snapshot.rows.drain(..).map(|r| r.fact).collect();
        snapshot.rows = policy.judge(facts)?;
        snapshot.refusals.clone_from(&policy.refusals);
        snapshot.off.clone_from(&policy.off);
    }
    let matched: Vec<Row> = snapshot
        .rows
        .iter()
        .filter(|r| filters.admits(r))
        .cloned()
        .collect();
    let classes = policy.classes(&matched)?;
    let blind: Vec<&Blind> = snapshot.blind.iter().filter(|b| filters.sees(b)).collect();
    let outcome = call(
        &mut program,
        "outcome",
        &[json!(matched.len()), json!(blind.len()), json!(true)],
    )?;
    let fresh = freshness(&snapshot, config, now, liveness);
    let limit = filters.limit.unwrap_or(default_limit);
    let items: Vec<Json> = matched
        .iter()
        .take(limit)
        .map(|r| {
            let mut row = r.clone();
            row.fact.email = None;
            serde_json::to_value(row).unwrap_or_default()
        })
        .collect();
    Ok(json!({
        "ok": true,
        "enabled": config.enabled,
        "outcome": outcome,
        "age_secs": snapshot.age(now).as_secs(),
        "stale": fresh.stale,
        "stale_why": fresh.why,
        "throttled": snapshot.throttled,
        "scan_secs": snapshot.scan_secs,
        "on_disk": snapshot.on_disk,
        "total": snapshot.total,
        "read": snapshot.read,
        "deep_unknown": snapshot.deep_unknown,
        "blind_count": snapshot.blind.len(),
        "blind_in_scope": blind.len(),
        "blind": blind.iter().take(limit).collect::<Vec<_>>(),
        "classes": classes,
        "matched": matched.len(),
        "returned": items.len(),
        "items": items,
        "off": snapshot.off,
        "refusals": snapshot.refusals,
    }))
}

pub(crate) fn mcp_report(
    config: &SprawlConfig,
    filters: &Filters,
    now: SystemTime,
) -> Result<Json> {
    report(
        config,
        config
            .enabled
            .then(|| read_snapshot(&snapshot_path()))
            .flatten(),
        filters,
        100,
        now,
        || live_liveness(now),
    )
}

fn meta(snapshot: Option<&Snapshot>, config: &SprawlConfig, now: SystemTime) -> Json {
    match snapshot {
        Some(s) => {
            let fresh = freshness(s, config, now, || live_liveness(now));
            json!({
                "snapshot": true,
                "stale": fresh.stale,
                "why": fresh.why,
                "age_secs": s.age(now).as_secs(),
                "blind": s.blind.len(),
                "total": s.total,
                "read": s.read,
            })
        }
        None => json!({
            "snapshot": false,
            "why": live_liveness(now).sentence(),
        }),
    }
}

pub(crate) fn session_lines(config: &SprawlConfig, path: &Path, now: SystemTime) -> Vec<String> {
    let snapshot = read_snapshot(path);
    let classes = snapshot.as_ref().map_or(json!([]), |s| s.classes.clone());
    let meta = meta(snapshot.as_ref(), config, now);
    let alerts = snapshot
        .as_ref()
        .map_or_else(Vec::new, |s| alert_rows(&s.rows));
    let answered = crate::blue_policy::load_pure(None, POLICY_SOURCE, SHOWN).and_then(|mut p| {
        let mut lines = strings(&call(&mut p, "session_lines", &[classes, meta])?);
        lines.extend(strings(&call(
            &mut p,
            "alert_lines",
            &[Json::Array(alerts), json!(config.alert_lines)],
        )?));
        Ok(lines)
    });
    match answered {
        Ok(lines) => lines,
        Err(e) => vec![format!("tend sprawl: blind, judgement failed: {e:#}")],
    }
}

fn alert_rows(rows: &[Row]) -> Vec<Json> {
    rows.iter()
        .filter(|r| {
            r.rank == 0 && r.announce && matches!(r.fact.kind.as_str(), "branch" | "remote-branch")
        })
        .map(|r| {
            json!([
                r.verdict,
                format!("{}/{}", r.fact.org, r.fact.repo),
                r.fact.branch,
                r.age_hours.unwrap_or_default(),
                r.fact.pushed.unwrap_or(false)
            ])
        })
        .collect()
}

pub(crate) fn status_text(config: &SprawlConfig, path: &Path, now: SystemTime) -> String {
    let snapshot = read_snapshot(path);
    let classes = snapshot.as_ref().map_or(json!([]), |s| s.classes.clone());
    let meta = meta(snapshot.as_ref(), config, now);
    crate::blue_policy::load_pure(None, POLICY_SOURCE, SHOWN)
        .and_then(|mut p| call(&mut p, "status_line", &[classes, meta]))
        .ok()
        .and_then(|v| v.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "sprawl: blind (judgement failed)".to_owned())
}

pub(crate) struct TouchedNote {
    pub lines: Vec<String>,
    pub unread: usize,
}

pub(crate) fn touched_note(
    workspaces: &[&Workspace],
    repos: &[LocalRepo],
    config: &SprawlConfig,
    path: &Path,
    budget: Duration,
    now: SystemTime,
) -> Result<TouchedNote> {
    let deadline = Instant::now() + budget;
    let orgs = orgs_of(workspaces);
    let memo = memo_of(read_snapshot(path).as_ref());
    let org_of = |r: &LocalRepo| {
        orgs.get(&r.workspace)
            .cloned()
            .unwrap_or_else(|| r.workspace.clone())
    };
    let distinct = crate::wip::distinct(repos);
    let results = crate::scan::parallel(&distinct, 4, Some(deadline), |repo| {
        observe(repo, &org_of(repo), &memo, deadline)
    });
    let mut facts = Vec::new();
    let mut unread = 0usize;
    for result in results {
        match result {
            Some(Ok(found)) => facts.extend(found),
            Some(Err(_)) | None => unread += 1,
        }
    }
    let mut policy = Policy::load(config, now)?;
    let rows = policy.judge(facts)?;
    let small: Vec<Json> = rows
        .iter()
        .map(|r| json!([r.fact.repo, r.verdict, r.rank, r.announce]))
        .collect();
    let lines = call(&mut policy.program, "touched_lines", &[Json::Array(small)])?;
    Ok(TouchedNote {
        lines: strings(&lines),
        unread,
    })
}

pub(crate) fn print_validate(answer: &Json) {
    let count = |key: &str| answer[key].as_array().map_or(0, Vec::len);
    println!(
        "sprawl rules: {} standing, {} off, {} refused, {} failing",
        count("rules"),
        count("off"),
        count("refusals"),
        count("failures"),
    );
    for refusal in answer["refusals"].as_array().into_iter().flatten() {
        println!("refused: {}", refusal.as_str().unwrap_or_default());
    }
    for failure in answer["failures"].as_array().into_iter().flatten() {
        println!(
            "failed: {}: {} expected {}, got {}",
            failure["rule"].as_str().unwrap_or_default(),
            failure["item"].as_str().unwrap_or_default(),
            failure["expected"].as_str().unwrap_or_default(),
            failure["got"].as_str().unwrap_or_default(),
        );
    }
}

pub(crate) fn print_report(report: &Json) {
    let outcome = report["outcome"].as_str().unwrap_or("blind");
    if report.get("age_secs").is_none() {
        println!(
            "sprawl: blind: {}",
            report["why"].as_str().unwrap_or_default()
        );
        return;
    }
    println!(
        "sprawl: {outcome}; snapshot {} old; {} of {} repos read, {} blind; scan took {:.1}s",
        crate::reach::human_age(report["age_secs"].as_u64().unwrap_or(0)),
        report["read"],
        report["total"],
        report["blind_count"],
        report["scan_secs"].as_f64().unwrap_or(0.0),
    );
    if report["stale"].as_bool().unwrap_or(false) {
        println!(
            "stale: {}",
            report["stale_why"].as_str().unwrap_or_default()
        );
    }
    if let Some(why) = report["throttled"].as_str() {
        println!("throttled: {why}");
    }
    if report["deep_unknown"].as_u64().unwrap_or(0) > 0 {
        println!(
            "{} branches over the per-repo budget: counts unknown, so read as lingering, never landed",
            report["deep_unknown"]
        );
    }
    for class in report["classes"].as_array().into_iter().flatten() {
        println!(
            "  {:<8} {:>6} {:<26} in {} repos",
            class["severity"].as_str().unwrap_or_default(),
            class["count"],
            class["verdict"].as_str().unwrap_or_default(),
            class["repos"],
        );
    }
    for row in report["items"].as_array().into_iter().flatten() {
        println!(
            "- {}/{} {} [{}]: {}",
            row["workspace"].as_str().unwrap_or_default(),
            row["repo"].as_str().unwrap_or_default(),
            row["ref"].as_str().unwrap_or_default(),
            row["verdict"].as_str().unwrap_or_default(),
            row["next"].as_str().unwrap_or_default(),
        );
    }
    let matched = report["matched"].as_u64().unwrap_or(0);
    let returned = report["returned"].as_u64().unwrap_or(0);
    if matched > returned {
        println!(
            "and {} more (--limit, --verdict, --repo, --org)",
            matched - returned
        );
    }
    for blind in report["blind"].as_array().into_iter().flatten() {
        println!(
            "blind: {}/{}: {}",
            blind["workspace"].as_str().unwrap_or_default(),
            blind["repo"].as_str().unwrap_or_default(),
            blind["why"].as_str().unwrap_or_default(),
        );
    }
    for refusal in report["refusals"].as_array().into_iter().flatten() {
        println!("refused: {}", refusal.as_str().unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 2_000_000_000;
    const DAY: u64 = 86_400;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn git_ok(dir: &Path, args: &[&str]) {
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

    fn commit(dir: &Path, name: &str, body: &str, author: &str, when: u64) {
        std::fs::write(dir.join(name), body).unwrap();
        git_ok(dir, &["add", "."]);
        let date = format!("@{when} +0000");
        let email = format!("{author}@example.invalid");
        git_env(
            dir,
            &["commit", "-q", "--no-verify", "-m", name],
            &[
                ("GIT_AUTHOR_NAME", author),
                ("GIT_AUTHOR_EMAIL", &email),
                ("GIT_COMMITTER_DATE", &date),
                ("GIT_AUTHOR_DATE", &date),
            ],
        );
    }

    fn fixture(base: &Path, name: &str) -> LocalRepo {
        let upstream = base.join(".upstreams").join(format!("{name}.git"));
        std::fs::create_dir_all(&upstream).unwrap();
        git_ok(&upstream, &["init", "-q", "--bare", "-b", "main"]);
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        git_ok(&path, &["init", "-q", "-b", "main"]);
        git_ok(&path, &["config", "user.email", "me@example.invalid"]);
        git_ok(&path, &["config", "user.name", "me"]);
        git_ok(&path, &["config", "commit.gpgsign", "false"]);
        git_ok(
            &path,
            &["remote", "add", "origin", &upstream.to_string_lossy()],
        );
        commit(&path, "base", "base\n", "me", NOW - 30 * DAY);
        git_ok(&path, &["push", "-q", "-u", "origin", "main"]);
        LocalRepo {
            workspace: "ws".into(),
            name: name.into(),
            path,
        }
    }

    fn on_branch(repo: &Path, branch: &str, file: &str, body: &str, author: &str, when: u64) {
        git_ok(repo, &["checkout", "-q", "-b", branch, "main"]);
        commit(repo, file, body, author, when);
        git_ok(repo, &["checkout", "-q", "main"]);
    }

    fn config() -> SprawlConfig {
        SprawlConfig {
            own_authors: vec!["me".into()],
            ..SprawlConfig::default()
        }
    }

    fn workspace(base: &Path) -> Workspace {
        let mut ws = Workspace::test_default("ws");
        ws.base_dir = base.to_string_lossy().into_owned();
        ws.org = Some("pleme-io".into());
        ws
    }

    fn judged(repo: &LocalRepo) -> Vec<Row> {
        let facts = observe(
            repo,
            "pleme-io",
            &Memo::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        Policy::load(&config(), at(NOW))
            .unwrap()
            .judge(facts)
            .unwrap()
    }

    fn find<'a>(rows: &'a [Row], kind: &str, reference: &str) -> &'a Row {
        rows.iter()
            .find(|r| r.fact.kind == kind && r.fact.reference == reference)
            .unwrap_or_else(|| panic!("no {kind} {reference} in {rows:#?}"))
    }

    #[test]
    fn a_repo_with_only_its_default_branch_reads_as_no_items() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "plain");
        assert!(judged(&repo).is_empty());
    }

    #[test]
    fn a_rebased_and_landed_branch_is_landed_though_ancestry_says_ahead() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        let old = NOW - 20 * DAY;
        on_branch(&repo.path, "rebased", "a", "a\n", "me", old);
        commit(&repo.path, "b", "b\n", "me", old - 10);
        git_ok(&repo.path, &["cherry-pick", "rebased"]);
        git_ok(&repo.path, &["push", "-q", "origin", "main"]);
        git_ok(&repo.path, &["fetch", "-q", "origin"]);
        let rows = judged(&repo);
        let row = find(&rows, "branch", "rebased");
        assert_eq!(row.fact.ahead, Some(1), "ancestry says ahead");
        assert_eq!(row.fact.unique, Some(0), "patch says nothing is new");
        assert_eq!(row.verdict, "landed");
        assert!(row.next.contains("branch -D rebased"), "{}", row.next);
    }

    #[test]
    fn unique_commits_past_the_age_are_overdue_and_the_count_agrees_with_git_log() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        let old = NOW - 20 * DAY;
        git_ok(&repo.path, &["checkout", "-q", "-b", "absorb/work", "main"]);
        for i in 0..3 {
            commit(&repo.path, &format!("f{i}"), "x\n", "me", old + i);
        }
        git_ok(
            &repo.path,
            &["push", "-q", "origin", "HEAD:refs/heads/absorb/work"],
        );
        git_ok(&repo.path, &["checkout", "-q", "main"]);
        git_ok(&repo.path, &["fetch", "-q", "origin"]);
        let rows = judged(&repo);
        let local = find(&rows, "branch", "absorb/work");
        let counted = git(
            &repo.path,
            &["log", "--oneline", "origin/main..absorb/work"],
        )
        .unwrap()
        .lines()
        .count();
        assert_eq!(local.fact.unique, Some(u32::try_from(counted).unwrap()));
        assert_eq!(counted, 3);
        assert_eq!(local.verdict, "overdue-branch");
        assert_eq!(local.fact.pushed, Some(true));
        assert_eq!(
            find(&rows, "remote-branch", "origin/absorb/work").verdict,
            "overdue-branch"
        );
    }

    #[test]
    fn the_open_branch_alert_defaults_to_two_days_and_is_on() {
        let open = SprawlConfig::default().open_branch;
        assert!(open.enabled);
        assert_eq!(open.max_age_hours, 48);
    }

    #[test]
    fn an_own_branch_past_two_days_not_landed_alerts_and_off_it_lingers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "old", "o", "o\n", "me", NOW - 3 * DAY);
        on_branch(&repo.path, "young", "y", "y\n", "me", NOW - DAY);
        on_branch(&repo.path, "shared", "s", "s\n", "me", NOW - 3 * DAY);
        git_ok(&repo.path, &["push", "-q", "origin", "shared"]);
        git_ok(&repo.path, &["fetch", "-q", "origin"]);
        let facts = observe(
            &repo,
            "pleme-io",
            &Memo::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        let judge = |cfg: &SprawlConfig| {
            Policy::load(cfg, at(NOW))
                .unwrap()
                .judge(facts.clone())
                .unwrap()
        };
        let rows = judge(&config());
        let old = find(&rows, "branch", "old");
        assert_eq!(old.verdict, "overdue-branch");
        assert_eq!(old.severity, "alert");
        assert_eq!(old.age_hours, Some(72));
        assert_eq!(old.fact.pushed, Some(false));
        assert_eq!(find(&rows, "branch", "young").verdict, "active");
        assert_eq!(find(&rows, "branch", "shared").fact.pushed, Some(true));
        assert_eq!(
            find(&rows, "remote-branch", "origin/shared").verdict,
            "overdue-branch"
        );
        let off = SprawlConfig {
            open_branch: OpenBranchConfig {
                enabled: false,
                ..OpenBranchConfig::default()
            },
            ..config()
        };
        let quiet = judge(&off);
        assert_eq!(find(&quiet, "branch", "old").verdict, "lingering");
        assert_eq!(find(&quiet, "branch", "young").verdict, "active");
        let wide = SprawlConfig {
            open_branch: OpenBranchConfig {
                enabled: true,
                max_age_hours: 96,
            },
            ..config()
        };
        assert_eq!(find(&judge(&wide), "branch", "old").verdict, "active");
        let mut policy = Policy::load(&off, at(NOW)).unwrap();
        assert_eq!(policy.off, vec!["overdue-branch".to_owned()]);
        assert_eq!(policy.validate(&off).unwrap()["ok"], true);
        assert_eq!(policy.validate(&wide).unwrap()["ok"], true);
    }

    #[test]
    fn a_count_past_the_repo_budget_is_unknown_and_never_landed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "w", "a", "a\n", "me", NOW - 20 * DAY);
        let facts = observe(&repo, "pleme-io", &Memo::new(), Instant::now()).unwrap();
        let fact = facts.iter().find(|f| f.reference == "w").unwrap();
        assert_eq!(fact.unique, None);
        let rows = Policy::load(&config(), at(NOW))
            .unwrap()
            .judge(facts)
            .unwrap();
        assert_eq!(find(&rows, "branch", "w").verdict, "overdue-branch");
        assert_eq!(find(&rows, "branch", "w").landed, None);
    }

    #[test]
    fn a_memo_answers_for_an_unchanged_ref_with_no_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "w", "a", "a\n", "me", NOW - 20 * DAY);
        let rows = judged(&repo);
        let snapshot = Snapshot {
            taken_at_unix: NOW,
            scan_secs: 0.0,
            on_disk: 1,
            total: 1,
            read: 1,
            workers: 1,
            throttled: None,
            deep_unknown: 0,
            blind: Vec::new(),
            rows,
            classes: json!([]),
            refusals: Vec::new(),
            off: Vec::new(),
            digest: 0,
        };
        let memo = memo_of(Some(&snapshot));
        let facts = observe(&repo, "pleme-io", &memo, Instant::now()).unwrap();
        assert_eq!(
            facts.iter().find(|f| f.reference == "w").unwrap().unique,
            Some(1)
        );
        git_ok(&repo.path, &["checkout", "-q", "w"]);
        commit(&repo.path, "c", "c\n", "me", NOW - 19 * DAY);
        git_ok(&repo.path, &["checkout", "-q", "main"]);
        let moved = observe(&repo, "pleme-io", &memo, Instant::now()).unwrap();
        assert_eq!(
            moved.iter().find(|f| f.reference == "w").unwrap().unique,
            None
        );
    }

    #[test]
    fn another_authors_branch_is_foreign_and_its_command_never_deletes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "theirs", "t", "t\n", "someone", NOW - 20 * DAY);
        let rows = judged(&repo);
        let row = find(&rows, "branch", "theirs");
        assert_eq!(row.verdict, "foreign-author");
        assert!(row.report_only);
        assert!(!row.next.contains("-D"), "{}", row.next);
        assert!(row.next.starts_with("report only"), "{}", row.next);
    }

    #[test]
    fn an_akeylesslabs_branch_of_mine_is_only_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "ASM-1-x", "a", "a\n", "me", NOW - 20 * DAY);
        let facts = observe(
            &repo,
            "akeylesslabs",
            &Memo::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        let rows = Policy::load(&config(), at(NOW))
            .unwrap()
            .judge(facts)
            .unwrap();
        let row = find(&rows, "branch", "ASM-1-x");
        assert_eq!(row.verdict, "lingering");
        assert!(row.report_only);
    }

    #[test]
    fn stashes_split_into_bulk_sweeps_and_individual_ones() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        for message in [
            "retire-spec-autostash-1",
            "claude-fleet-clean-2",
            "my own idea",
        ] {
            std::fs::write(repo.path.join("base"), message).unwrap();
            git_ok(&repo.path, &["stash", "push", "-q", "-m", message]);
        }
        let rows = judged(&repo);
        let verdicts: Vec<(&str, &str)> = rows
            .iter()
            .filter(|r| r.fact.kind == "stash")
            .map(|r| (r.verdict.as_str(), r.fact.reference.as_str()))
            .collect();
        assert_eq!(verdicts.len(), 3, "{verdicts:?}");
        let count = |v: &str| verdicts.iter().filter(|(x, _)| *x == v).count();
        assert_eq!(count("bulk-sweep-stash"), 2);
        assert_eq!(count("individual-stash"), 1);
        let listed = git(&repo.path, &["stash", "list"]).unwrap().lines().count();
        assert_eq!(listed, verdicts.len(), "one row per stash git lists");
    }

    #[test]
    fn a_worktree_whose_path_is_gone_is_dead_and_a_dirty_one_says_so() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        let dead = tmp.path().join("dead-wt");
        let live = tmp.path().join("live-wt");
        git_ok(
            &repo.path,
            &["worktree", "add", "-q", "--detach", &dead.to_string_lossy()],
        );
        git_ok(
            &repo.path,
            &["worktree", "add", "-q", "--detach", &live.to_string_lossy()],
        );
        std::fs::remove_dir_all(&dead).unwrap();
        std::fs::write(live.join("scratch"), "x").unwrap();
        let rows = judged(&repo);
        let dead_row = rows
            .iter()
            .find(|r| r.fact.reference.ends_with("dead-wt"))
            .unwrap();
        assert_eq!(dead_row.verdict, "dead-worktree");
        assert_eq!(dead_row.fact.exists, Some(false));
        let live_row = rows
            .iter()
            .find(|r| r.fact.reference.ends_with("live-wt"))
            .unwrap();
        assert_eq!(live_row.verdict, "dirty-worktree");
        assert_eq!(rows.iter().filter(|r| r.fact.kind == "worktree").count(), 2);
    }

    #[test]
    fn a_backup_ref_and_a_gone_upstream_are_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "gone", "g", "g\n", "me", NOW - 20 * DAY);
        git_ok(&repo.path, &["push", "-q", "-u", "origin", "gone"]);
        git_ok(&repo.path, &["push", "-q", "origin", "--delete", "gone"]);
        git_ok(&repo.path, &["fetch", "-q", "--prune", "origin"]);
        git_ok(
            &repo.path,
            &["update-ref", "refs/backup/closed-2026-11-03/x", "gone"],
        );
        let rows = judged(&repo);
        assert_eq!(find(&rows, "branch", "gone").fact.upstream_gone, Some(true));
        assert_eq!(
            find(&rows, "backup-ref", "refs/backup/closed-2026-11-03/x").verdict,
            "backup-ref"
        );
    }

    #[test]
    fn a_rewritten_origin_has_no_merge_base_and_alerts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        git_ok(&other, &["init", "-q", "-b", "main"]);
        git_ok(&other, &["config", "user.email", "x@example.invalid"]);
        git_ok(&other, &["config", "user.name", "x"]);
        git_ok(&other, &["config", "commit.gpgsign", "false"]);
        commit(&other, "z", "z\n", "x", NOW - DAY);
        let upstream = tmp.path().join(".upstreams/r.git");
        git_ok(
            &other,
            &["push", "-q", "--force", &upstream.to_string_lossy(), "main"],
        );
        git_ok(&repo.path, &["fetch", "-q", "origin"]);
        let rows = judged(&repo);
        let row = find(&rows, "default-branch", "main");
        assert_eq!(row.verdict, "default-branch-diverged");
        assert_eq!(row.severity, "alert");
        assert_eq!(row.rank, 0);
        assert_eq!(rows[0].verdict, "default-branch-diverged", "worst first");
    }

    #[test]
    fn a_control_repo_ahead_of_origin_with_a_shared_history_is_not_diverged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        commit(&repo.path, "ahead", "x\n", "me", NOW - DAY);
        assert!(judged(&repo)
            .iter()
            .all(|r| r.fact.kind != "default-branch"));
    }

    #[test]
    fn a_broken_repo_is_blind_and_the_rest_are_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _good = fixture(tmp.path(), "good");
        let broken = tmp.path().join("broken");
        std::fs::create_dir_all(broken.join(".git")).unwrap();
        let ws = workspace(tmp.path());
        let scanned = scan(&[&ws], &config(), 2, &Memo::new(), None);
        assert_eq!(scanned.total, 2);
        assert_eq!(scanned.read, 1);
        assert_eq!(scanned.blind.len(), 1);
        assert_eq!(scanned.blind[0].repo, "broken");
        assert_eq!(scanned.blind[0].org, "pleme-io");
    }

    #[test]
    fn a_scan_past_its_deadline_is_blind_not_zero() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _a = fixture(tmp.path(), "a");
        let ws = workspace(tmp.path());
        let scanned = scan(&[&ws], &config(), 1, &Memo::new(), Some(Instant::now()));
        assert_eq!(scanned.read, 0);
        assert_eq!(scanned.blind.len(), 1);
        assert!(scanned.blind[0].why.contains("scan budget"));
    }

    fn snapshot_of(tmp: &Path, cfg: &SprawlConfig) -> Snapshot {
        let ws = workspace(tmp);
        refresh(
            &[&ws],
            cfg,
            2,
            None,
            &tmp.join("cache/sprawl.json"),
            at(NOW),
        )
        .unwrap()
    }

    fn no_probe() -> crate::heartbeat::Liveness {
        panic!("a fresh snapshot does not ask whether the daemon runs")
    }

    #[test]
    fn the_report_filters_and_carries_age_outcome_and_no_email() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "w", "a", "a\n", "me", NOW - 20 * DAY);
        on_branch(&repo.path, "x", "b", "b\n", "someone", NOW - 20 * DAY);
        let cfg = config();
        let snap = snapshot_of(tmp.path(), &cfg);
        let all = report(
            &cfg,
            Some(snap.clone()),
            &Filters::default(),
            100,
            at(NOW + 60),
            no_probe,
        )
        .unwrap();
        assert_eq!(all["outcome"], "found");
        assert_eq!(all["age_secs"], 60);
        assert_eq!(all["matched"], 2);
        assert!(!all.to_string().contains("example.invalid"), "{all}");
        let narrow = Filters {
            verdict: Some("overdue-branch".into()),
            ..Filters::default()
        };
        let one = report(&cfg, Some(snap.clone()), &narrow, 100, at(NOW), no_probe).unwrap();
        assert_eq!(one["matched"], 1);
        assert_eq!(one["items"][0]["ref"], "w");
        assert_eq!(one["classes"].as_array().unwrap().len(), 1);
        let limited = Filters {
            limit: Some(1),
            ..Filters::default()
        };
        let cut = report(&cfg, Some(snap.clone()), &limited, 100, at(NOW), no_probe).unwrap();
        assert_eq!(cut["returned"], 1);
        assert_eq!(cut["matched"], 2);
        let nothing = Filters {
            repo: Some("no-such".into()),
            ..Filters::default()
        };
        let empty = report(&cfg, Some(snap), &nothing, 100, at(NOW), no_probe).unwrap();
        assert_eq!(empty["outcome"], "empty");
    }

    #[test]
    fn no_snapshot_and_an_unread_repo_in_scope_are_blind_never_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = config();
        let none = report(&cfg, None, &Filters::default(), 100, at(NOW), no_probe).unwrap();
        assert_eq!(none["outcome"], "blind");
        std::fs::create_dir_all(tmp.path().join("broken/.git")).unwrap();
        let _good = fixture(tmp.path(), "good");
        let snap = snapshot_of(tmp.path(), &cfg);
        let there = Filters {
            repo: Some("broken".into()),
            ..Filters::default()
        };
        let blind = report(&cfg, Some(snap.clone()), &there, 100, at(NOW), no_probe).unwrap();
        assert_eq!(blind["outcome"], "blind");
        let control = Filters {
            repo: Some("good".into()),
            ..Filters::default()
        };
        let clean = report(&cfg, Some(snap), &control, 100, at(NOW), no_probe).unwrap();
        assert_eq!(clean["outcome"], "empty");
    }

    #[test]
    fn a_stale_snapshot_asks_the_daemon_and_says_it_is_throttled_not_dead() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _r = fixture(tmp.path(), "r");
        let cfg = config();
        let snap = snapshot_of(tmp.path(), &cfg);
        let later = at(NOW + 3 * 3600);
        let out = report(
            &cfg,
            Some(snap.clone()),
            &Filters::default(),
            100,
            later,
            || crate::heartbeat::Liveness::Running {
                pid: 7,
                tick_age_secs: 600,
                state: "throttle".into(),
                why: "load".into(),
            },
        )
        .unwrap();
        assert_eq!(out["stale"], true);
        let why = out["stale_why"].as_str().unwrap();
        assert!(why.contains("The daemon runs"), "{why}");
        assert!(why.contains("throttle"), "{why}");
        assert!(!why.contains("not running"), "{why}");
        let fresh = report(
            &cfg,
            Some(snap),
            &Filters::default(),
            100,
            at(NOW + 60),
            no_probe,
        )
        .unwrap();
        assert_eq!(fresh["stale"], false);
    }

    #[test]
    fn a_rule_change_rejudges_the_saved_facts_without_a_scan() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "w", "a", "a\n", "me", NOW - 20 * DAY);
        let cfg = config();
        let snap = snapshot_of(tmp.path(), &cfg);
        let tuned = SprawlConfig {
            rules: vec![
                json!({"entry": "off", "name": "overdue-branch", "value": {"target": "rule"}}),
                json!({"entry": "off", "name": "lingering", "value": {"target": "rule"}}),
            ],
            ..config()
        };
        let out = report(
            &tuned,
            Some(snap),
            &Filters::default(),
            100,
            at(NOW),
            no_probe,
        )
        .unwrap();
        assert_eq!(out["items"][0]["verdict"], "unclassified");
        assert_eq!(out["off"], json!(["overdue-branch", "lingering"]));
    }

    #[test]
    fn a_tend_b_entry_written_with_kousei_reaches_the_judgement() {
        let text = "use(\"kousei\", %i[off tune])\n\ndef config()\n  {\n    sprawl: {\n      rules: [\n        off(:rule, \"lingering\"),\n        tune(:rule, \"active\", {scope: {max_age_hours: 1}})\n      ]\n    }\n  }\nend\n";
        let evaluated = crate::blue_config::evaluate_text(None, text).unwrap();
        let rules = evaluated.json["sprawl"]["rules"]
            .as_array()
            .unwrap()
            .clone();
        let cfg = SprawlConfig { rules, ..config() };
        let policy = Policy::load(&cfg, at(NOW)).unwrap();
        assert_eq!(policy.refusals, Vec::<String>::new());
        assert_eq!(policy.off, vec!["lingering".to_owned()]);
    }

    #[test]
    fn validate_passes_the_built_ins_and_fails_a_rule_whose_example_is_wrong() {
        let mut policy = Policy::load(&config(), at(NOW)).unwrap();
        let good = policy.validate(&config()).unwrap();
        assert_eq!(good["ok"], true, "{good}");
        let wrong = SprawlConfig {
            rules: vec![json!({
                "entry": "rule",
                "name": "wrong",
                "value": {
                    "verdict": "wrong",
                    "kinds": ["stash"],
                    "when": [["message", "contains_any", ["zzz"]]],
                    "severity": "info",
                    "examples": [{"item": {"kind": "stash", "message": "aaa"}, "expect": "wrong"}]
                }
            })],
            ..config()
        };
        let bad = policy.validate(&wrong).unwrap();
        assert_eq!(bad["ok"], false);
        assert_eq!(bad["failures"][0]["rule"], "wrong");
    }

    #[test]
    fn a_two_string_clause_would_turn_into_a_map_so_the_unknown_op_has_three() {
        let cfg = SprawlConfig {
            rules: vec![json!({
                "entry": "rule",
                "name": "uncounted",
                "value": {
                    "verdict": "uncounted",
                    "kinds": ["branch"],
                    "when": [["ahead", "unknown", true]],
                    "severity": "info",
                    "examples": [{"item": {"kind": "branch"}, "expect": "uncounted"}]
                }
            })],
            ..config()
        };
        let mut policy = Policy::load(&cfg, at(NOW)).unwrap();
        assert_eq!(policy.refusals, Vec::<String>::new());
        let fact = Fact {
            kind: "branch".into(),
            author: Some("me".into()),
            committed_unix: Some(NOW - 20 * DAY),
            ..Fact::default()
        };
        let counted = Fact {
            ahead: Some(1),
            ..fact.clone()
        };
        let rows = policy.judge(vec![fact, counted]).unwrap();
        let of = |ahead: Option<u32>| {
            rows.iter()
                .find(|r| r.fact.ahead == ahead)
                .map(|r| r.verdict.as_str())
        };
        assert_eq!(of(None), Some("uncounted"));
        assert_eq!(of(Some(1)), Some("overdue-branch"));
    }

    #[test]
    fn the_session_lines_are_one_per_class_then_one_per_alerting_branch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        for i in 0..3 {
            on_branch(
                &repo.path,
                &format!("w{i}"),
                &format!("a{i}"),
                "a\n",
                "me",
                NOW - 20 * DAY,
            );
        }
        std::fs::write(repo.path.join("base"), "x").unwrap();
        git_ok(
            &repo.path,
            &["stash", "push", "-q", "-m", "claude-fleet-clean-1"],
        );
        let cfg = config();
        let snap = snapshot_of(tmp.path(), &cfg);
        let total: u64 = snap
            .classes
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["count"].as_u64().unwrap())
            .sum();
        assert_eq!(total, snap.rows.len() as u64);
        let lines = session_lines(&cfg, &tmp.path().join("cache/sprawl.json"), at(NOW));
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(
            lines[0].contains("3 overdue-branch in 1 repo "),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("1 bulk-sweep-stash")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("overdue-branch pleme-io/r w0, 20d old, local only")),
            "{lines:?}"
        );
        let text = status_text(&cfg, &tmp.path().join("cache/sprawl.json"), at(NOW));
        assert!(text.contains("3 alert, 0 warn, 1 info"), "{text}");
        assert!(text.contains("1 of 1 repos read"), "{text}");
        let quiet = SprawlConfig {
            open_branch: OpenBranchConfig {
                enabled: false,
                ..OpenBranchConfig::default()
            },
            ..config()
        };
        snapshot_of(tmp.path(), &quiet);
        let calm = session_lines(&quiet, &tmp.path().join("cache/sprawl.json"), at(NOW));
        assert_eq!(calm.len(), 2, "{calm:?}");
        assert!(calm[0].contains("3 lingering in 1 repo "), "{calm:?}");
    }

    #[test]
    fn no_snapshot_at_all_says_blind_in_the_session_and_status_lines() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = config();
        let lines = session_lines(&cfg, &tmp.path().join("none.json"), at(NOW));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("blind"), "{lines:?}");
        assert!(status_text(&cfg, &tmp.path().join("none.json"), at(NOW)).contains("blind"));
    }

    #[test]
    fn the_scan_changes_no_ref_in_the_repo_it_reads() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "w", "a", "a\n", "me", NOW - 20 * DAY);
        std::fs::write(repo.path.join("base"), "x").unwrap();
        git_ok(&repo.path, &["stash", "push", "-q", "-m", "mine"]);
        let before = git(&repo.path, &["for-each-ref"]).unwrap();
        let _ = judged(&repo);
        assert_eq!(git(&repo.path, &["for-each-ref"]).unwrap(), before);
    }

    #[test]
    fn a_touched_repo_gets_one_line_with_what_it_holds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = fixture(tmp.path(), "r");
        on_branch(&repo.path, "w", "a", "a\n", "me", NOW - 20 * DAY);
        let ws = workspace(tmp.path());
        let note = touched_note(
            &[&ws],
            std::slice::from_ref(&repo),
            &config(),
            &tmp.path().join("none.json"),
            Duration::from_secs(30),
            at(NOW),
        )
        .unwrap();
        assert_eq!(note.unread, 0);
        assert_eq!(note.lines.len(), 1);
        assert!(
            note.lines[0].contains("r holds 1 overdue-branch"),
            "{:?}",
            note.lines
        );
    }
}
