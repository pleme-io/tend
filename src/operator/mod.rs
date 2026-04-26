//! Fleet update controller — Rust K8s operator built into tend.
//!
//! See `docs/OPERATOR-DESIGN.md` for the full architectural decisions.
//! This module is feature-gated behind `--features operator` so the
//! default tend CLI doesn't pay the kube-rs dependency cost.

pub mod crds;
pub mod flake_lock_adapter;
pub mod lock_format;

use anyhow::Result;

/// Entry point for the `tend operator` subcommand. Currently a scaffold
/// — registers nothing, reconciles nothing. The next session wires the
/// actual `kube::runtime::Controller<FlakeUpdatePolicy>` and the
/// discover → propose → DAG → verify → apply loop per the design doc.
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tend=debug".into()),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "tend operator: scaffold loaded; reconcile loop not yet wired \
         (see docs/OPERATOR-DESIGN.md for next-session plan)"
    );

    Ok(())
}
