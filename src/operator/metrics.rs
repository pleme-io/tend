//! Prometheus metrics for the fleet update controller.
//!
//! Counters cover the lifecycle a proposal walks through, plus
//! per-gate verification outcomes — operators can answer "how many
//! advances are stuck in Verifying", "which gate fails most often",
//! "how many applies have landed cleanly" without grepping logs.
//!
//! Exposed at `:9090/metrics`. Scraped by vmagent via the chart's
//! ServiceMonitor (VMOperator's converter ownership picks up
//! prom-operator ServiceMonitors automatically).

use anyhow::Result;
use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use prometheus::{register_int_counter_vec, Encoder, IntCounterVec, Registry, TextEncoder};
use std::sync::OnceLock;
use tokio::net::TcpListener;

static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub struct OperatorMetrics {
    pub proposals_total: IntCounterVec,
    pub gates_total: IntCounterVec,
    pub applies_total: IntCounterVec,
    pub reconcile_errors_total: IntCounterVec,
}

static METRICS: OnceLock<OperatorMetrics> = OnceLock::new();

pub fn metrics() -> &'static OperatorMetrics {
    METRICS.get_or_init(|| {
        let proposals_total = register_int_counter_vec!(
            "tend_operator_proposals_total",
            "Total FlakeUpdateProposal phase transitions observed",
            &["phase"]
        )
        .expect("register proposals_total");

        let gates_total = register_int_counter_vec!(
            "tend_operator_gates_total",
            "Verification gate runs by name and outcome",
            &["gate", "outcome"]
        )
        .expect("register gates_total");

        let applies_total = register_int_counter_vec!(
            "tend_operator_applies_total",
            "Apply-path outcomes (writing a verified pin to flake.lock)",
            &["outcome"]
        )
        .expect("register applies_total");

        let reconcile_errors_total = register_int_counter_vec!(
            "tend_operator_reconcile_errors_total",
            "Reconcile errors by CRD kind",
            &["kind"]
        )
        .expect("register reconcile_errors_total");

        OperatorMetrics {
            proposals_total,
            gates_total,
            applies_total,
            reconcile_errors_total,
        }
    })
}

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| prometheus::default_registry().clone())
}

/// Spawn an HTTP listener serving `/metrics` in Prometheus text format.
/// Returns immediately; the listener task runs until the runtime
/// shuts down.
pub fn spawn_server(addr: &str) -> Result<()> {
    let _ = metrics(); // ensure counters registered before first scrape
    let bind: std::net::SocketAddr = addr.parse()?;
    tokio::spawn(async move {
        let listener = match TcpListener::bind(bind).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, addr = %bind, "metrics: bind failed");
                return;
            }
        };
        tracing::info!(addr = %bind, "metrics: listening");
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "metrics: accept failed");
                    continue;
                }
            };
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                if let Err(e) = http1::Builder::new()
                    .serve_connection(io, service_fn(serve))
                    .await
                {
                    tracing::debug!(error = %e, "metrics: connection ended");
                }
            });
        }
    });
    Ok(())
}

async fn serve(
    _req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let encoder = TextEncoder::new();
    let metric_families = registry().gather();
    let mut buf = Vec::new();
    encoder.encode(&metric_families, &mut buf).ok();
    Ok(Response::builder()
        .header("Content-Type", encoder.format_type())
        .body(Full::new(Bytes::from(buf)))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_init_and_increment() {
        let m = metrics();
        m.proposals_total.with_label_values(&["Pending"]).inc();
        m.gates_total
            .with_label_values(&["nix-flake-check", "passed"])
            .inc();
        m.applies_total.with_label_values(&["landed"]).inc();
        // Re-fetching the same counter handle returns the same singleton.
        assert!(
            metrics()
                .proposals_total
                .with_label_values(&["Pending"])
                .get()
                >= 1
        );
    }
}
