use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::config::Workspace;
use crate::drift::{DriftEvent, DriftSink};
use crate::flake::{self, ExecOptions, ExecSummary};
use crate::flake_graph::FlakeGraph;
use crate::head_cache::UpstreamHead;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Condition {
    StaleFlakeLock,
    UncleanGit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Remedy {
    GatedFlakeUpdateAndPush,
    RecordInDoctor,
}

impl Condition {
    pub(crate) const ALL: [Condition; 2] = [Condition::StaleFlakeLock, Condition::UncleanGit];

    pub(crate) const fn remedy(self) -> Remedy {
        match self {
            Condition::StaleFlakeLock => Remedy::GatedFlakeUpdateAndPush,
            Condition::UncleanGit => Remedy::RecordInDoctor,
        }
    }

    pub(crate) const fn gated_before_push(self) -> bool {
        match self.remedy() {
            Remedy::GatedFlakeUpdateAndPush => true,
            Remedy::RecordInDoctor => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, rename_all = "snake_case")]
pub(crate) struct ConditionsConfig {
    pub stale_flake_lock: bool,
    pub unclean_git: bool,
}

impl Default for ConditionsConfig {
    fn default() -> Self {
        Self {
            stale_flake_lock: Condition::StaleFlakeLock.gated_before_push(),
            unclean_git: Condition::UncleanGit.gated_before_push(),
        }
    }
}

impl ConditionsConfig {
    pub(crate) const fn off() -> Self {
        Self {
            stale_flake_lock: false,
            unclean_git: false,
        }
    }

    pub(crate) const fn enabled(&self, c: Condition) -> bool {
        match c {
            Condition::StaleFlakeLock => self.stale_flake_lock,
            Condition::UncleanGit => self.unclean_git,
        }
    }
}

pub(crate) async fn remedy_stale_flake_lock(
    ws: &Workspace,
    upstream: &dyn UpstreamHead,
    opts: ExecOptions,
) -> anyhow::Result<ExecSummary> {
    let (graph, _skips) = FlakeGraph::for_workspace(ws)?;
    if graph.deps().is_empty() {
        return Ok(ExecSummary::default());
    }
    let chain = graph.to_input_names(flake::compute_update_chain_all(graph.deps())?);
    let (chain, _dropped) = flake::filter_to_divergent(ws, chain, Some(upstream)).await?;
    if chain.is_empty() {
        return Ok(ExecSummary::default());
    }
    let owned = ws.clone();
    tokio::task::spawn_blocking(move || flake::execute_update_chain(&owned, &chain, opts)).await?
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dirt {
    pub paths: Vec<String>,
    pub age_secs: u64,
}

pub(crate) fn observe_dirt(repo: &Path, now: SystemTime) -> Option<Dirt> {
    let out = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let paths = parse_porcelain_z(&out.stdout);
    if paths.is_empty() {
        return None;
    }
    let age_secs = paths
        .iter()
        .filter_map(|p| {
            std::fs::symlink_metadata(repo.join(p))
                .ok()?
                .modified()
                .ok()
        })
        .filter_map(|m| now.duration_since(m).ok())
        .map(|d| d.as_secs())
        .max()
        .unwrap_or(0);
    Some(Dirt { paths, age_secs })
}

fn parse_porcelain_z(raw: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        out.push(String::from_utf8_lossy(&entry[3..]).into_owned());
        if status.contains(&b'R') || status.contains(&b'C') {
            fields.next();
        }
    }
    out
}

pub(crate) fn record_unclean(
    ws: &Workspace,
    repos: &[String],
    sink: &dyn DriftSink,
    now: SystemTime,
) -> usize {
    let Ok(base) = ws.resolved_base_dir() else {
        return 0;
    };
    let mut n = 0;
    for repo in repos {
        let path = base.join(repo);
        if !path.join(".git").exists() {
            continue;
        }
        if let Some(d) = observe_dirt(&path, now) {
            sink.record(&DriftEvent::UncleanAfterCycle {
                workspace: ws.name.clone(),
                repo_name: repo.clone(),
                paths: d.paths,
                age_secs: d.age_secs,
            });
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drift::InMemoryDriftSink;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    fn workspace(base: &Path) -> Workspace {
        let mut ws = Workspace::test_default("w");
        ws.base_dir = base.display().to_string();
        ws
    }

    #[test]
    fn every_condition_has_a_remedy_and_defaults_on_only_when_gated() {
        let cfg = ConditionsConfig::default();
        for c in Condition::ALL {
            assert_eq!(cfg.enabled(c), c.gated_before_push(), "{c:?}");
        }
    }

    #[test]
    fn a_condition_configured_off_stays_off() {
        let cfg: ConditionsConfig = serde_yaml_ng::from_str("stale_flake_lock: false").unwrap();
        assert!(!cfg.enabled(Condition::StaleFlakeLock));
        assert!(cfg.enabled(Condition::UncleanGit));
    }

    #[test]
    fn porcelain_z_keeps_paths_with_spaces_and_skips_rename_sources() {
        let raw = b" M a b.txt\0R  new.rs\0old.rs\0?? dir/x\0";
        assert_eq!(parse_porcelain_z(raw), vec!["a b.txt", "new.rs", "dir/x"]);
    }

    #[test]
    fn foreign_dirt_is_recorded_with_paths_and_never_touched() {
        let base = tempfile::tempdir().unwrap();
        for r in ["clean", "dirty"] {
            let p = base.path().join(r);
            std::fs::create_dir_all(&p).unwrap();
            git(&p, &["init", "-q"]);
            std::fs::write(p.join("f"), "1").unwrap();
            git(&p, &["add", "f"]);
            git(&p, &["commit", "-qm", "init"]);
        }
        let dirty = base.path().join("dirty");
        std::fs::write(dirty.join("f"), "2").unwrap();
        std::fs::write(dirty.join("new"), "x").unwrap();
        let later = SystemTime::now() + std::time::Duration::from_secs(90);
        let sink = InMemoryDriftSink::new();
        let n = record_unclean(
            &workspace(base.path()),
            &["clean".into(), "dirty".into(), "absent".into()],
            &sink,
            later,
        );
        assert_eq!(n, 1);
        let events = sink.snapshot();
        let DriftEvent::UncleanAfterCycle {
            repo_name,
            paths,
            age_secs,
            ..
        } = &events[0]
        else {
            panic!("{events:?}");
        };
        assert_eq!(repo_name, "dirty");
        assert_eq!(paths, &vec!["f".to_string(), "new".to_string()]);
        assert!(*age_secs >= 89);
        assert_eq!(std::fs::read_to_string(dirty.join("f")).unwrap(), "2");
        assert!(dirty.join("new").exists());
    }
}
