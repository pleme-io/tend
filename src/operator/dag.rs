//! DAG planner — consumes `shigoto-dag`.
//!
//! The typed DAG implementation has been lifted into `shigoto-dag` (per
//! theory/SHIGOTO.md §IV.3 M0.7). This module is now a thin tend-side
//! adapter:
//!   - Re-exports `shigoto_dag::Dag` as `FleetDag` for source compat
//!     with existing `operator/reconcile.rs` call sites.
//!   - Provides `DagNodeId::new(repo, input)` as a typed constructor
//!     that builds a flake-update-shaped `shigoto_types::JobId`
//!     (scope=Repo, kind="flake-update", subject=Pinned(input_name)).
//!
//! Keeping the tend-side surface (`FleetDag`, `DagNodeId`) lets the
//! existing reconcile.rs compile unchanged; the underlying graph is
//! the canonical shigoto-dag implementation.

pub use shigoto_dag::{Dag as FleetDag, DagError};
use shigoto_types::{JobId, JobKindId, JobScope, JobSubject};

/// Source-compat shim: tend's existing reconcile.rs constructs nodes
/// via `DagNodeId::new(repo, input)`. We keep that callsite shape by
/// exposing `DagNodeId` as a zero-sized type whose `::new` builds the
/// flake-update-shaped `shigoto_types::JobId` (scope=Repo,
/// kind="flake-update", subject=Pinned(input)). Existing call sites
/// compile unchanged.
pub struct DagNodeId;

impl DagNodeId {
    #[must_use]
    pub fn new(repo: impl Into<String>, input: impl Into<String>) -> JobId {
        JobId {
            scope: JobScope::Repo {
                workspace: "fleet".into(),
                repo: repo.into(),
            },
            kind: JobKindId::new("flake-update"),
            subject: JobSubject::Pinned(input.into()),
        }
    }
}

/// Extract the flake-update input name from a `JobId` whose `subject`
/// is `Pinned(input_name)`. Returns `None` for JobIds that aren't
/// flake-update-shaped (which would be a programming error in the
/// flake-update reconcile path).
#[must_use]
pub fn flake_input_name(id: &JobId) -> Option<&str> {
    match &id.subject {
        JobSubject::Pinned(s) => Some(s),
        _ => None,
    }
}
