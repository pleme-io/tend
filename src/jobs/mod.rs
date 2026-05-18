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
//! - [`pull_repo`] — wraps `sync::pull_one_repo` (M0.10b)
//! - [`status_repo`] — wraps `sync::check_one_repo_status` (M0.10d)
//! - [`fetch_repo`] — wraps `sync::fetch_one_repo` (M0.10e)
//! - [`sync_repo`] — wraps `sync::sync_one_repo` (M0.10f)
//! - [`discover_org`] — wraps `provider::discover_github_repos_cached` (M0.19 / M2)
//!
//! # Wrapper pattern (per CSE principle 1: solve problems once)
//!
//! Each Job follows the same six-piece shape:
//!
//! 1. A `pub(crate) const X_REPO_KIND: &str = "tend.x-repo";` canonical
//!    kind id (so `JobId::kind` is the single source of truth for which
//!    operation this is).
//! 2. A `#[derive(Debug, Clone)] pub(crate) struct XRepoJob { workspace,
//!    repo_name, repo_path, [quiet?] }` pure-data struct. Pre-resolved
//!    path so `execute` has no I/O at construction.
//! 3. A `new(workspace, repo_name, repo_path)` constructor with `Into`
//!    polymorphism on each argument.
//! 4. A `pub(crate) enum XRepoError { Invocation(String) }` carrying
//!    only invocation failures. Typed-success-with-failure-outcome
//!    (e.g. `PullOutcome::Failed`) flows through `Output`, not `Error`,
//!    so the scheduler can decide retries from the Output.
//! 5. `Job::id()` returns `JobScope::Workspace(...)` + `JobSubject::Repo(...)`
//!    + `JobKindId::new(X_REPO_KIND)` — the three coordinates fully
//!    name the Job across all tend workspaces.
//! 6. `Job::execute()` is a `spawn_blocking` hop into the matching
//!    `x_one_repo` helper in `sync.rs`. The helper owns the actual
//!    state machine; this method just adapts the call to async +
//!    lifts errors into the typed `XRepoError`.
//!
//! Three instances is the right point to write this down. At the
//! fourth (or if duplication exceeds ~200 LOC), evaluate extracting
//! the shape into a `macro_rules!` form — but only if a 4th data
//! point confirms the variation across Jobs (e.g. status takes no
//! `quiet`; pull/fetch take three helper args; future jobs may take
//! more) is uniform enough to template cleanly. Premature extraction
//! produces a brittle macro for marginal LOC savings.

pub(crate) mod discover_org;
pub(crate) mod fetch_repo;
pub(crate) mod pull_repo;
pub(crate) mod status_repo;
pub(crate) mod sync_repo;
