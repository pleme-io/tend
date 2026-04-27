//! Status reconciliation primitives shared across controllers.
//!
//! kube-rs `Controller` watches the resource it owns, including status
//! subresource writes. Patching status on every reconcile cycle is a
//! self-trigger: each write increments `resourceVersion`, the watch
//! fires "object updated", reconcile runs again, status gets patched
//! again. The result is a tight loop that defeats `Action::requeue`.
//!
//! Two pieces fix it:
//!
//!   1. `stabilize_conditions` — K8s convention: `last_transition_time`
//!      marks when `status` flipped, not when the condition was last
//!      observed. Carrying over the prior timestamp when status didn't
//!      flip is what makes byte-equal status writes detectable.
//!
//!   2. The caller compares new vs. previous status (full struct equality)
//!      and skips the patch when equal. The requeue schedule still
//!      governs when we re-discover.
//!
//! Phase 2-4 controllers (Helm/Cargo/Image) inherit this unchanged —
//! the loop is structural to kube-rs, not specific to the flake domain.

use super::crds::Condition;

/// Carry over `last_transition_time` for any condition whose `status`
/// hasn't flipped. New conditions (no match by `type` in `previous`)
/// keep their freshly-set timestamp. Conditions where status flipped
/// also keep the new timestamp — the flip is the transition.
pub fn stabilize_conditions(new: &mut [Condition], previous: &[Condition]) {
    for n in new.iter_mut() {
        if let Some(prev) = previous.iter().find(|p| p.r#type == n.r#type) {
            if prev.status == n.status {
                n.last_transition_time = prev.last_transition_time;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn cond(t: &str, s: &str, ts: i64) -> Condition {
        Condition {
            r#type: t.into(),
            status: s.into(),
            reason: None,
            message: None,
            last_transition_time: Some(Utc.timestamp_opt(ts, 0).unwrap()),
        }
    }

    #[test]
    fn unchanged_status_carries_old_timestamp() {
        let prev = vec![cond("Reconciled", "True", 100)];
        let mut new = vec![cond("Reconciled", "True", 999)];
        stabilize_conditions(&mut new, &prev);
        assert_eq!(new[0].last_transition_time.unwrap().timestamp(), 100);
    }

    #[test]
    fn flipped_status_keeps_new_timestamp() {
        let prev = vec![cond("Reconciled", "True", 100)];
        let mut new = vec![cond("Reconciled", "False", 999)];
        stabilize_conditions(&mut new, &prev);
        assert_eq!(new[0].last_transition_time.unwrap().timestamp(), 999);
    }

    #[test]
    fn unmatched_type_keeps_new_timestamp() {
        let prev = vec![cond("Reconciled", "True", 100)];
        let mut new = vec![cond("Healthy", "True", 999)];
        stabilize_conditions(&mut new, &prev);
        assert_eq!(new[0].last_transition_time.unwrap().timestamp(), 999);
    }

    #[test]
    fn message_change_with_same_status_still_carries_old_time() {
        // K8s convention: timestamp marks status transitions, not message
        // edits. A condition stuck "False" with a different message gets
        // a new message but old time — so the loop-breaker only fires
        // when truly nothing changed (caller compares full struct).
        let mut prev = cond("Reconciled", "False", 100);
        prev.message = Some("rate limited".into());
        let mut new = cond("Reconciled", "False", 999);
        new.message = Some("rate limited (different reset)".into());
        stabilize_conditions(std::slice::from_mut(&mut new), &[prev]);
        assert_eq!(new.last_transition_time.unwrap().timestamp(), 100);
    }
}
