//! `fleet_advance_consumer` — bridges fleet-wide advance detection
//! to the existing `FlakeUpdatePolicy` reconciler.
//!
//! Subscribes to `tend.fleet.advances.>` (where `fleet_watch`
//! publishes a `FleetAdvanceEvent` for every detected upstream
//! advance). For each event, annotates every enrolled
//! `FlakeUpdatePolicy` CR with a marker. The annotation change is
//! a watcher event that kube-rs's Controller picks up immediately,
//! firing `reconcile_policy` within seconds.
//!
//! Why annotate-trigger instead of direct proposal creation: the
//! reconciler's discovery flow is the canonical single source of
//! truth for "should this advance become a Proposal?". It already
//! handles the input-mode logic (Auto / Gated / Locked / Forbidden),
//! the lock-hash dedup, and the typed proposal CRD. Triggering it
//! reuses all of that — and the discovery now hits a hot ETag cache
//! (the throttle worker just observed the advance) so the trigger is
//! near-free in API quota.
//!
//! See pleme-io/theory/RATE-LIMITED-CONSUMERS.md §V (the bridge
//! pattern: continuous watcher → typed event → reactive reconciler).

use anyhow::{Context as _, Result};
use kube::{
    api::{Api, Patch, PatchParams},
    Client,
};
use serde_json::json;
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::crds::FlakeUpdatePolicy;
use super::fleet_watch::{FleetAdvanceEvent, ADVANCE_SUBJECT_PREFIX};

/// Annotation key set on every FlakeUpdatePolicy CR when an advance
/// arrives. Value = sanitized-key of the advanced repo. The change
/// triggers kube-rs's Controller; `reconcile_policy` fires within
/// the resync interval (typically <1s).
///
/// Using the SAME annotation key (overwriting on each advance)
/// guarantees the resourceVersion changes, which is what kube-rs
/// uses to detect updates. If we used per-advance keys, the CR would
/// grow unbounded.
const ADVANCE_TRIGGER_ANNOTATION: &str = "tend.pleme.io/last-fleet-advance";

/// Spawn the consumer task. Returns when the subscription closes
/// (typically: never, because async-nats reconnects on its own).
///
/// Listens on `tend.fleet.advances.>` with a wildcard subscription
/// — every advance any watcher emits anywhere in the cluster fans
/// out to all policies via kube annotations.
pub async fn run(nats: async_nats::Client, kube: Client) {
    let pattern = format!("{ADVANCE_SUBJECT_PREFIX}.>");
    let mut sub = match nats.subscribe(pattern.clone()).await {
        Ok(s) => s,
        Err(e) => {
            warn!(pattern = %pattern, error = %e, "fleet advance consumer: subscribe failed");
            return;
        }
    };
    info!(pattern = %pattern, "fleet advance consumer: subscribed");

    let kube = Arc::new(kube);

    use futures::StreamExt;
    while let Some(msg) = sub.next().await {
        let kube = kube.clone();
        // Spawn so a slow handler doesn't block the next event.
        tokio::spawn(async move {
            if let Err(e) = handle_advance(&kube, msg).await {
                warn!(error = %e, "fleet advance consumer: handle failed");
            }
        });
    }

    info!("fleet advance consumer: subscription closed");
}

async fn handle_advance(
    kube: &Client,
    msg: async_nats::Message,
) -> Result<()> {
    let key = msg
        .subject
        .as_str()
        .rsplit('.')
        .next()
        .unwrap_or("unknown")
        .to_string();
    let evt: FleetAdvanceEvent =
        serde_json::from_slice(&msg.payload).context("decode FleetAdvanceEvent")?;

    debug!(
        key = %key,
        repo = %evt.id,
        from = ?evt.from_rev,
        to = %evt.to_rev,
        "fleet advance consumer: received event"
    );

    // List every enrolled policy. Today there's typically one per
    // workspace; tomorrow there may be one per cluster pinning. We
    // annotate ALL of them — letting each reconciler decide whether
    // the advance is relevant to its specific flake.lock.
    let policies_all: Api<FlakeUpdatePolicy> = Api::all(kube.clone());
    let list = policies_all
        .list(&Default::default())
        .await
        .context("listing FlakeUpdatePolicy CRs")?;

    if list.items.is_empty() {
        debug!(key = %key, "fleet advance consumer: no policies enrolled, skipping");
        return Ok(());
    }

    let patch_value = json!({
        "metadata": {
            "annotations": {
                ADVANCE_TRIGGER_ANNOTATION: key,
                format!("{ADVANCE_TRIGGER_ANNOTATION}-rev"): evt.to_rev,
                format!("{ADVANCE_TRIGGER_ANNOTATION}-at"): evt.observed_at.to_rfc3339(),
            }
        }
    });
    let patch = Patch::Merge(&patch_value);
    let pp = PatchParams::apply("tend-fleet-advance-consumer").force();

    for policy in list.items {
        let name = policy.metadata.name.as_deref().unwrap_or("unnamed");
        let ns = policy
            .metadata
            .namespace
            .as_deref()
            .unwrap_or("default");
        let scoped: Api<FlakeUpdatePolicy> =
            Api::namespaced(kube.clone(), ns);
        match scoped.patch(name, &pp, &patch).await {
            Ok(_) => {
                info!(
                    policy = %name,
                    ns = %ns,
                    repo = %evt.id,
                    "fleet advance consumer: triggered policy reconcile"
                );
            }
            Err(e) => {
                warn!(
                    policy = %name,
                    ns = %ns,
                    error = %e,
                    "fleet advance consumer: patch failed"
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotation_key_stable() {
        // Catches accidental rename — downstream operators may grep
        // for this annotation to find which advance triggered them.
        assert_eq!(ADVANCE_TRIGGER_ANNOTATION, "tend.pleme.io/last-fleet-advance");
    }
}
