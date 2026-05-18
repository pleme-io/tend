//! Typed Job impls that wrap tend's existing primitives so they can be
//! scheduled by `shigoto::Scheduler`. The legacy planner/daemon path
//! still works; this module is the migration target: each existing batch
//! call becomes one or more Job impls registered with the scheduler.
//!
//! Per Constructive Substrate Engineering principle 1 ("solve problems
//! once, in one place"): the Jobs are *thin wrappers* around the
//! existing batch primitives — the pull/sync/status state machines stay
//! in `sync.rs` and `git.rs`. The Jobs only adapt them to shigoto's
//! typed Input/Output/Error surface.
//!
//! Roadmap:
//! - [`pull_repo`] — wraps `sync::pull_one_repo` for one repo (M0.10b)
//! - [`status_repo`] — wraps `sync::check_one_repo_status` (M0.10d)
//! - sync_repo  — wraps `sync::sync_repos` for one repo (planned)
//! - fetch_repo — wraps `sync::fetch_repos` for one repo (planned)

pub(crate) mod pull_repo;
pub(crate) mod status_repo;
