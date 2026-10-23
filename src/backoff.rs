use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct FetchBackoffConfig {
    /// Skip the fetch of a remote that failed, for an exponentially growing
    /// delay with jitter, instead of retrying it every cycle. Off retries
    /// every cycle.
    pub enabled: bool,
    /// Delay after the first failure, seconds. Doubles per consecutive failure.
    pub base_secs: u64,
    /// Ceiling of the delay, seconds.
    pub max_secs: u64,
}

impl Default for FetchBackoffConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_secs: 300,
            max_secs: 21_600,
        }
    }
}

pub(crate) fn delay_secs(policy: &FetchBackoffConfig, failures: u32, unit: f64) -> u64 {
    if failures == 0 {
        return 0;
    }
    let exp = (failures - 1).min(32);
    let raw = policy
        .base_secs
        .saturating_mul(1u64 << exp)
        .min(policy.max_secs);
    let unit = unit.clamp(0.0, 1.0);
    let floor = raw / 2;
    (floor + ((raw - floor) as f64 * unit) as u64).min(policy.max_secs)
}

pub(crate) fn is_remote_failure(stderr: &str) -> bool {
    const MARKS: &[&str] = &[
        "Could not resolve hostname",
        "Could not read from remote repository",
        "Repository not found",
        "mux_client_request_session",
        "Connection timed out",
        "Connection refused",
        "Connection reset",
        "Connection closed",
        "unable to access",
        "Permission denied (publickey",
        "Network is unreachable",
        "Operation timed out",
    ];
    MARKS.iter().any(|m| stderr.contains(m))
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    retry_at_ms: u64,
}

#[derive(Debug, Default)]
pub(crate) struct BackoffTable {
    entries: HashMap<PathBuf, Entry>,
}

impl BackoffTable {
    pub(crate) fn waiting(&self, key: &Path, now_ms: u64) -> bool {
        self.entries
            .get(key)
            .is_some_and(|e| e.retry_at_ms > now_ms)
    }

    pub(crate) fn record_failure(
        &mut self,
        key: &Path,
        now_ms: u64,
        policy: &FetchBackoffConfig,
    ) -> u64 {
        let failures = self.entries.get(key).map_or(0, |e| e.failures) + 1;
        let delay = delay_secs(policy, failures, jitter_unit(key, failures));
        self.entries.insert(
            key.to_path_buf(),
            Entry {
                failures,
                retry_at_ms: now_ms.saturating_add(delay.saturating_mul(1000)),
            },
        );
        delay
    }

    pub(crate) fn record_success(&mut self, key: &Path) {
        self.entries.remove(key);
    }

    pub(crate) fn backed_off(&self, now_ms: u64) -> usize {
        self.entries
            .values()
            .filter(|e| e.retry_at_ms > now_ms)
            .count()
    }

    pub(crate) fn tracked(&self) -> usize {
        self.entries.len()
    }
}

fn jitter_unit(key: &Path, failures: u32) -> f64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    failures.hash(&mut h);
    now_ms().hash(&mut h);
    (h.finish() % 10_000) as f64 / 10_000.0
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[derive(Debug, Default)]
pub(crate) struct Backoff {
    policy: FetchBackoffConfig,
    table: BackoffTable,
}

impl Backoff {
    pub(crate) fn configure(&mut self, policy: FetchBackoffConfig) {
        self.policy = policy;
        if !policy.enabled {
            self.table = BackoffTable::default();
        }
    }

    pub(crate) fn should_skip(&self, key: &Path, now_ms: u64) -> bool {
        self.policy.enabled && self.table.waiting(key, now_ms)
    }

    pub(crate) fn note_fetch(&mut self, key: &Path, stderr: Option<&str>, now_ms: u64) {
        if !self.policy.enabled {
            return;
        }
        match stderr {
            None => self.table.record_success(key),
            Some(e) if is_remote_failure(e) => {
                let policy = self.policy;
                self.table.record_failure(key, now_ms, &policy);
            }
            Some(_) => {}
        }
    }
}

fn shared() -> &'static Mutex<Backoff> {
    static SHARED: OnceLock<Mutex<Backoff>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Backoff::default()))
}

pub(crate) fn configure(policy: FetchBackoffConfig) {
    shared().lock().configure(policy);
}

pub(crate) fn should_skip(key: &Path) -> bool {
    shared().lock().should_skip(key, now_ms())
}

pub(crate) fn note_fetch(key: &Path, stderr: Option<&str>) {
    shared().lock().note_fetch(key, stderr, now_ms());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackoffSnapshot {
    pub enabled: bool,
    pub backed_off: usize,
    pub tracked: usize,
}

pub fn snapshot() -> BackoffSnapshot {
    let s = shared().lock();
    BackoffSnapshot {
        enabled: s.policy.enabled,
        backed_off: s.table.backed_off(now_ms()),
        tracked: s.table.tracked(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> FetchBackoffConfig {
        FetchBackoffConfig {
            enabled: true,
            base_secs: 10,
            max_secs: 100,
        }
    }

    #[test]
    fn delay_doubles_then_caps() {
        let p = policy();
        assert_eq!(delay_secs(&p, 1, 1.0), 10);
        assert_eq!(delay_secs(&p, 2, 1.0), 20);
        assert_eq!(delay_secs(&p, 3, 1.0), 40);
        assert_eq!(delay_secs(&p, 4, 1.0), 80);
        assert_eq!(delay_secs(&p, 5, 1.0), 100);
        assert_eq!(delay_secs(&p, 4000, 1.0), 100);
    }

    #[test]
    fn jitter_stays_between_half_and_full() {
        let p = policy();
        assert_eq!(delay_secs(&p, 3, 0.0), 20);
        assert_eq!(delay_secs(&p, 3, 0.5), 30);
        assert_eq!(delay_secs(&p, 3, 1.0), 40);
        for f in 1..40 {
            for u in [0.0, 0.3, 0.99, 1.0, 7.0, -1.0] {
                assert!(delay_secs(&p, f, u) <= p.max_secs);
            }
        }
    }

    #[test]
    fn a_failed_remote_waits_then_a_success_clears_it() {
        let p = policy();
        let mut t = BackoffTable::default();
        let key = Path::new("/ws/repo");
        assert!(!t.waiting(key, 0));
        let delay = t.record_failure(key, 1_000, &p);
        assert!(delay >= 5 && delay <= 10);
        assert!(t.waiting(key, 1_000));
        assert_eq!(t.backed_off(1_000), 1);
        assert!(!t.waiting(key, 1_000 + delay * 1000));
        t.record_failure(key, 1_000, &p);
        t.record_success(key);
        assert!(!t.waiting(key, 1_000));
        assert_eq!(t.tracked(), 0);
    }

    #[test]
    fn only_remote_failures_are_classified() {
        assert!(is_remote_failure("ssh: Could not resolve hostname github.com"));
        assert!(is_remote_failure("ERROR: Repository not found."));
        assert!(is_remote_failure(
            "mux_client_request_session: session request failed"
        ));
        assert!(!is_remote_failure("fatal: cannot lock ref 'refs/remotes/x'"));
    }

    #[test]
    fn default_is_on() {
        assert!(FetchBackoffConfig::default().enabled);
        assert!(Backoff::default().policy.enabled);
    }

    #[test]
    fn turned_off_never_skips_and_forgets() {
        let key = Path::new("/ws/repo");
        let mut b = Backoff::default();
        b.note_fetch(key, Some("Could not resolve hostname x"), 0);
        assert!(b.should_skip(key, 1));
        b.configure(FetchBackoffConfig {
            enabled: false,
            ..FetchBackoffConfig::default()
        });
        b.note_fetch(key, Some("Could not resolve hostname x"), 0);
        assert!(!b.should_skip(key, 1));
        assert_eq!(b.table.tracked(), 0);
    }

    #[test]
    fn a_non_remote_failure_does_not_back_off() {
        let key = Path::new("/ws/repo");
        let mut b = Backoff::default();
        b.note_fetch(key, Some("fatal: cannot lock ref"), 0);
        assert!(!b.should_skip(key, 1));
    }
}
