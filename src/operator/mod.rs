//! Fleet update controller — Rust K8s operator built into tend.
//!
//! See `docs/OPERATOR-DESIGN.md` for the full architectural design.
//! Feature-gated behind `--features operator` so the default tend
//! CLI doesn't pay the kube-rs dependency cost.

pub mod apply;
pub mod budget;
pub mod crds;
pub mod dag;
pub mod discovery;
pub mod failure_set;
pub mod flake_lock_adapter;
pub mod flake_nix;
pub mod fleet_advance_consumer;
pub mod fleet_watch;
pub mod gates;
pub mod git_ops;
pub mod head_cache;
pub mod helm_release_adapter;
pub mod lock_format;
pub mod metrics;
pub mod nats_throttle;
pub mod planner;
pub mod reconcile;
pub mod status;
pub mod throttle;
pub mod upstream;
pub mod workspace;

use anyhow::{Context as _, Result};
use futures::StreamExt;
use kube::{
    api::Api,
    runtime::{watcher::Config as WatcherConfig, Controller},
    Client,
};
use std::sync::Arc;

use crate::config::Config;
use reconcile::Context;

/// Entry point for the `tend operator` subcommand.
///
/// Spawns two `Controller`s — one per CRD kind — sharing a `Context`
/// with the kube client, parsed tend config, and an HTTP client used
/// by the upstream HEAD-resolver.
pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tend=debug,kube=info".into()),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "tend operator starting"
    );

    let client = Client::try_default()
        .await
        .context("loading default kubeconfig")?;

    // Honor TEND_WORKSPACE_CONFIG (chart sets it to /etc/tend/config.yaml,
    // mounted from the workspace ConfigMap). Falls back to load_config's
    // default ~/.config/tend/config.yaml for local CLI use.
    let tend_config = match std::env::var("TEND_WORKSPACE_CONFIG").ok() {
        Some(path) if !path.is_empty() => crate::load_config(Some(std::path::Path::new(&path)))
            .with_context(|| format!("loading tend workspace config from {path}"))?,
        _ => crate::load_config(None).context("loading tend workspace config")?,
    };

    let ctx = Arc::new(Context {
        client: client.clone(),
        tend_config: Arc::new(tend_config),
        http: reqwest::Client::builder()
            .user_agent("tend-operator")
            .build()
            .context("building http client")?,
        github_token: load_github_token(),
        repo_locks: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        // File-backed cache on the workspace PVC. Survives pod
        // restarts. Path is operator-tunable via TEND_HEAD_CACHE_PATH
        // (set by the chart); falls back to the default location
        // when unset for CLI / out-of-cluster use.
        head_cache: head_cache::open_at(std::path::PathBuf::from(
            std::env::var("TEND_HEAD_CACHE_PATH")
                .unwrap_or_else(|_| "/var/lib/tend-workspace/.tend-cache/head.json".into()),
        ))
        .await,
        // samba::LeakyBucket pacer honoring TEND_BUDGET_MAX_PER_HOUR —
        // chart-tunable via underTheRadar.budgetMaxPerHour values.
        budget: Arc::new(budget::pacer_from_env()),
    });

    // Prometheus metrics endpoint — vmagent scrapes via the chart's
    // ServiceMonitor (VMOperator converts SM → VMServiceScrape).
    let metrics_addr =
        std::env::var("TEND_METRICS_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
    metrics::spawn_server(&metrics_addr).context("spawning metrics server")?;

    // Fleet-wide org watcher — when TEND_FLEET_WATCH_ENABLED=true,
    // continuously enumerates the org, filters to flake-having repos,
    // and round-robin publishes refresh-jobs through the NATS throttle
    // at the throttle's drain rate (Mode A steady-trickle, fleet-wide).
    // Coexists with the FlakeUpdatePolicy reconciler — both publishers
    // share the same throttle worker; results are processed by their
    // respective subscribers.
    if let Some(watcher) = fleet_watch::FleetWatchTask::from_env()
        .await
        .context("initializing fleet watcher")?
    {
        tracing::info!("fleet watcher: starting");
        // Spawn the bridge consumer FIRST so it's listening before
        // the watcher starts emitting advances. Uses its own NATS
        // client connection.
        let nats_url = std::env::var("TEND_NATS_URL")
            .unwrap_or_else(|_| "nats://pleme-nats.nats.svc:4222".to_string());
        match async_nats::connect(&nats_url).await {
            Ok(advance_nats) => {
                let advance_kube = client.clone();
                tokio::spawn(async move {
                    fleet_advance_consumer::run(advance_nats, advance_kube).await;
                });
                tracing::info!("fleet advance consumer: spawned");
            }
            Err(e) => {
                tracing::warn!(error = %e, "fleet advance consumer: nats connect failed; advance bridge disabled");
            }
        }
        tokio::spawn(async move { watcher.run().await });
    }

    let policies: Api<crds::FlakeUpdatePolicy> = Api::all(client.clone());
    let proposals: Api<crds::FlakeUpdateProposal> = Api::all(client.clone());

    let policy_ctrl = Controller::new(policies.clone(), WatcherConfig::default())
        .owns(proposals.clone(), WatcherConfig::default())
        .run(
            reconcile::reconcile_policy,
            reconcile::policy_error_policy,
            ctx.clone(),
        )
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => tracing::debug!(?obj, "policy reconciled"),
                Err(e) => tracing::warn!(error = %e, "policy reconcile failed"),
            }
        });

    let proposal_ctrl = Controller::new(proposals, WatcherConfig::default())
        .run(
            reconcile::reconcile_proposal,
            reconcile::proposal_error_policy,
            ctx.clone(),
        )
        .for_each(|res| async move {
            match res {
                Ok((obj, _)) => tracing::debug!(?obj, "proposal reconciled"),
                Err(e) => tracing::warn!(error = %e, "proposal reconcile failed"),
            }
        });

    tokio::select! {
        _ = policy_ctrl => tracing::info!("policy controller exited"),
        _ = proposal_ctrl => tracing::info!("proposal controller exited"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
    }
    Ok(())
}

/// Delegates to `provider::github_token`, which absorbed this
/// function's env-plus-file resolution. Kept as a named local so the
/// operator's construction site reads the same as before.
fn load_github_token() -> Option<crate::secret::Secret> {
    crate::provider::github_token()
}
