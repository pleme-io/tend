use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::audit::AuditLog;
use crate::config::{PushPolicy, Workspace};
use crate::sync::{BranchStatus, Head, RepoObservation, RepoStatus, StateWord, Tracking};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    Disabled,
    PolicyIsPr,
    Missing,
    NotAheadOnly(StateWord),
    Diverged { behind: u32 },
    Detached,
    NoUpstream,
    UnknownDefaultBranch,
    NotDefaultBranch { branch: String, default: String },
    UpstreamNameDiffers { merge: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Upstream {
    pub remote: String,
    pub merge: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushPlan {
    pub remote: String,
    pub branch: String,
    pub ahead: u32,
}

pub(crate) fn guard(
    status: &RepoStatus,
    branch: &BranchStatus,
    upstream: Option<&Upstream>,
    default_branch: Option<&str>,
) -> Result<PushPlan, Refusal> {
    if status.word() != StateWord::Ahead || branch.dirty > 0 {
        return Err(Refusal::NotAheadOnly(status.word()));
    }
    let Head::Branch(name) = &branch.head else {
        return Err(Refusal::Detached);
    };
    let Tracking::Compared { ahead, behind } = branch.tracking else {
        return Err(Refusal::NoUpstream);
    };
    if behind > 0 {
        return Err(Refusal::Diverged { behind });
    }
    if ahead == 0 {
        return Err(Refusal::NotAheadOnly(status.word()));
    }
    let upstream = upstream.ok_or(Refusal::NoUpstream)?;
    let default = default_branch.ok_or(Refusal::UnknownDefaultBranch)?;
    if name != default {
        return Err(Refusal::NotDefaultBranch {
            branch: name.clone(),
            default: default.to_owned(),
        });
    }
    if upstream.merge != format!("refs/heads/{name}") {
        return Err(Refusal::UpstreamNameDiffers {
            merge: upstream.merge.clone(),
        });
    }
    Ok(PushPlan {
        remote: upstream.remote.clone(),
        branch: name.clone(),
        ahead,
    })
}

fn git_value(repo: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("running git {args:?} in {}", repo.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!value.is_empty()).then_some(value))
}

fn upstream_of(repo: &Path, branch: &str) -> Result<Option<Upstream>> {
    let remote = git_value(
        repo,
        &["config", "--get", &format!("branch.{branch}.remote")],
    )?;
    let merge = git_value(
        repo,
        &["config", "--get", &format!("branch.{branch}.merge")],
    )?;
    Ok(remote
        .zip(merge)
        .map(|(remote, merge)| Upstream { remote, merge }))
}

fn default_branch(repo: &Path, remote: &str) -> Result<Option<String>> {
    let head = git_value(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    )?;
    let prefix = format!("refs/remotes/{remote}/");
    Ok(head.and_then(|h| h.strip_prefix(&prefix).map(ToOwned::to_owned)))
}

pub(crate) fn decide(ws: &Workspace, repo: &Path) -> Result<Result<PushPlan, Refusal>> {
    if !ws.push_ahead {
        return Ok(Err(Refusal::Disabled));
    }
    if ws.push_policy != PushPolicy::Main {
        return Ok(Err(Refusal::PolicyIsPr));
    }
    let Some(observation) = RepoObservation::observe(repo)? else {
        return Ok(Err(Refusal::Missing));
    };
    let status = observation.status();
    let branch = observation.branch();
    if status.word() != StateWord::Ahead {
        return Ok(Err(Refusal::NotAheadOnly(status.word())));
    }
    let upstream = match &branch.head {
        Head::Branch(name) => upstream_of(repo, name)?,
        Head::Unborn | Head::Detached => None,
    };
    let default = match &upstream {
        Some(up) => default_branch(repo, &up.remote)?,
        None => None,
    };
    Ok(guard(
        &status,
        branch,
        upstream.as_ref(),
        default.as_deref(),
    ))
}

pub(crate) fn push(repo: &Path, plan: &PushPlan) -> Result<(), String> {
    let refspec = format!("refs/heads/{0}:refs/heads/{0}", plan.branch);
    let output = Command::new("git")
        .args(["push", "--porcelain", &plan.remote, &refspec])
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(repo)
        .output()
        .map_err(|e| format!("running git push: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[derive(Debug)]
pub(crate) struct Pushed {
    pub repo: String,
    pub plan: PushPlan,
    pub result: Result<(), String>,
}

pub(crate) fn run_workspace(ws: &Workspace, repos: &[String], audit: &AuditLog) -> Vec<Pushed> {
    if !ws.push_ahead || ws.push_policy != PushPolicy::Main {
        return Vec::new();
    }
    let Ok(base) = ws.resolved_base_dir() else {
        return Vec::new();
    };
    let mut pushed = Vec::new();
    for name in repos {
        let path = base.join(name);
        let Ok(Ok(plan)) = decide(ws, &path) else {
            continue;
        };
        let result = push(&path, &plan);
        audit.ahead_pushed(
            &ws.name,
            name,
            &plan.remote,
            &plan.branch,
            plan.ahead,
            result.as_ref().err().map(String::as_str),
        );
        pushed.push(Pushed {
            repo: name.clone(),
            plan,
            result,
        });
    }
    pushed
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn rev(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn identity(repo: &Path) {
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["config", "commit.gpgsign", "false"]);
    }

    fn commit(repo: &Path, content: &str) {
        std::fs::write(repo.join("f"), content).unwrap();
        git(repo, &["add", "f"]);
        git(repo, &["commit", "-q", "--no-verify", "-m", content.trim()]);
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        base: std::path::PathBuf,
        upstream: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let base = tmp.path().join("ws");
            std::fs::create_dir_all(&base).unwrap();
            let upstream = tmp.path().join("upstream.git");
            std::fs::create_dir_all(&upstream).unwrap();
            git(&upstream, &["init", "-q", "--bare", "-b", "main"]);
            let seed = tmp.path().join("seed");
            std::fs::create_dir_all(&seed).unwrap();
            git(&seed, &["init", "-q", "-b", "main"]);
            identity(&seed);
            commit(&seed, "seed\n");
            git(
                &seed,
                &["remote", "add", "origin", &upstream.to_string_lossy()],
            );
            git(&seed, &["push", "-q", "origin", "main"]);
            git(&base, &["clone", "-q", &upstream.to_string_lossy(), "repo"]);
            identity(&base.join("repo"));
            Self {
                _tmp: tmp,
                base,
                upstream,
            }
        }

        fn repo(&self) -> std::path::PathBuf {
            self.base.join("repo")
        }

        fn workspace(&self, push_ahead: bool, policy: PushPolicy) -> Workspace {
            let mut ws = Workspace::test_default("ws");
            ws.base_dir = self.base.to_string_lossy().into_owned();
            ws.push_ahead = push_ahead;
            ws.push_policy = policy;
            ws
        }

        fn upstream_moves(&self) {
            let other = self.base.join("other");
            git(
                &self.base,
                &["clone", "-q", &self.upstream.to_string_lossy(), "other"],
            );
            identity(&other);
            commit(&other, "from elsewhere\n");
            git(&other, &["push", "-q", "origin", "main"]);
            git(&self.repo(), &["fetch", "-q", "origin"]);
        }

        fn audit(&self) -> AuditLog {
            AuditLog::new(self.base.join("audit.jsonl"))
        }

        fn audit_lines(&self) -> Vec<serde_json::Value> {
            std::fs::read_to_string(self.base.join("audit.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    fn names() -> Vec<String> {
        vec!["repo".to_owned()]
    }

    #[test]
    fn an_ahead_only_default_branch_is_fast_forward_pushed_and_audited() {
        let fx = Fixture::new();
        commit(&fx.repo(), "local work\n");
        let ws = fx.workspace(true, PushPolicy::Main);

        let pushed = run_workspace(&ws, &names(), &fx.audit());
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].plan.branch, "main");
        assert_eq!(pushed[0].plan.ahead, 1);
        assert!(pushed[0].result.is_ok(), "{:?}", pushed[0].result);
        assert_eq!(rev(&fx.upstream, "main"), rev(&fx.repo(), "HEAD"));

        let audit = fx.audit_lines();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0]["event"], "ahead_pushed");
        assert_eq!(audit[0]["repo"], "repo");
        assert_eq!(audit[0]["branch"], "main");
        assert_eq!(audit[0]["ok"], true);
    }

    #[test]
    fn the_knob_is_off_by_default_and_off_means_no_git_at_all() {
        let fx = Fixture::new();
        commit(&fx.repo(), "local work\n");
        let before = rev(&fx.upstream, "main");
        let ws = fx.workspace(false, PushPolicy::Main);
        assert!(!Workspace::test_default("x").push_ahead);

        assert_eq!(decide(&ws, &fx.repo()).unwrap(), Err(Refusal::Disabled));
        assert!(run_workspace(&ws, &names(), &fx.audit()).is_empty());
        assert_eq!(rev(&fx.upstream, "main"), before);
        assert!(fx.audit_lines().is_empty());
    }

    #[test]
    fn a_pr_only_workspace_never_pushes() {
        let fx = Fixture::new();
        commit(&fx.repo(), "local work\n");
        let before = rev(&fx.upstream, "main");
        let ws = fx.workspace(true, PushPolicy::Pr);

        assert_eq!(decide(&ws, &fx.repo()).unwrap(), Err(Refusal::PolicyIsPr));
        assert!(run_workspace(&ws, &names(), &fx.audit()).is_empty());
        assert_eq!(rev(&fx.upstream, "main"), before);
    }

    #[test]
    fn a_dirty_repo_is_never_pushed_even_when_ahead() {
        let fx = Fixture::new();
        commit(&fx.repo(), "local work\n");
        std::fs::write(fx.repo().join("scratch"), "uncommitted\n").unwrap();
        let before = rev(&fx.upstream, "main");
        let ws = fx.workspace(true, PushPolicy::Main);

        assert_eq!(
            decide(&ws, &fx.repo()).unwrap(),
            Err(Refusal::NotAheadOnly(StateWord::Dirty))
        );
        assert!(run_workspace(&ws, &names(), &fx.audit()).is_empty());
        assert_eq!(rev(&fx.upstream, "main"), before);
    }

    #[test]
    fn a_behind_repo_is_never_pushed() {
        let fx = Fixture::new();
        fx.upstream_moves();
        let ws = fx.workspace(true, PushPolicy::Main);

        assert_eq!(
            decide(&ws, &fx.repo()).unwrap(),
            Err(Refusal::NotAheadOnly(StateWord::Behind))
        );
        assert!(run_workspace(&ws, &names(), &fx.audit()).is_empty());
    }

    #[test]
    fn a_diverged_repo_is_never_pushed() {
        let fx = Fixture::new();
        fx.upstream_moves();
        commit(&fx.repo(), "local work\n");
        let before = rev(&fx.upstream, "main");
        let ws = fx.workspace(true, PushPolicy::Main);

        assert_eq!(
            decide(&ws, &fx.repo()).unwrap(),
            Err(Refusal::Diverged { behind: 1 })
        );
        assert!(run_workspace(&ws, &names(), &fx.audit()).is_empty());
        assert_eq!(rev(&fx.upstream, "main"), before);
    }

    #[test]
    fn a_non_default_branch_is_never_pushed_even_when_it_tracks_an_upstream() {
        let fx = Fixture::new();
        git(&fx.repo(), &["checkout", "-q", "-b", "feat"]);
        commit(&fx.repo(), "feature\n");
        git(&fx.repo(), &["push", "-q", "-u", "origin", "feat"]);
        commit(&fx.repo(), "more feature\n");
        let before = rev(&fx.upstream, "feat");
        let ws = fx.workspace(true, PushPolicy::Main);

        assert_eq!(
            decide(&ws, &fx.repo()).unwrap(),
            Err(Refusal::NotDefaultBranch {
                branch: "feat".into(),
                default: "main".into()
            })
        );
        assert!(run_workspace(&ws, &names(), &fx.audit()).is_empty());
        assert_eq!(rev(&fx.upstream, "feat"), before);
    }

    #[test]
    fn an_unknown_default_branch_refuses_rather_than_guessing() {
        let fx = Fixture::new();
        git(
            &fx.repo(),
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        );
        commit(&fx.repo(), "local work\n");
        let ws = fx.workspace(true, PushPolicy::Main);

        assert_eq!(
            decide(&ws, &fx.repo()).unwrap(),
            Err(Refusal::UnknownDefaultBranch)
        );
    }

    #[test]
    fn the_guard_refuses_every_state_but_ahead_only() {
        let up = Upstream {
            remote: "origin".into(),
            merge: "refs/heads/main".into(),
        };
        let ahead_only = BranchStatus {
            head: Head::Branch("main".into()),
            tracking: Tracking::Compared {
                ahead: 2,
                behind: 0,
            },
            dirty: 0,
        };
        assert_eq!(
            guard(&RepoStatus::Ahead, &ahead_only, Some(&up), Some("main")),
            Ok(PushPlan {
                remote: "origin".into(),
                branch: "main".into(),
                ahead: 2
            })
        );
        for status in [
            RepoStatus::Dirty,
            RepoStatus::Stuck,
            RepoStatus::NoRemote,
            RepoStatus::NoUpstream,
            RepoStatus::Behind,
            RepoStatus::Unborn,
            RepoStatus::Missing,
            RepoStatus::Unknown,
        ] {
            assert!(
                guard(&status, &ahead_only, Some(&up), Some("main")).is_err(),
                "{status:?} must refuse"
            );
        }
        let detached = BranchStatus {
            head: Head::Detached,
            ..ahead_only.clone()
        };
        assert_eq!(
            guard(&RepoStatus::Ahead, &detached, Some(&up), Some("main")),
            Err(Refusal::Detached)
        );
        let renamed = Upstream {
            remote: "origin".into(),
            merge: "refs/heads/trunk".into(),
        };
        assert_eq!(
            guard(
                &RepoStatus::Ahead,
                &ahead_only,
                Some(&renamed),
                Some("main")
            ),
            Err(Refusal::UpstreamNameDiffers {
                merge: "refs/heads/trunk".into()
            })
        );
        assert_eq!(
            guard(&RepoStatus::Ahead, &ahead_only, None, Some("main")),
            Err(Refusal::NoUpstream)
        );
    }
}
