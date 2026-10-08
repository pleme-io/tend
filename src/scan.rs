use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Workspace;
use crate::sync::{self, RepoFacts, RepoStatus, StateWord};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusSnapshotConfig {
    pub enable: bool,
    pub max_age_minutes: u64,
    pub workers: usize,
}

impl Default for StatusSnapshotConfig {
    fn default() -> Self {
        Self {
            enable: true,
            max_age_minutes: 30,
            workers: 4,
        }
    }
}

impl StatusSnapshotConfig {
    #[must_use]
    pub fn max_age(&self) -> Duration {
        Duration::from_secs(self.max_age_minutes.saturating_mul(60))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalRepo {
    pub workspace: String,
    pub name: String,
    pub path: PathBuf,
}

pub(crate) fn local_repos(workspaces: &[&Workspace]) -> Vec<LocalRepo> {
    let mut repos = Vec::new();
    for ws in workspaces {
        let Ok(base) = ws.resolved_base_dir() else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.join(".git").exists() {
                repos.push(LocalRepo {
                    workspace: ws.name.clone(),
                    name,
                    path,
                });
            }
        }
    }
    repos.sort_by(|a, b| a.path.cmp(&b.path));
    repos
}

pub(crate) fn enclosing_repo(workspaces: &[&Workspace], cwd: &Path) -> Option<LocalRepo> {
    let top = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    workspaces.iter().find_map(|ws| {
        let base = ws.resolved_base_dir().ok()?;
        let rel = top.strip_prefix(&base).ok()?;
        if rel.as_os_str().is_empty() {
            return None;
        }
        Some(LocalRepo {
            workspace: ws.name.clone(),
            name: rel.to_string_lossy().into_owned(),
            path: top.to_path_buf(),
        })
    })
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn git_marker_paths(repo: &Path) -> Vec<PathBuf> {
    let Some(git_dir) = sync::git_dir(repo) else {
        return Vec::new();
    };
    let mut paths = vec![git_dir.join("index"), git_dir.join("HEAD")];
    if let Some(branch) = sync::head_branch(&git_dir) {
        paths.push(
            sync::common_dir(&git_dir)
                .join("refs")
                .join("heads")
                .join(branch),
        );
    }
    paths
}

pub(crate) fn touched_since(repo: &Path, since: SystemTime) -> bool {
    git_marker_paths(repo)
        .iter()
        .filter_map(|p| mtime(p))
        .any(|m| m > since)
}

pub(crate) fn last_activity(repo: &Path) -> Option<SystemTime> {
    git_marker_paths(repo).iter().filter_map(|p| mtime(p)).max()
}

pub(crate) type Observed = Option<(RepoStatus, RepoFacts)>;

pub(crate) type Probe = fn(&Path) -> Result<Observed>;

pub(crate) fn full_probe(path: &Path) -> Result<Observed> {
    sync::observe_one_repo(path).map(Some)
}

pub(crate) fn unsettled_probe(path: &Path) -> Result<Observed> {
    sync::observe_unsettled(path)
}

#[derive(Debug)]
pub(crate) struct Scanned {
    pub repo: LocalRepo,
    pub outcome: Result<Observed, String>,
}

#[derive(Debug, Default)]
pub(crate) struct ScanResult {
    pub scanned: Vec<Scanned>,
    pub unchecked: Vec<LocalRepo>,
}

impl ScanResult {
    pub(crate) fn rows(&self) -> Vec<ScanRow> {
        self.scanned
            .iter()
            .filter_map(|s| match &s.outcome {
                Ok(Some((status, facts))) => Some(ScanRow::new(&s.repo, status, facts)),
                Ok(None) | Err(_) => None,
            })
            .collect()
    }

    pub(crate) fn failed(&self) -> usize {
        self.scanned.iter().filter(|s| s.outcome.is_err()).count()
    }
}

pub(crate) fn classify(
    repos: Vec<LocalRepo>,
    probe: Probe,
    workers: usize,
    deadline: Option<Instant>,
) -> ScanResult {
    let next = AtomicUsize::new(0);
    let workers = workers.clamp(1, repos.len().max(1));
    let finished: Vec<(usize, Result<Observed, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(repo) = repos.get(i) else {
                            break;
                        };
                        out.push((i, probe(&repo.path).map_err(|e| format!("{e:#}"))));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });

    let mut by_index: Vec<Option<Result<Observed, String>>> =
        (0..repos.len()).map(|_| None).collect();
    for (i, outcome) in finished {
        by_index[i] = Some(outcome);
    }
    let mut result = ScanResult::default();
    for (repo, outcome) in repos.into_iter().zip(by_index) {
        match outcome {
            Some(outcome) => result.scanned.push(Scanned { repo, outcome }),
            None => result.unchecked.push(repo),
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScanRow {
    pub workspace: String,
    pub name: String,
    pub path: PathBuf,
    pub state: StateWord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_count: Option<usize>,
}

impl ScanRow {
    pub(crate) fn new(repo: &LocalRepo, status: &RepoStatus, facts: &RepoFacts) -> Self {
        Self {
            workspace: repo.workspace.clone(),
            name: repo.name.clone(),
            path: repo.path.clone(),
            state: status.word(),
            branch: facts.branch.clone(),
            ahead: facts.ahead,
            behind: facts.behind,
            dirty_count: facts.dirty_count,
        }
    }

    pub(crate) fn label(&self) -> String {
        match &self.branch {
            Some(branch) => format!("{}/{}@{branch}", self.workspace, self.name),
            None => format!("{}/{}", self.workspace, self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub taken_at_unix: u64,
    pub total: usize,
    pub rows: Vec<ScanRow>,
}

impl Snapshot {
    pub(crate) fn taken_at(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.taken_at_unix)
    }

    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.taken_at()).unwrap_or_default()
    }
}

pub(crate) fn snapshot_path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("status")
        .join("local.json")
}

pub(crate) fn read_snapshot(path: &Path) -> Option<Snapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(snapshot)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
}

pub(crate) fn refresh_snapshot(
    workspaces: &[&Workspace],
    workers: usize,
    path: &Path,
    started: SystemTime,
) -> Result<Snapshot> {
    let repos = local_repos(workspaces);
    let total = repos.len();
    let result = classify(repos, full_probe, workers, None);
    let snapshot = Snapshot {
        taken_at_unix: started
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        total,
        rows: result.rows(),
    };
    write_snapshot(path, &snapshot)?;
    Ok(snapshot)
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

    fn repo_with_commit(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(path, &["config", "user.email", "t@t"]);
        git(path, &["config", "user.name", "t"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        std::fs::write(path.join("f"), "x\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-q", "--no-verify", "-m", "init"]);
    }

    fn workspace(name: &str, base: &Path) -> Workspace {
        let mut ws = Workspace::test_default(name);
        ws.base_dir = base.to_string_lossy().into_owned();
        ws
    }

    #[test]
    fn local_repos_lists_git_dirs_one_level_down_and_skips_the_rest() {
        let tmp = tempfile::TempDir::new().unwrap();
        repo_with_commit(&tmp.path().join("b-repo"));
        repo_with_commit(&tmp.path().join("a-repo"));
        std::fs::create_dir_all(tmp.path().join("not-a-repo")).unwrap();
        repo_with_commit(&tmp.path().join(".hidden"));
        let ws = workspace("ws", tmp.path());

        let names: Vec<String> = local_repos(&[&ws]).into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["a-repo", "b-repo"]);
    }

    #[test]
    fn the_enclosing_repo_is_found_from_a_subdirectory_and_named_relative_to_its_workspace() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        repo_with_commit(&repo);
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        let ws = workspace("ws", tmp.path());

        let found = enclosing_repo(&[&ws], &repo.join("src/deep")).unwrap();
        assert_eq!(found.name, "repo");
        assert_eq!(found.workspace, "ws");
        assert_eq!(found.path, repo);

        let outside = tempfile::TempDir::new().unwrap();
        repo_with_commit(&outside.path().join("elsewhere"));
        assert!(enclosing_repo(&[&ws], &outside.path().join("elsewhere")).is_none());
    }

    #[test]
    fn touched_since_sees_index_head_and_branch_ref_writes_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        repo_with_commit(&repo);
        std::thread::sleep(Duration::from_millis(20));
        let since = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));

        std::fs::write(repo.join("untracked-edit"), "dirty\n").unwrap();
        assert!(
            !touched_since(&repo, since),
            "a working-tree write is not a git-state touch"
        );

        git(&repo, &["add", "untracked-edit"]);
        assert!(touched_since(&repo, since), "staging rewrites the index");
    }

    #[test]
    fn classify_stops_at_the_deadline_and_reports_what_it_never_reached() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["a", "b", "c"] {
            repo_with_commit(&tmp.path().join(name));
        }
        let ws = workspace("ws", tmp.path());
        let repos = local_repos(&[&ws]);

        let expired = classify(repos.clone(), full_probe, 2, Some(Instant::now()));
        assert!(expired.scanned.is_empty());
        assert_eq!(expired.unchecked.len(), 3);

        let full = classify(repos, full_probe, 2, None);
        assert_eq!(full.scanned.len(), 3);
        assert!(full.unchecked.is_empty());
        assert!(full.rows().iter().all(|r| r.state == StateWord::NoRemote));
    }

    #[test]
    fn a_snapshot_round_trips_and_carries_its_age() {
        let tmp = tempfile::TempDir::new().unwrap();
        repo_with_commit(&tmp.path().join("repo"));
        let ws = workspace("ws", tmp.path());
        let path = tmp.path().join("cache/status/local.json");
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);

        let written = refresh_snapshot(&[&ws], 2, &path, started).unwrap();
        assert_eq!(written.total, 1);
        let read = read_snapshot(&path).unwrap();
        assert_eq!(read, written);
        assert_eq!(read.taken_at(), started);
        assert_eq!(
            read.age(started + Duration::from_secs(90)),
            Duration::from_secs(90)
        );
    }
}
