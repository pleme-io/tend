//! GitHub request pacing + requeue jitter.
//!
//! Why this exists: GitHub's abuse detector flags request patterns that
//! look like scrapers — sustained rate, cron-shaped cycles, bursts.
//! Our authenticated quota is 5000/hr but using even 10% of that
//! consistently is enough to trip behavioral detection. The pacer
//! self-caps at a fraction of the theoretical limit and adapts down
//! when GitHub signals pressure via `X-RateLimit-Remaining`.
//!
//! The actual pacing primitive is `samba::LeakyBucket` — the same
//! typed rate-limited-consumer primitive `tend throttle`'s worker uses
//! (`src/operator/throttle.rs`). Before 2026-07 this module also
//! defined a hand-rolled sliding-window limiter (`RequestBudget`) that
//! duplicated `LeakyBucket`'s job for the in-process discovery path
//! while the NATS-relayed throttle path already used samba. Both paths
//! now share one canonical primitive — see
//! pleme-io/theory/RATE-LIMITED-CONSUMERS.md.

use std::time::Duration;

/// Maximum requests per hour we'll send to GitHub from one pod.
/// 100 = 2% of the 5000/hr authenticated quota — well below behavioral
/// flag thresholds and leaves headroom for human use of the same token.
const DEFAULT_MAX_PER_HOUR: u32 = 100;

/// Headroom % below which the bucket halves — reproduces the old
/// `RequestBudget::observe_pressure` cutoff (25–49% remaining → 2×
/// backoff, i.e. a 0.5 pace multiplier).
const PRESSURE_WARN_PCT: u8 = 50;

/// Headroom % below which the bucket quarters — reproduces the old
/// cutoff (10–24% remaining → 4× backoff, i.e. a 0.25 pace multiplier).
/// samba's `PressureLevel::Emergency` tier (<10% → 0.125×, matching the
/// old 8× backoff) is fixed at 10% internally, not configurable here.
const PRESSURE_CRITICAL_PCT: u8 = 25;

/// Construct a `samba::LeakyBucket` capped at `max_per_hour` requests/hour,
/// with a full burst ceiling (so — like the old sliding-window budget —
/// up to `max_per_hour` requests can go out immediately after an idle
/// period, then the bucket paces the rest) and zero jitter (the old
/// budget's `acquire()` had no randomized wait; jitter for *requeue*
/// timing is handled separately by `jittered_from_env` below).
///
/// # Panics
/// Never, in practice — every argument is a literal or a `max(1)`-guarded
/// value, so `LeakyBucket::new`'s validation can't fail here.
#[must_use]
pub fn pacer(max_per_hour: u32) -> samba::LeakyBucket {
    let capped = max_per_hour.max(1);
    samba::LeakyBucket::new(
        1.0,               // quota_pct: capped is already the absolute req/hr cap
        f64::from(capped), // initial_rph
        PRESSURE_WARN_PCT,
        PRESSURE_CRITICAL_PCT,
        0.0,    // jitter_pct
        capped, // burst — allow an immediate burst up to the cap
    )
    .expect("pacer: literal LeakyBucket params are always valid")
}

/// Read `TEND_BUDGET_MAX_PER_HOUR` env var; fall back to the 100 req/hr
/// default. Invalid/empty values silently use default so misconfiguration
/// never wedges the operator. Chart-tunable via
/// `underTheRadar.budgetMaxPerHour`.
#[must_use]
pub fn pacer_from_env() -> samba::LeakyBucket {
    let max = std::env::var("TEND_BUDGET_MAX_PER_HOUR")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_PER_HOUR);
    pacer(max)
}

/// Multiply a base requeue duration by a random factor in
/// [1.0, 1.0 + max_jitter_pct]. Spreads cycle starts so multiple pods
/// (and the policy/proposal reconcilers within one pod) don't
/// synchronize into a tight cron-shaped pattern.
#[must_use]
pub fn jittered(base: Duration, max_jitter_pct: f32) -> Duration {
    use rand::Rng;
    let jitter = rand::thread_rng().gen_range(0.0..=max_jitter_pct.max(0.0));
    base.mul_f32(1.0 + jitter)
}

/// Convenience: read the jitter pct from env (TEND_REQUEUE_JITTER_PCT,
/// defaults to 0.30) and apply it to `base`. Operators tune via Helm
/// values without code changes.
#[must_use]
pub fn jittered_from_env(base: Duration) -> Duration {
    let pct = std::env::var("TEND_REQUEUE_JITTER_PCT")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
        .map(|f| f.clamp(0.0, 1.0))
        .unwrap_or(0.30);
    jittered(base, pct)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the main path is genuinely paced through samba's
    /// `LeakyBucket`, not just "it compiles the same as before": a
    /// 5-request/hour pacer admits the first 5 acquires immediately
    /// (burst ceiling) and measurably delays the 6th.
    #[tokio::test(flavor = "multi_thread")]
    async fn pacer_admits_burst_then_paces_via_samba_leaky_bucket() {
        let bucket = pacer(5);
        for _ in 0..5 {
            tokio::time::timeout(Duration::from_millis(200), bucket.acquire())
                .await
                .expect("first 5 acquires should not block (burst ceiling)");
        }
        // 5/hr → one token every 720s; the 6th acquire must wait, proving
        // requests genuinely flow through the LeakyBucket's pacing math
        // and not some no-op stand-in.
        let wait = tokio::time::timeout(Duration::from_millis(200), bucket.acquire()).await;
        assert!(
            wait.is_err(),
            "6th acquire on a 5/hr bucket should still be waiting after 200ms"
        );
    }

    #[tokio::test]
    async fn pacer_pressure_escalates_on_low_headroom() {
        let bucket = pacer(100);
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Healthy);

        bucket.record_headroom(60, 100).await; // 60% remaining → Healthy
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Healthy);

        bucket.record_headroom(40, 100).await; // 40% remaining → Warn (0.5×)
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Warn);

        bucket.record_headroom(20, 100).await; // 20% remaining → Critical (0.25×)
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Critical);

        bucket.record_headroom(5, 100).await; // 5% remaining → Emergency (0.125×)
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Emergency);
    }

    #[tokio::test]
    async fn pacer_recovers_to_healthy_pressure() {
        let bucket = pacer(100);
        bucket.record_headroom(5, 100).await;
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Emergency);
        bucket.record_headroom(80, 100).await; // back to comfortable
        assert_eq!(bucket.pressure().await, samba::PressureLevel::Healthy);
    }

    #[tokio::test]
    async fn pacer_tracks_dynamically_observed_limit() {
        // Starts at the constructed cap...
        let bucket = pacer(100);
        assert!((bucket.target_rpm().await - 100.0 / 60.0).abs() < 0.01);
        // ...and re-rates when GitHub reports a different ceiling (e.g.
        // a GitHub App token's 15000/hr instead of a PAT's 5000/hr) —
        // a capability `RequestBudget` never had (its max_per_hour was
        // fixed at construction).
        bucket.record_observed_limit(15_000).await;
        assert!((bucket.target_rpm().await - 15_000.0 / 60.0).abs() < 0.01);
    }

    #[test]
    fn jittered_stays_within_bounds() {
        let base = Duration::from_secs(300);
        for _ in 0..100 {
            let j = jittered(base, 0.3);
            assert!(
                j >= base && j <= base.mul_f32(1.3),
                "jittered {j:?} outside [base, base*1.3]"
            );
        }
    }

    #[test]
    fn jittered_zero_pct_returns_base() {
        let base = Duration::from_secs(60);
        assert_eq!(jittered(base, 0.0), base);
    }
}
