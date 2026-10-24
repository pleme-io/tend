use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

const POLICY_SOURCE: &str = include_str!("../blue/drift.b");

pub(crate) const DEFAULT_INTERVAL_MINUTES: u64 = 30;
pub(crate) const DEFAULT_RATE_LIMIT_FLOOR: u64 = 200;
pub(crate) const DEFAULT_LABEL: &str = "tend-drift";
const RUNS_PER_GATE: u32 = 30;
const DETAIL_MAX_CHARS: usize = 240;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DriftConfig {
    pub enable: bool,
    pub interval_minutes: u64,
    pub rate_limit_floor: u64,
    pub issues: DriftIssuesConfig,
    pub checks: BTreeMap<String, DriftCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DriftIssuesConfig {
    pub enable: bool,
    pub label: String,
}

impl Default for DriftIssuesConfig {
    fn default() -> Self {
        Self {
            enable: false,
            label: DEFAULT_LABEL.to_owned(),
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    #[default]
    Command,
    Gate,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ReadAs {
    #[default]
    ExitStatus,
    RepoForgeNdjson,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DriftCheck {
    pub enable: bool,
    pub kind: String,
    pub template: String,
    pub source: Source,
    pub command: Vec<String>,
    pub dirs: Vec<String>,
    pub read: ReadAs,
    pub repos: Vec<String>,
    pub workflow: String,
    pub branch: String,
    pub cadence_minutes: u64,
    pub red_window_hours: u64,
    pub timeout_minutes: u64,
    pub drift_on: Vec<String>,
}

impl Default for DriftCheck {
    fn default() -> Self {
        Self {
            enable: true,
            kind: String::new(),
            template: String::new(),
            source: Source::Command,
            command: Vec::new(),
            dirs: Vec::new(),
            read: ReadAs::ExitStatus,
            repos: Vec::new(),
            workflow: String::new(),
            branch: "main".to_owned(),
            cadence_minutes: 360,
            red_window_hours: 24,
            timeout_minutes: 30,
            drift_on: Vec::new(),
        }
    }
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_owned()).collect()
}

fn command_check(
    kind: &str,
    template: &str,
    argv: &[&str],
    dirs: &[&str],
    cadence: u64,
) -> DriftCheck {
    DriftCheck {
        kind: kind.to_owned(),
        template: template.to_owned(),
        source: Source::Command,
        command: strings(argv),
        dirs: strings(dirs),
        cadence_minutes: cadence,
        ..DriftCheck::default()
    }
}

fn gate_check(kind: &str, template: &str, repos: &[&str], workflow: &str) -> DriftCheck {
    DriftCheck {
        kind: kind.to_owned(),
        template: template.to_owned(),
        source: Source::Gate,
        repos: strings(repos),
        workflow: workflow.to_owned(),
        cadence_minutes: 60,
        ..DriftCheck::default()
    }
}

#[must_use]
pub fn default_checks() -> BTreeMap<String, DriftCheck> {
    let root = "~/code/github/pleme-io";
    let at = |repo: &str| format!("{root}/{repo}");
    let mut checks = BTreeMap::new();
    checks.insert(
        "repo".to_owned(),
        DriftCheck {
            read: ReadAs::RepoForgeNdjson,
            ..command_check(
                "repo",
                "pleme-io/repo-forge",
                &["repo-forge", "check", "--catalog", "repos.lisp", "--json"],
                &[&at("repo-forge")],
                360,
            )
        },
    );
    checks.insert(
        "reusable-workflow".to_owned(),
        gate_check(
            "reusable-workflow",
            "pleme-io/flows",
            &["pleme-io/flows"],
            "substrate-parity.yml",
        ),
    );
    checks.insert(
        "action".to_owned(),
        gate_check(
            "action",
            "pleme-io/actions",
            &["pleme-io/actions"],
            "ci.yml",
        ),
    );
    checks.insert(
        "bidama".to_owned(),
        gate_check("bidama", "pleme-io/blue", &["pleme-io/blue"], "ci.yml"),
    );
    checks.insert(
        "service".to_owned(),
        command_check(
            "service",
            "pleme-io/ola-mundo",
            &["nix", "build", ".#checks.{system}.project", "--no-link"],
            &[&at("kensa"), &at("ola-mundo")],
            720,
        ),
    );
    checks.insert(
        "chart".to_owned(),
        command_check(
            "chart",
            "pleme-io/helmworks",
            &[
                "nix",
                "build",
                ".#checks.{system}.values-schema-fresh",
                "--no-link",
            ],
            &[&at("helmworks")],
            720,
        ),
    );
    checks.insert(
        "skill".to_owned(),
        command_check(
            "skill",
            "pleme-io/blackmatter-pleme",
            &["nix", "run", ".#lint"],
            &[&at("blackmatter-pleme")],
            360,
        ),
    );
    checks
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            enable: true,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            rate_limit_floor: DEFAULT_RATE_LIMIT_FLOOR,
            issues: DriftIssuesConfig::default(),
            checks: default_checks(),
        }
    }
}

impl DriftConfig {
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_minutes.max(1).saturating_mul(60))
    }
}

impl DriftCheck {
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        if self.kind.trim().is_empty() {
            return Some("no kind".to_owned());
        }
        if !is_repo_slug(&self.template) {
            return Some(format!("template `{}` is not owner/repo", self.template));
        }
        match self.source {
            Source::Command if self.command.is_empty() => {
                Some("source command with no command".to_owned())
            }
            Source::Command if self.dirs.is_empty() => {
                Some("source command with no dirs".to_owned())
            }
            Source::Gate if self.repos.is_empty() => Some("source gate with no repos".to_owned()),
            Source::Gate if self.workflow.trim().is_empty() => {
                Some("source gate with no workflow".to_owned())
            }
            Source::Gate => self
                .repos
                .iter()
                .find(|r| !is_repo_slug(r))
                .map(|r| format!("gate repo `{r}` is not owner/repo")),
            Source::Command => None,
        }
    }

    fn cadence(&self) -> Duration {
        Duration::from_secs(self.cadence_minutes.max(1).saturating_mul(60))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_minutes.max(1).saturating_mul(60))
    }
}

fn is_repo_slug(s: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    matches!(s.split_once('/'), Some((o, r)) if ok(o) && ok(r))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub conclusion: String,
    pub created_unix: u64,
    pub created_at: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub name: String,
    pub sight: String,
    pub why: String,
    pub drifted: bool,
    pub detail: String,
    pub runs: Vec<Run>,
}

impl Instance {
    fn seen(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            sight: "seen".to_owned(),
            why: String::new(),
            drifted: false,
            detail: String::new(),
            runs: Vec::new(),
        }
    }

    fn blind(name: &str, why: impl Into<String>) -> Self {
        Self {
            sight: "blind".to_owned(),
            why: why.into(),
            ..Self::seen(name)
        }
    }

    fn to_policy(&self) -> Json {
        json!({
            "name": self.name,
            "sight": self.sight,
            "why": self.why,
            "drifted": self.drifted,
            "detail": self.detail,
            "runs": self.runs.iter().map(|r| json!([
                r.conclusion, i64::try_from(r.created_unix).unwrap_or(i64::MAX), r.created_at, r.url
            ])).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub observed_unix: u64,
    pub sight: String,
    pub why: String,
    pub complete: bool,
    pub instances: Vec<Instance>,
}

impl Observation {
    fn seen(now: u64, instances: Vec<Instance>) -> Self {
        Self {
            observed_unix: now,
            sight: "seen".to_owned(),
            why: String::new(),
            complete: true,
            instances,
        }
    }

    fn blind(now: u64, why: impl Into<String>) -> Self {
        Self {
            observed_unix: now,
            sight: "blind".to_owned(),
            why: why.into(),
            complete: false,
            instances: Vec::new(),
        }
    }
}

pub enum Ran {
    Exited {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    Failed(String),
}

pub trait Runner: Send + Sync {
    fn run(&self, argv: &[String], cwd: &Path, timeout: Duration) -> Ran;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, argv: &[String], cwd: &Path, timeout: Duration) -> Ran {
        let Some((program, args)) = argv.split_first() else {
            return Ran::Failed("empty command".to_owned());
        };
        let spawned = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ran::Failed(format!("{program}: not found on PATH"))
            }
            Err(e) => return Ran::Failed(format!("{program}: {e}")),
        };
        let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                if let Some(mut p) = pipe {
                    let mut bytes = Vec::new();
                    let _ = p.read_to_end(&mut bytes);
                    text = String::from_utf8_lossy(&bytes).into_owned();
                }
                text
            })
        };
        let out = drain(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        );
        let err = drain(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        );
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Ok(s),
                Ok(None) if started.elapsed() >= timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "timed out after {} minutes",
                        timeout.as_secs() / 60
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(e) => break Err(e.to_string()),
            }
        };
        let stdout = out.join().unwrap_or_default();
        let stderr = err.join().unwrap_or_default();
        match status {
            Ok(s) => Ran::Exited {
                code: s.code(),
                stdout,
                stderr,
            },
            Err(why) => Ran::Failed(why),
        }
    }
}

pub enum RunsRead {
    Runs(Vec<Run>),
    Blind(String),
}

#[async_trait]
pub trait Actions: Send + Sync {
    async fn runs(&self, repo: &str, workflow: &str, branch: &str) -> RunsRead;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIssue {
    pub number: u64,
    pub body: String,
}

#[async_trait]
pub trait Issues: Send + Sync {
    async fn find(&self, repo: &str, label: &str, title: &str) -> Result<Option<OpenIssue>>;
    async fn create(&self, repo: &str, label: &str, title: &str, body: &str) -> Result<u64>;
    async fn update(&self, repo: &str, number: u64, body: &str) -> Result<()>;
    async fn close(&self, repo: &str, number: u64, body: &str) -> Result<()>;
}

#[must_use]
pub fn nix_system() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{}-{os}", std::env::consts::ARCH)
}

fn expand(dir: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(dir.trim()).into_owned())
}

fn instance_name(dir: &Path) -> String {
    dir.file_name().map_or_else(
        || dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn clip(s: &str) -> String {
    let flat = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "/");
    if flat.chars().count() <= DETAIL_MAX_CHARS {
        flat
    } else {
        flat.chars().take(DETAIL_MAX_CHARS).collect::<String>() + "…"
    }
}

#[must_use]
pub fn gist(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .take_while(|l| !l.starts_with("Stack backtrace"))
        .filter(|l| !l.is_empty() && *l != "Caused by:")
        .collect();
    let start = lines
        .iter()
        .position(|l| l.to_ascii_lowercase().starts_with("error"));
    let picked: Vec<&str> = match start {
        Some(i) => lines.iter().skip(i).take(2).copied().collect(),
        None => lines.last().copied().into_iter().collect(),
    };
    clip(&picked.join(": "))
}

#[must_use]
pub fn read_repo_forge(stdout: &str) -> (Vec<Instance>, bool) {
    let mut instances = Vec::new();
    let mut complete = false;
    for line in stdout.lines().map(str::trim).filter(|l| l.starts_with('{')) {
        let Ok(v) = serde_json::from_str::<Json>(line) else {
            continue;
        };
        match v.get("repo").and_then(Json::as_str) {
            None => complete = true,
            Some(repo) if v.get("drifts").is_some() => {
                let drifts = v["drifts"].as_array().map_or(0, Vec::len);
                let mut i = Instance::seen(repo);
                if drifts > 0 {
                    let s = &v["summary"];
                    i.drifted = true;
                    i.detail = format!(
                        "{drifts} file(s) off the archetype: {} missing, {} modified",
                        s["missing"].as_u64().unwrap_or(0),
                        s["modified"].as_u64().unwrap_or(0)
                    );
                }
                instances.push(i);
            }
            Some(repo) => instances.push(Instance::blind(repo, "no local clone to compare")),
        }
    }
    (instances, complete)
}

fn observe_command(check: &DriftCheck, runner: &dyn Runner, now: u64) -> Observation {
    let system = nix_system();
    let argv: Vec<String> = check
        .command
        .iter()
        .map(|a| a.replace("{system}", &system))
        .collect();
    match check.read {
        ReadAs::RepoForgeNdjson => {
            let dir = expand(&check.dirs[0]);
            if !dir.is_dir() {
                return Observation::blind(now, format!("{}: no such directory", dir.display()));
            }
            match runner.run(&argv, &dir, check.timeout()) {
                Ran::Failed(why) => Observation::blind(now, why),
                Ran::Exited { stdout, stderr, .. } => {
                    let (instances, complete) = read_repo_forge(&stdout);
                    let mut o = Observation::seen(now, instances);
                    if !complete {
                        o.complete = false;
                        o.why = format!("stopped before the fleet rollup: {}", gist(&stderr));
                    }
                    o
                }
            }
        }
        ReadAs::ExitStatus => {
            let instances = check
                .dirs
                .iter()
                .map(|d| {
                    let dir = expand(d);
                    let name = instance_name(&dir);
                    if !dir.is_dir() {
                        return Instance::blind(
                            &name,
                            format!("{}: no such directory", dir.display()),
                        );
                    }
                    match runner.run(&argv, &dir, check.timeout()) {
                        Ran::Failed(why) => Instance::blind(&name, why),
                        Ran::Exited { code: Some(0), .. } => Instance::seen(&name),
                        Ran::Exited { code: None, .. } => {
                            Instance::blind(&name, "killed by a signal")
                        }
                        Ran::Exited {
                            code: Some(c),
                            stdout,
                            stderr,
                        } => {
                            let seen = format!("{stdout}\n{stderr}")
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ");
                            if check.drift_on.is_empty()
                                || check.drift_on.iter().any(|m| seen.contains(m.as_str()))
                            {
                                let mut i = Instance::seen(&name);
                                i.drifted = true;
                                i.detail = format!("exit {c}: {}", gist(&stderr));
                                i
                            } else {
                                Instance::blind(
                                    &name,
                                    format!(
                                        "exit {c} without a drift marker, so the check did not run: {}",
                                        gist(&stderr)
                                    ),
                                )
                            }
                        }
                    }
                })
                .collect();
            Observation::seen(now, instances)
        }
    }
}

async fn observe_gate(check: &DriftCheck, actions: &dyn Actions, now: u64) -> Observation {
    let mut instances = Vec::with_capacity(check.repos.len());
    for repo in &check.repos {
        instances.push(
            match actions.runs(repo, &check.workflow, &check.branch).await {
                RunsRead::Runs(runs) => Instance {
                    runs,
                    ..Instance::seen(repo)
                },
                RunsRead::Blind(why) => Instance::blind(repo, why),
            },
        );
    }
    Observation::seen(now, instances)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub judged_unix: u64,
    pub observations: BTreeMap<String, Observation>,
    pub report: Json,
    pub issues: Vec<Json>,
}

pub fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn policy_check(name: &str, check: &DriftCheck, obs: Option<&Observation>) -> Json {
    let window = i64::try_from(check.red_window_hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    let refused = check.refusal();
    let (sight, why, complete, instances) = match (&refused, obs) {
        (Some(r), _) => (
            "blind".to_owned(),
            format!("refused: {r}"),
            false,
            Vec::new(),
        ),
        (None, None) => (
            "blind".to_owned(),
            "not observed yet".to_owned(),
            false,
            Vec::new(),
        ),
        (None, Some(o)) => (
            o.sight.clone(),
            o.why.clone(),
            o.complete,
            o.instances.clone(),
        ),
    };
    json!({
        "name": name,
        "kind": check.kind,
        "template": check.template,
        "enabled": check.enable,
        "window": window,
        "source": match check.source { Source::Command => "command", Source::Gate => "gate" },
        "sight": sight,
        "why": why,
        "complete": complete,
        "instances": instances.iter().map(Instance::to_policy).collect::<Vec<_>>(),
    })
}

pub fn judge(
    cfg: &DriftConfig,
    observations: &BTreeMap<String, Observation>,
    now: u64,
) -> Result<Json> {
    let checks: Vec<Json> = cfg
        .checks
        .iter()
        .map(|(name, c)| policy_check(name, c, observations.get(name)))
        .collect();
    let out = crate::blue_policy::call(
        POLICY_SOURCE,
        "drift.b",
        "judge",
        &[
            Json::Array(checks),
            json!(i64::try_from(now).unwrap_or(i64::MAX)),
        ],
    )?;
    if out.get("verdict").and_then(Json::as_str).is_none() {
        return Err(anyhow!("drift.b: judge() answered {out}"));
    }
    Ok(out)
}

pub struct Probes<'a> {
    pub runner: &'a dyn Runner,
    pub actions: &'a dyn Actions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due<'a> {
    Cadence,
    All,
    Only(&'a str),
}

pub async fn sweep(
    cfg: &DriftConfig,
    prior: &State,
    due: Due<'_>,
    now: SystemTime,
    probes: &Probes<'_>,
) -> Result<State> {
    let now_s = unix(now);
    let mut observations = BTreeMap::new();
    for (name, check) in &cfg.checks {
        let last = prior.observations.get(name);
        let fresh = match due {
            Due::All => true,
            Due::Only(n) => n == name,
            Due::Cadence => last
                .is_none_or(|o| now_s.saturating_sub(o.observed_unix) >= check.cadence().as_secs()),
        };
        if !check.enable || check.refusal().is_some() || !fresh {
            if let Some(o) = last {
                observations.insert(name.clone(), o.clone());
            }
            continue;
        }
        let obs = match check.source {
            Source::Gate => observe_gate(check, probes.actions, now_s).await,
            Source::Command => {
                let c = check.clone();
                let runner = probes.runner;
                std::thread::scope(|s| {
                    s.spawn(|| observe_command(&c, runner, now_s))
                        .join()
                        .unwrap_or_else(|_| Observation::blind(now_s, "the check panicked"))
                })
            }
        };
        observations.insert(name.clone(), obs);
    }
    let report = judge(cfg, &observations, now_s)?;
    Ok(State {
        judged_unix: now_s,
        observations,
        report,
        issues: Vec::new(),
    })
}

pub async fn apply_issues(plan: &Json, label: &str, issues: &dyn Issues) -> Vec<Json> {
    let mut done = Vec::new();
    for item in plan.as_array().into_iter().flatten() {
        let field = |k: &str| {
            item.get(k)
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let (action, repo, title, body) = (
            field("action"),
            field("template"),
            field("title"),
            field("body"),
        );
        if action != "open" && action != "close" {
            continue;
        }
        let outcome = async {
            let found = issues.find(&repo, label, &title).await?;
            Ok::<_, anyhow::Error>(match (action.as_str(), found) {
                ("open", None) => format!(
                    "created #{}",
                    issues.create(&repo, label, &title, &body).await?
                ),
                ("open", Some(i)) if i.body.trim() == body.trim() => {
                    format!("unchanged #{}", i.number)
                }
                ("open", Some(i)) => {
                    issues.update(&repo, i.number, &body).await?;
                    format!("updated #{}", i.number)
                }
                ("close", Some(i)) => {
                    issues.close(&repo, i.number, &body).await?;
                    format!("closed #{}", i.number)
                }
                _ => "none open".to_owned(),
            })
        }
        .await;
        done.push(match outcome {
            Ok(what) => json!({"template": repo, "title": title, "action": action, "result": what}),
            Err(e) => json!({"template": repo, "title": title, "action": action, "error": format!("{e:#}")}),
        });
    }
    done
}

pub(crate) fn state_path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("drift")
        .join("state.json")
}

#[must_use]
pub fn read_state(path: &Path) -> Option<State> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn write_state(path: &Path, state: &State) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

fn rows<'a>(report: &'a Json, key: &'a str) -> impl Iterator<Item = (&'a str, &'a Json)> + 'a {
    report["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(move |c| {
            let name = c["name"].as_str().unwrap_or_default();
            c[key]
                .as_array()
                .into_iter()
                .flatten()
                .map(move |a| (name, a))
        })
}

fn row_text(name: &str, a: &Json) -> String {
    let since = a["since"].as_str().filter(|s| !s.is_empty());
    let detail = a["detail"].as_str().filter(|s| !s.is_empty());
    let mut s = format!(
        "{name}: {} {}",
        a["instance"].as_str().unwrap_or_default(),
        a["state"].as_str().unwrap_or_default()
    );
    if let Some(t) = since {
        s.push_str(&format!(" since {t}"));
    }
    if let Some(d) = detail {
        s.push_str(&format!(" ({d})"));
    }
    s
}

#[must_use]
pub fn finding_lines(report: &Json) -> Vec<String> {
    let mut out = Vec::new();
    for (glyph, key) in [("❌", "alerts"), ("⚠️", "blind"), ("➖", "watch")] {
        out.extend(rows(report, key).map(|(n, a)| format!("{glyph} {}", row_text(n, a))));
    }
    out
}

#[must_use]
pub fn session_note(state: Option<&State>, now: SystemTime) -> Option<String> {
    let Some(state) = state else {
        return Some("tend drift: no sweep has run on this host, so template drift is unseen (blind). Run `tend drift run`.".to_owned());
    };
    let verdict = state.report["verdict"].as_str().unwrap_or("blind");
    if verdict == "green" {
        return None;
    }
    let age = unix(now).saturating_sub(state.judged_unix) / 60;
    let lines = finding_lines(&state.report);
    let shown: Vec<&String> = lines.iter().take(8).collect();
    let mut s = format!(
        "tend drift ({verdict}, swept {age} min ago): {} alert(s), {} blind check(s).",
        state.report["alerts"].as_u64().unwrap_or(0),
        state.report["blind"].as_u64().unwrap_or(0)
    );
    for l in shown {
        s.push('\n');
        s.push_str(l);
    }
    if lines.len() > 8 {
        s.push_str(&format!(
            "\n… and {} more: `tend drift report`.",
            lines.len() - 8
        ));
    }
    Some(s)
}

#[must_use]
pub fn status_line(state: Option<&State>) -> String {
    match state {
        None => "drift: ⚠️ blind (no sweep has run; `tend drift run`)".to_owned(),
        Some(s) => {
            let v = s.report["verdict"].as_str().unwrap_or("blind");
            let glyph = match v {
                "green" => "✅",
                "alert" => "❌",
                "watch" => "➖",
                _ => "⚠️",
            };
            format!(
                "drift: {glyph} {v} ({} alert(s), {} blind; `tend drift report`)",
                s.report["alerts"].as_u64().unwrap_or(0),
                s.report["blind"].as_u64().unwrap_or(0)
            )
        }
    }
}

pub fn print_report(state: &State) {
    println!("{}", status_line(Some(state)));
    for c in state.report["checks"].as_array().into_iter().flatten() {
        let v = c["verdict"].as_str().unwrap_or_default();
        let glyph = match v {
            "green" => "✅",
            "off" => "➖",
            "drifted" | "red-past-window" => "❌",
            _ => "⚠️",
        };
        println!(
            "{glyph} {} [{} -> {}]: {v} ({} of {} instance(s) green)",
            c["name"].as_str().unwrap_or_default(),
            c["kind"].as_str().unwrap_or_default(),
            c["template"].as_str().unwrap_or_default(),
            c["green"].as_u64().unwrap_or(0),
            c["instances"].as_u64().unwrap_or(0)
        );
    }
    for l in finding_lines(&state.report) {
        println!("  {l}");
    }
    for i in &state.issues {
        println!("  issue: {i}");
    }
}

pub struct HttpActions {
    floor: u64,
    remaining: std::sync::Mutex<Option<u64>>,
}

impl HttpActions {
    #[must_use]
    pub fn new(floor: u64) -> Self {
        Self {
            floor,
            remaining: std::sync::Mutex::new(None),
        }
    }
}

#[derive(Deserialize)]
struct RunsPage {
    workflow_runs: Vec<RunRow>,
}

#[derive(Deserialize)]
struct RunRow {
    conclusion: Option<String>,
    created_at: String,
    html_url: String,
}

#[async_trait]
impl Actions for HttpActions {
    async fn runs(&self, repo: &str, workflow: &str, branch: &str) -> RunsRead {
        let left = *self
            .remaining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = left.filter(|n| *n < self.floor) {
            return RunsRead::Blind(format!(
                "GitHub rate limit: {n} requests left, under the floor {}",
                self.floor
            ));
        }
        let client = match crate::gh_auth::credentials().rest_client(crate::provider::GITHUB_API) {
            Ok(c) => c,
            Err(e) => return RunsRead::Blind(format!("GitHub client: {e}")),
        };
        let path = format!(
            "/repos/{repo}/actions/workflows/{workflow}/runs?branch={branch}&status=completed&per_page={RUNS_PER_GATE}"
        );
        let resp = match client.get_raw(&path).await {
            Ok(r) => r,
            Err(e) => return RunsRead::Blind(format!("GitHub unreachable: {e}")),
        };
        let header = |k: &str| {
            resp.headers()
                .get(k)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
        };
        if let Some(n) = header("x-ratelimit-remaining") {
            *self
                .remaining
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(n);
        }
        let status = resp.status().as_u16();
        if status == 404 {
            return RunsRead::Blind(format!("{repo}: workflow {workflow} not found"));
        }
        if status == 403 || status == 429 {
            return RunsRead::Blind(format!(
                "GitHub refused ({status}): rate limit or permission"
            ));
        }
        if !(200..300).contains(&status) {
            return RunsRead::Blind(format!("GitHub answered {status}"));
        }
        match resp.json::<RunsPage>().await {
            Ok(page) => RunsRead::Runs(
                page.workflow_runs
                    .into_iter()
                    .map(|r| Run {
                        conclusion: r.conclusion.unwrap_or_default(),
                        created_unix: chrono::DateTime::parse_from_rfc3339(&r.created_at)
                            .map_or(0, |t| u64::try_from(t.timestamp()).unwrap_or(0)),
                        created_at: r.created_at,
                        url: r.html_url,
                    })
                    .collect(),
            ),
            Err(e) => RunsRead::Blind(format!("unreadable runs page: {e}")),
        }
    }
}

pub struct HttpIssues;

#[derive(Deserialize)]
struct IssueRow {
    number: u64,
    title: String,
    body: Option<String>,
    pull_request: Option<Json>,
}

#[derive(Deserialize)]
struct Created {
    number: u64,
}

fn rest() -> Result<todoku::HttpClient> {
    crate::gh_auth::credentials()
        .rest_client(crate::provider::GITHUB_API)
        .map_err(|e| anyhow!("GitHub client: {e}"))
}

#[async_trait]
impl Issues for HttpIssues {
    async fn find(&self, repo: &str, label: &str, title: &str) -> Result<Option<OpenIssue>> {
        let rows: Vec<IssueRow> = rest()?
            .get(&format!(
                "/repos/{repo}/issues?labels={label}&state=open&per_page=100"
            ))
            .await
            .with_context(|| format!("listing {label} issues in {repo}"))?;
        Ok(rows
            .into_iter()
            .find(|r| r.pull_request.is_none() && r.title == title)
            .map(|r| OpenIssue {
                number: r.number,
                body: r.body.unwrap_or_default(),
            }))
    }

    async fn create(&self, repo: &str, label: &str, title: &str, body: &str) -> Result<u64> {
        let made: Created = rest()?
            .post(
                &format!("/repos/{repo}/issues"),
                &json!({"title": title, "body": body, "labels": [label]}),
            )
            .await
            .with_context(|| format!("opening an issue in {repo}"))?;
        Ok(made.number)
    }

    async fn update(&self, repo: &str, number: u64, body: &str) -> Result<()> {
        let _: Json = rest()?
            .request(
                reqwest::Method::PATCH,
                &format!("/repos/{repo}/issues/{number}"),
                Some(&json!({"body": body})),
            )
            .await
            .with_context(|| format!("updating {repo}#{number}"))?;
        Ok(())
    }

    async fn close(&self, repo: &str, number: u64, body: &str) -> Result<()> {
        let _: Json = rest()?
            .request(
                reqwest::Method::PATCH,
                &format!("/repos/{repo}/issues/{number}"),
                Some(&json!({"body": body, "state": "closed", "state_reason": "completed"})),
            )
            .await
            .with_context(|| format!("closing {repo}#{number}"))?;
        Ok(())
    }
}

pub async fn run_live(cfg: &DriftConfig, due: Due<'_>, path: &Path) -> Result<State> {
    let prior = read_state(path).unwrap_or_default();
    let actions = HttpActions::new(cfg.rate_limit_floor);
    let probes = Probes {
        runner: &SystemRunner,
        actions: &actions,
    };
    let mut state = sweep(cfg, &prior, due, SystemTime::now(), &probes).await?;
    if cfg.issues.enable {
        state.issues = apply_issues(&state.report["issues"], &cfg.issues.label, &HttpIssues).await;
    }
    write_state(path, &state)?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeRunner(Mutex<BTreeMap<String, (Option<i32>, String, String)>>);

    impl FakeRunner {
        fn with(cases: &[(&str, Option<i32>, &str, &str)]) -> Self {
            Self(Mutex::new(
                cases
                    .iter()
                    .map(|(d, c, o, e)| ((*d).to_owned(), (*c, (*o).to_owned(), (*e).to_owned())))
                    .collect(),
            ))
        }
        fn set(&self, dir: &str, code: Option<i32>, out: &str, err: &str) {
            self.0
                .lock()
                .unwrap()
                .insert(dir.to_owned(), (code, out.to_owned(), err.to_owned()));
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, _argv: &[String], cwd: &Path, _t: Duration) -> Ran {
            let key = instance_name(cwd);
            match self.0.lock().unwrap().get(&key) {
                Some((code, out, err)) => Ran::Exited {
                    code: *code,
                    stdout: out.clone(),
                    stderr: err.clone(),
                },
                None => Ran::Failed(format!("{key}: not found on PATH")),
            }
        }
    }

    struct FakeActions(Mutex<BTreeMap<String, Option<Vec<Run>>>>);

    #[async_trait]
    impl Actions for FakeActions {
        async fn runs(&self, repo: &str, _w: &str, _b: &str) -> RunsRead {
            match self.0.lock().unwrap().get(repo) {
                Some(Some(r)) => RunsRead::Runs(r.clone()),
                Some(None) => RunsRead::Blind(
                    "GitHub rate limit: 3 requests left, under the floor 200".to_owned(),
                ),
                None => RunsRead::Blind(format!("{repo}: workflow not found")),
            }
        }
    }

    #[derive(Default)]
    struct FakeIssues(Mutex<BTreeMap<(String, String), (u64, String, bool)>>);

    #[async_trait]
    impl Issues for FakeIssues {
        async fn find(&self, repo: &str, _l: &str, title: &str) -> Result<Option<OpenIssue>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(repo.to_owned(), title.to_owned()))
                .filter(|(_, _, open)| *open)
                .map(|(n, b, _)| OpenIssue {
                    number: *n,
                    body: b.clone(),
                }))
        }
        async fn create(&self, repo: &str, _l: &str, title: &str, body: &str) -> Result<u64> {
            let mut m = self.0.lock().unwrap();
            let n = m.len() as u64 + 1;
            m.insert(
                (repo.to_owned(), title.to_owned()),
                (n, body.to_owned(), true),
            );
            Ok(n)
        }
        async fn update(&self, repo: &str, number: u64, body: &str) -> Result<()> {
            for ((r, _), v) in self.0.lock().unwrap().iter_mut() {
                if r == repo && v.0 == number {
                    v.1 = body.to_owned();
                }
            }
            Ok(())
        }
        async fn close(&self, repo: &str, number: u64, _b: &str) -> Result<()> {
            for ((r, _), v) in self.0.lock().unwrap().iter_mut() {
                if r == repo && v.0 == number {
                    v.2 = false;
                }
            }
            Ok(())
        }
    }

    const HOUR: u64 = 3600;
    const NOW: u64 = 1_800_000_000;

    fn run(conclusion: &str, hours_ago: u64) -> Run {
        let t = NOW - hours_ago * HOUR;
        Run {
            conclusion: conclusion.to_owned(),
            created_unix: t,
            created_at: format!("t-{hours_ago}h"),
            url: format!("https://github.com/pleme-io/flows/actions/runs/{t}"),
        }
    }

    fn fixture(root: &Path) -> DriftConfig {
        for d in ["repo-forge", "kensa", "helmworks"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let at = |d: &str| root.join(d).display().to_string();
        let mut checks = BTreeMap::new();
        checks.insert(
            "repo".to_owned(),
            DriftCheck {
                read: ReadAs::RepoForgeNdjson,
                ..command_check(
                    "repo",
                    "pleme-io/repo-forge",
                    &["repo-forge"],
                    &[&at("repo-forge")],
                    60,
                )
            },
        );
        checks.insert(
            "reusable-workflow".to_owned(),
            gate_check(
                "reusable-workflow",
                "pleme-io/flows",
                &["pleme-io/flows"],
                "substrate-parity.yml",
            ),
        );
        checks.insert(
            "service".to_owned(),
            command_check(
                "service",
                "pleme-io/ola-mundo",
                &["nix"],
                &[&at("kensa")],
                60,
            ),
        );
        checks.insert(
            "chart".to_owned(),
            command_check(
                "chart",
                "pleme-io/helmworks",
                &["nix"],
                &[&at("helmworks")],
                60,
            ),
        );
        DriftConfig {
            checks,
            issues: DriftIssuesConfig {
                enable: true,
                label: DEFAULT_LABEL.to_owned(),
            },
            ..DriftConfig::default()
        }
    }

    const DRIFTED: &str = r#"{"drifts":[{"class":"hybrid","kind":"missing","path":"flake.nix"}],"repo":"acervo","summary":{"boilerplate":0,"hybrid":1,"authored":0,"missing":1,"modified":0}}
{"drifts":[],"repo":"kensa","summary":{"boilerplate":0,"hybrid":0,"authored":0,"missing":0,"modified":0}}
{"clean":1,"drifted":1,"missing_repos":0,"total":2}"#;
    const CLEAN: &str = r#"{"drifts":[],"repo":"acervo","summary":{"boilerplate":0,"hybrid":0,"authored":0,"missing":0,"modified":0}}
{"drifts":[],"repo":"kensa","summary":{"boilerplate":0,"hybrid":0,"authored":0,"missing":0,"modified":0}}
{"clean":2,"drifted":0,"missing_repos":0,"total":2}"#;

    fn verdict<'a>(state: &'a State, name: &str) -> &'a str {
        state.report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .and_then(|c| c["verdict"].as_str())
            .unwrap()
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[tokio::test]
    async fn red_then_green_with_one_issue_per_kind_and_a_blind_check() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = fixture(tmp.path());
        let runner = FakeRunner::with(&[
            ("repo-forge", Some(1), DRIFTED, ""),
            ("kensa", Some(0), "", ""),
            (
                "helmworks",
                Some(1),
                "",
                "error: flake 'git+file:///helmworks' does not provide attribute",
            ),
        ]);
        let actions = FakeActions(Mutex::new(BTreeMap::from([(
            "pleme-io/flows".to_owned(),
            Some(vec![
                run("failure", 2),
                run("failure", 30),
                run("failure", 72),
                run("success", 80),
            ]),
        )])));
        let issues = FakeIssues::default();
        let probes = Probes {
            runner: &runner,
            actions: &actions,
        };

        let red = sweep(&cfg, &State::default(), Due::All, at(NOW), &probes)
            .await
            .unwrap();
        assert_eq!(red.report["verdict"], "alert");
        assert_eq!(verdict(&red, "repo"), "drifted");
        assert_eq!(verdict(&red, "reusable-workflow"), "red-past-window");
        assert_eq!(verdict(&red, "service"), "green");
        assert_eq!(verdict(&red, "chart"), "drifted");
        let flows = red.report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "reusable-workflow")
            .unwrap();
        assert_eq!(flows["alerts"][0]["since"], "t-72h");
        let made = apply_issues(&red.report["issues"], DEFAULT_LABEL, &issues).await;
        assert_eq!(
            made.iter()
                .filter(|m| m["result"].as_str().unwrap_or("").starts_with("created"))
                .count(),
            3
        );
        let again = apply_issues(&red.report["issues"], DEFAULT_LABEL, &issues).await;
        assert!(again
            .iter()
            .all(|m| !m["result"].as_str().unwrap_or("").starts_with("created")));
        assert_eq!(issues.0.lock().unwrap().len(), 3);

        runner.set("repo-forge", Some(0), CLEAN, "");
        runner.set("helmworks", Some(0), "", "");
        actions.0.lock().unwrap().insert(
            "pleme-io/flows".to_owned(),
            Some(vec![run("success", 1), run("failure", 2)]),
        );
        let green = sweep(&cfg, &red, Due::All, at(NOW + HOUR), &probes)
            .await
            .unwrap();
        assert_eq!(green.report["verdict"], "green");
        let closed = apply_issues(&green.report["issues"], DEFAULT_LABEL, &issues).await;
        assert_eq!(
            closed
                .iter()
                .filter(|m| m["result"].as_str().unwrap_or("").starts_with("closed"))
                .count(),
            3
        );
        assert!(issues.0.lock().unwrap().values().all(|(_, _, open)| !open));

        actions
            .0
            .lock()
            .unwrap()
            .insert("pleme-io/flows".to_owned(), None);
        let blind = sweep(
            &cfg,
            &green,
            Due::Only("reusable-workflow"),
            at(NOW + 2 * HOUR),
            &probes,
        )
        .await
        .unwrap();
        assert_eq!(verdict(&blind, "reusable-workflow"), "blind");
        assert_eq!(blind.report["verdict"], "blind");
        let held = apply_issues(&blind.report["issues"], DEFAULT_LABEL, &issues).await;
        assert!(held.iter().all(|m| m["action"] != "open"));
        let flows_plan = blind.report["issues"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["template"] == "pleme-io/flows")
            .unwrap();
        assert_eq!(flows_plan["action"], "hold");
    }

    #[tokio::test]
    async fn a_command_not_on_path_and_a_partial_run_are_blind_never_green() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = fixture(tmp.path());
        let partial = r#"{"drifts":[],"repo":"kensa","summary":{"boilerplate":0,"hybrid":0,"authored":0,"missing":0,"modified":0}}"#;
        let runner = FakeRunner::with(&[(
            "repo-forge",
            Some(1),
            partial,
            "Error: render arnes\n\nCaused by:\n    `binary` is unset\n\nStack backtrace:\n  0: x",
        )]);
        let actions = FakeActions(Mutex::new(BTreeMap::new()));
        let probes = Probes {
            runner: &runner,
            actions: &actions,
        };
        let s = sweep(&cfg, &State::default(), Due::All, at(NOW), &probes)
            .await
            .unwrap();
        assert_eq!(verdict(&s, "repo"), "blind");
        assert_eq!(verdict(&s, "service"), "blind");
        assert_eq!(verdict(&s, "reusable-workflow"), "blind");
        let repo = s.report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "repo")
            .unwrap();
        assert_eq!(
            repo["blind"][0]["detail"],
            "stopped before the fleet rollup: Error: render arnes: `binary` is unset"
        );
        assert!(s.report["issues"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["action"] != "close"));
    }

    #[tokio::test]
    async fn a_check_runs_at_its_cadence_and_an_off_check_is_off() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = fixture(tmp.path());
        cfg.checks.get_mut("chart").unwrap().enable = false;
        let runner = FakeRunner::with(&[
            ("repo-forge", Some(0), CLEAN, ""),
            ("kensa", Some(0), "", ""),
        ]);
        let actions = FakeActions(Mutex::new(BTreeMap::from([(
            "pleme-io/flows".to_owned(),
            Some(vec![run("success", 1)]),
        )])));
        let probes = Probes {
            runner: &runner,
            actions: &actions,
        };
        let first = sweep(&cfg, &State::default(), Due::Cadence, at(NOW), &probes)
            .await
            .unwrap();
        assert_eq!(first.report["verdict"], "green");
        assert_eq!(verdict(&first, "chart"), "off");
        runner.set("kensa", Some(1), "", "error: checks.project differs");
        let soon = sweep(&cfg, &first, Due::Cadence, at(NOW + 60), &probes)
            .await
            .unwrap();
        assert_eq!(verdict(&soon, "service"), "green");
        let later = sweep(&cfg, &soon, Due::Cadence, at(NOW + HOUR), &probes)
            .await
            .unwrap();
        assert_eq!(verdict(&later, "service"), "drifted");
    }

    #[test]
    fn a_bad_entry_is_refused_alone_and_reads_blind() {
        let mut cfg = DriftConfig::default();
        cfg.checks.insert(
            "broken".to_owned(),
            DriftCheck {
                kind: "x".to_owned(),
                template: "nope".to_owned(),
                ..DriftCheck::default()
            },
        );
        let out = judge(&cfg, &BTreeMap::new(), NOW).unwrap();
        let broken = out["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "broken")
            .unwrap();
        assert_eq!(broken["verdict"], "blind");
        assert_eq!(
            broken["blind"][0]["detail"],
            "refused: template `nope` is not owner/repo"
        );
        assert!(DriftConfig::default()
            .checks
            .values()
            .all(|c| c.refusal().is_none()));
    }

    #[test]
    fn the_default_set_is_the_doctrine_table() {
        let kinds: Vec<String> = default_checks().values().map(|c| c.kind.clone()).collect();
        for k in [
            "repo",
            "reusable-workflow",
            "action",
            "bidama",
            "service",
            "chart",
            "skill",
        ] {
            assert!(kinds.iter().any(|x| x == k), "no default check for {k}");
        }
    }

    #[test]
    fn the_status_line_shows_state_as_a_symbol() {
        assert!(status_line(None).contains("blind"));
        assert!(session_note(None, at(NOW)).is_some());
    }

    #[test]
    fn the_system_runner_reports_a_missing_program_as_a_failure() {
        let r = SystemRunner.run(
            &["tend-drift-no-such-program".to_owned()],
            Path::new("."),
            Duration::from_secs(5),
        );
        assert!(matches!(r, Ran::Failed(w) if w.contains("not found")));
    }
}
