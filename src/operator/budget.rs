//! Request budget — sliding-window rate limiter shared across every
//! domain controller (Phase 1 flake, Phase 2-4 helm/cargo/image).
//!
//! Why this exists: GitHub's abuse detector flags request patterns that
//! look like scrapers — sustained rate, cron-shaped cycles, bursts.
//! Our authenticated quota is 5000/hr but using even 10% of that
//! consistently is enough to trip behavioral detection. The budget
//! self-caps at a fraction of the theoretical limit and adapts down
//! when GitHub signals pressure via `X-RateLimit-Remaining`.
//!
//! `acquire()` is a coordination point: each upstream HTTP request
//! awaits the budget before firing. When the sliding window is full,
//! the call yields and re-checks once the oldest entry expires —
//! discovery slows down rather than racing to exhaustion. The cycle
//! is supposed to take whatever time GitHub gives us, not whatever
//! time the operator wants.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Maximum requests per hour we'll send to GitHub from one pod.
/// 100 = 2% of the 5000/hr authenticated quota — well below behavioral
/// flag thresholds and leaves headroom for human use of the same token.
const DEFAULT_MAX_PER_HOUR: u32 = 100;

/// Sliding-window rate limiter. Thread-safe; shared via Arc.
pub struct RequestBudget {
    max_per_hour: u32,
    /// Timestamps of past requests within the 1h window. Pruned on
    /// every acquire(). std::sync::Mutex chosen over tokio's because
    /// the protected operation is purely CPU-bound (vec push, vec
    /// retain) and finishes in microseconds; awaiting is for the
    /// outer "wait for slot" loop, not the inner critical section.
    window: Mutex<Vec<Instant>>,
    /// Backoff multiplier — 1× = full budget, 2×/4×/8× = halved each
    /// time GitHub signals pressure. Resets to 1× on a clean response.
    /// Atomic so observe_pressure() doesn't have to take the window mutex.
    backoff_factor: AtomicU32,
}

impl RequestBudget {
    #[must_use]
    pub fn new(max_per_hour: u32) -> Self {
        Self {
            max_per_hour,
            window: Mutex::new(Vec::with_capacity(max_per_hour as usize)),
            backoff_factor: AtomicU32::new(1),
        }
    }

    /// Default — 100 req/hr ceiling.
    #[must_use]
    pub fn default_for_github() -> Self {
        Self::new(DEFAULT_MAX_PER_HOUR)
    }

    /// Read `TEND_BUDGET_MAX_PER_HOUR` env var; fall back to the
    /// 100 req/hr default. Invalid/empty values silently use default
    /// so misconfiguration never wedges the operator.
    #[must_use]
    pub fn from_env_or_default() -> Self {
        let max = std::env::var("TEND_BUDGET_MAX_PER_HOUR")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_PER_HOUR);
        Self::new(max)
    }

    /// Reserve one request slot. Returns immediately when a slot is
    /// available; awaits otherwise until the oldest in-window entry
    /// ages out. Caller is responsible for actually sending the
    /// request after acquire returns — there's no "release" because
    /// the entry is committed at reservation time (matches GitHub's
    /// view: even cancelled requests count).
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let now = Instant::now();
                let mut window = self.window.lock().expect("budget mutex poisoned");
                window.retain(|t| now.duration_since(*t) < Duration::from_secs(3600));
                let factor = self.backoff_factor.load(Ordering::Relaxed).max(1);
                let effective_max = self.max_per_hour / factor;
                if (window.len() as u32) < effective_max {
                    window.push(now);
                    return;
                }
                // Compute time until the oldest entry ages out.
                let oldest = *window.iter().min().expect("window nonempty here");
                let elapsed = now.duration_since(oldest);
                Duration::from_secs(3600).saturating_sub(elapsed)
                    + Duration::from_secs(1)
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Update the backoff factor based on a freshly observed
    /// `X-RateLimit-Remaining` / `X-RateLimit-Limit` pair from GitHub.
    /// Backoff escalates as remaining drops; resets when the user has
    /// breathing room.
    pub fn observe_pressure(&self, remaining: u32, limit: u32) {
        let limit = limit.max(1);
        let pct = (remaining * 100) / limit;
        let factor = match pct {
            0..=9 => 8,
            10..=24 => 4,
            25..=49 => 2,
            _ => 1,
        };
        self.backoff_factor.store(factor, Ordering::Relaxed);
    }

    /// Current effective requests-per-hour cap, reflecting any active
    /// backoff. Useful for plan generation — if the cap is 25/hr we
    /// only schedule 25 checks for this cycle.
    #[must_use]
    pub fn effective_max_per_hour(&self) -> u32 {
        let factor = self.backoff_factor.load(Ordering::Relaxed).max(1);
        self.max_per_hour / factor
    }

    /// Snapshot of how many slots are still free in the current
    /// 1h sliding window. Useful for telemetry + plan budgeting.
    #[must_use]
    pub fn slots_remaining(&self) -> u32 {
        let now = Instant::now();
        let window = self.window.lock().expect("budget mutex poisoned");
        let active = window
            .iter()
            .filter(|t| now.duration_since(**t) < Duration::from_secs(3600))
            .count() as u32;
        self.effective_max_per_hour().saturating_sub(active)
    }
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

    #[tokio::test(flavor = "multi_thread")]
    async fn acquire_returns_immediately_under_budget() {
        let b = RequestBudget::new(5);
        for _ in 0..5 {
            // All five should resolve without blocking.
            tokio::time::timeout(Duration::from_millis(50), b.acquire())
                .await
                .expect("acquire should not block under budget");
        }
        assert_eq!(b.slots_remaining(), 0, "budget should be saturated");
    }

    #[test]
    fn observe_pressure_escalates_backoff() {
        let b = RequestBudget::new(100);
        assert_eq!(b.effective_max_per_hour(), 100);

        b.observe_pressure(60, 100); // 60% remaining → 1×
        assert_eq!(b.effective_max_per_hour(), 100);

        b.observe_pressure(40, 100); // 40% remaining → 2×
        assert_eq!(b.effective_max_per_hour(), 50);

        b.observe_pressure(20, 100); // 20% remaining → 4×
        assert_eq!(b.effective_max_per_hour(), 25);

        b.observe_pressure(5, 100); // 5% remaining → 8×
        assert_eq!(b.effective_max_per_hour(), 12);
    }

    #[test]
    fn observe_pressure_recovers_to_full_budget() {
        let b = RequestBudget::new(100);
        b.observe_pressure(5, 100);
        assert_eq!(b.effective_max_per_hour(), 12);
        b.observe_pressure(80, 100); // back to comfortable
        assert_eq!(b.effective_max_per_hour(), 100);
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
