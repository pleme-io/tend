//! `TendDaemonState` — the aggregator the kanshou server exposes
//! for a running tend daemon (`tend daemon ...`).
//!
//! Tend was the canonical "what is it doing right now?" pain point —
//! `nix flake update` cycles ran in the background with no live
//! visibility into which workspace was being processed, which repo
//! was being pulled, when the next tick fires. Operators resorted to
//! pgrep + lsof archaeology.
//!
//! With this primitive, `gen kanshou query tend <field>` returns the
//! live counters: queued jobs, in-flight repo, last-tick timestamps,
//! per-workspace stats. Each leaf is one match-arm; extending the
//! query surface is mechanical.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kanshou::{Introspect, Query, QueryError, QueryResult};

/// Live aggregator. Reads atomics + accessor closures, hand-
/// implemented Introspect because the derive macro doesn't cover
/// atomics or `&self` method calls.
pub struct TendDaemonState {
    pub started_at_unix_ms: u64,
    pub last_tick_unix_ms: Arc<AtomicU64>,
    pub ticks_completed: Arc<AtomicU64>,
    pub repos_pulled: Arc<AtomicU64>,
    pub repos_fetched: Arc<AtomicU64>,
    pub pull_failures: Arc<AtomicU64>,
    /// Currently in-flight repo path (None when idle).
    pub current_repo: Arc<parking_lot::RwLock<Option<String>>>,
    /// Current workspace being processed (None when idle).
    pub current_workspace: Arc<parking_lot::RwLock<Option<String>>>,
}

impl Default for TendDaemonState {
    fn default() -> Self {
        Self::new()
    }
}

impl TendDaemonState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started_at_unix_ms: now_unix_ms(),
            last_tick_unix_ms: Arc::new(AtomicU64::new(0)),
            ticks_completed: Arc::new(AtomicU64::new(0)),
            repos_pulled: Arc::new(AtomicU64::new(0)),
            repos_fetched: Arc::new(AtomicU64::new(0)),
            pull_failures: Arc::new(AtomicU64::new(0)),
            current_repo: Arc::new(parking_lot::RwLock::new(None)),
            current_workspace: Arc::new(parking_lot::RwLock::new(None)),
        }
    }
}

impl Introspect for TendDaemonState {
    fn query(&self, q: &Query) -> QueryResult {
        let Some(first) = q.path.first().map(String::as_str) else {
            return Err(QueryError::unknown_field(String::new()));
        };
        let now = now_unix_ms();
        match first {
            "ticks" => {
                let last = self.last_tick_unix_ms.load(Ordering::Relaxed);
                let ms_since: serde_json::Value = if last == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(now.saturating_sub(last))
                };
                Ok(serde_json::json!({
                    "completed": self.ticks_completed.load(Ordering::Relaxed),
                    "last_tick_unix_ms": last,
                    "ms_since_last": ms_since,
                }))
            }
            "repos" => Ok(serde_json::json!({
                "pulled": self.repos_pulled.load(Ordering::Relaxed),
                "fetched": self.repos_fetched.load(Ordering::Relaxed),
                "pull_failures": self.pull_failures.load(Ordering::Relaxed),
            })),
            "current" => Ok(serde_json::json!({
                "repo": self.current_repo.read().as_deref(),
                "workspace": self.current_workspace.read().as_deref(),
            })),
            "process" => Ok(serde_json::json!({
                "pid": std::process::id(),
                "binary": std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "started_at_unix_ms": self.started_at_unix_ms,
                "uptime_ms": now.saturating_sub(self.started_at_unix_ms),
                "version": env!("CARGO_PKG_VERSION"),
            })),
            other => Err(QueryError::unknown_field(other.to_string())),
        }
    }

    fn schema(&self) -> &'static [&'static str] {
        &["ticks", "repos", "current", "process"]
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Spawn the kanshou server. Best-effort: bind failure is non-fatal.
/// Returns the path the server bound to.
pub fn spawn_server(
    app_name: &str,
    state: Arc<TendDaemonState>,
) -> std::io::Result<std::path::PathBuf> {
    let server = kanshou::Server::new(app_name, state)?;
    let socket_path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            tracing::warn!(error = ?e, "tend kanshou server exited with error");
        }
    });
    Ok(socket_path)
}
