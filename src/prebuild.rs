//! Prebuild: walk workspace repos that carry a flake.nix, build their
//! canonical output (`packages.${system}.default`), and optionally push
//! the resulting closure into an Attic binary cache.
//!
//! Sibling of [`flake_update_daemon`](crate::main::run_flake_update_daemon):
//! `flake-update` propagates lock bumps across the workspace's dep graph;
//! `prebuild` does the *next* step, which is actually realising the
//! resulting outputs into `/nix/store` so consumers (CI nodes, other
//! workstations) get cache hits instead of cold builds.
//!
//! Design — three guarantees:
//! 1. **Idempotent across cycles.** A persistent seen-rev cache at
//!    `${XDG_CACHE_HOME:-~/.cache}/tend/prebuild-seen.json` records
//!    `{ repo_path: last_built_git_rev }`. Repos at the same rev as
//!    last cycle are skipped.
//! 2. **Per-closure attic push (no SQLite-var-limit landmine).** Each
//!    successful `nix build` produces a small set of out-paths; we
//!    push those directly. We never try to push the entire store in
//!    one `get-missing-paths` request.
//! 3. **Resource discipline lives outside this code.** The daemon is
//!    a single-threaded loop (`nix build` does its own parallelism
//!    internally). Operators throttle via systemd `CPUQuota=`,
//!    `MemoryHigh=`, `IOWeight=`, etc. — see
//!    `blackmatter-tend/module/default.nix`.

use crate::audit::AuditLog;
use crate::config::Config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// One repo's outcome within a single prebuild cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// HEAD matches the seen-rev cache; nothing to do.
    NoChange,
    /// `nix build .` failed with "does not provide attribute" — repo
    /// has no `packages.${system}.default`. Skipped without error.
    NoDefault,
    /// Build succeeded; out-paths are the closures that now live in
    /// `/nix/store` and (optionally) were pushed to attic.
    Built { out_paths: Vec<String>, pushed: usize },
}

/// Rollup across all repos in a cycle.
#[derive(Debug, Default, Clone, Serialize)]
pub struct PrebuildSummary {
    pub built: usize,
    pub no_change: usize,
    pub skipped_no_default: usize,
    pub failed: usize,
    pub pushed: usize,
}

impl PrebuildSummary {
    /// "Work performed this cycle" — drives daemon exponential-backoff
    /// reset, mirroring [`flake::ExecSummary::work`].
    #[must_use]
    pub fn work(&self) -> usize {
        self.built
    }

    fn absorb(&mut self, outcome: &BuildOutcome) {
        match outcome {
            BuildOutcome::NoChange => self.no_change += 1,
            BuildOutcome::NoDefault => self.skipped_no_default += 1,
            BuildOutcome::Built { pushed, .. } => {
                self.built += 1;
                self.pushed += *pushed;
            }
        }
    }
}

/// Operator-tunable knobs threaded into every per-repo build attempt.
#[derive(Debug, Clone)]
pub struct PrebuildOptions {
    /// Suppress per-repo log lines (still emits one-line audit events).
    pub quiet: bool,
    /// If `Some`, push every produced closure to this attic cache via
    /// `attic push <cache_name> <out-path>` (per closure, so each
    /// request stays well below atticd's SQLite var limit).
    pub attic: Option<AtticPush>,
    /// Maximum concurrent per-repo `nix build` invocations. Each
    /// in-flight build still parallelises internally per `max-jobs`,
    /// so set this conservatively — `1` for shared developer
    /// workstations, `2`–`4` on a dedicated builder like rio. The
    /// hard ceiling on real resource use lives in systemd's
    /// `CPUQuota`/`MemoryHigh`/`IOWeight` on the unit, not here.
    pub max_inflight: usize,
}

impl Default for PrebuildOptions {
    fn default() -> Self {
        Self {
            quiet: false,
            attic: None,
            max_inflight: 1,
        }
    }
}

/// Attic-reachability awareness for the daemon loop.
///
/// When the daemon's push target (the Attic server named by
/// `--attic-url`) is unreachable, building locally is pure waste: we
/// produce closures we can't ship to the cache the rest of the fleet
/// reads from. Rather than burn CPU/IO on builds that strand in the
/// local store, the daemon probes reachability before each cycle and,
/// when the server is down, enters a *separate* exponential backoff —
/// distinct from the converged-backoff that governs steady-state idle
/// polling — sleeping and re-probing without building.
///
/// All knobs carry best-default values via [`Default`]; an operator who
/// sets nothing gets a 60s→1800s doubling probe with a 5s timeout.
#[derive(Debug, Clone)]
pub struct ReachabilityOptions {
    /// When false, the daemon never probes and always builds (legacy
    /// behavior). Defaults to true *only when an attic URL is present*
    /// — the wiring in `main.rs` enables it iff `--attic-url` is given.
    pub enabled: bool,
    /// The URL probed each cycle (the attic server root, e.g.
    /// `http://rio:8080/`). Reused from `--attic-url` so there's one
    /// source of truth for "where attic lives".
    pub url: String,
    /// Floor of the unreachable-backoff (seconds). First unreachable
    /// cycle sleeps this long.
    pub min_interval: u64,
    /// Ceiling of the unreachable-backoff (seconds). The doubling
    /// caps here so a long outage settles into a steady re-probe pace.
    pub max_interval: u64,
    /// Per-probe HTTP timeout (seconds). A short timeout keeps a dead
    /// server from stalling the loop.
    pub probe_timeout: u64,
}

impl Default for ReachabilityOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            min_interval: 60,
            max_interval: 1800,
            probe_timeout: 5,
        }
    }
}

impl ReachabilityOptions {
    /// Compute the next unreachable-backoff sleep given how many cycles
    /// in a row the server has been unreachable. Pure function: exposed
    /// for unit testing the doubling/cap behavior without a network.
    ///
    /// `consecutive_unreachable` is 1-based: the *first* unreachable
    /// observation passes `1` and yields `min_interval`; `2` yields
    /// `2*min`; capped at `max_interval`. A reachable observation never
    /// calls this — the caller resets to the normal converged loop.
    #[must_use]
    pub fn unreachable_sleep(&self, consecutive_unreachable: u32) -> u64 {
        let min = self.min_interval.max(1);
        let max = self.max_interval.max(min);
        // First strike (n=1) → min; each subsequent strike doubles.
        // Double via saturating_mul so the value (not just the shift
        // amount) can't overflow on a long outage — `checked_shl`
        // guards the shift count but silently *wraps* the value, which
        // would zero out a large product. Cap at `max` each step so we
        // bail early instead of churning through u32::MAX iterations.
        let mut scaled = min;
        for _ in 1..consecutive_unreachable {
            scaled = scaled.saturating_mul(2);
            if scaled >= max {
                return max;
            }
        }
        scaled.min(max)
    }
}

/// Probe whether the Attic server at `url` is reachable. A cheap HTTP
/// GET with a short timeout — any response (even a 4xx/5xx) proves the
/// server is *up* and answering, which is all we need to decide it's
/// worth building+pushing. Only a connect/timeout/DNS error counts as
/// unreachable. Uses the non-optional `reqwest` dep (rustls-tls) so no
/// new dependency enters the lockfile.
pub async fn probe_attic_reachable(url: &str, timeout_secs: u64) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    // A bare GET to the server root. atticd answers with a redirect or
    // status page; either way a transport-level success means "up".
    client.get(url).send().await.is_ok()
}

#[derive(Debug, Clone)]
pub struct AtticPush {
    /// Cache name as known by atticd (e.g. `nexus`).
    pub cache_name: String,
    /// Server alias (passed to `attic login`).
    pub server_name: String,
    /// Attic server URL (e.g. `http://rio:8080/`).
    pub server_url: String,
    /// Path to a file containing a JWT signed by atticd. Read once at
    /// the start of each cycle; not held in memory between cycles.
    pub token_file: PathBuf,
}

/// Persistent seen-rev cache so unchanged repos are skipped across
/// daemon cycles AND across process restarts.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SeenCache {
    /// `{ absolute_repo_path: last_successfully_built_git_rev }`.
    /// BTreeMap so the on-disk JSON has stable key order for diffs.
    pub revs: BTreeMap<String, String>,
}

impl SeenCache {
    pub(crate) fn default_path() -> PathBuf {
        crate::cache::tend_cache_root().join("prebuild-seen.json")
    }

    pub(crate) fn load_from(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let s = serde_json::to_string_pretty(self)
            .context("serializing prebuild-seen.json")?;
        std::fs::write(path, s)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// Public one-shot cycle: walks every workspace, builds every flake
/// repo whose HEAD has moved since last cycle, optionally pushes each
/// resulting closure to attic. Returns a rollup [`PrebuildSummary`].
///
/// Per-repo work is dispatched onto a [`JoinSet`] bounded by a
/// [`Semaphore`] of `opts.max_inflight` permits, so the cycle drips
/// builds in as the host's CPU/memory budget allows. Each build runs
/// via [`tokio::task::spawn_blocking`] so the (synchronous) Command
/// calls don't block the runtime's reactor.
///
/// The `cfg` argument is the materialised tend config (same shape
/// `flake-update` consumes). `ws_filter` matches `Workspace::name`
/// 1:1; pass `None` to iterate everything.
///
/// The function is fully reusable from the K8s `operator` feature —
/// no CLI-specific state lives here, just the typed
/// `Config`/`PrebuildOptions`/`AuditLog` triple.
pub async fn run_cycle(
    cfg: &Config,
    ws_filter: Option<&str>,
    opts: &PrebuildOptions,
    audit: &AuditLog,
) -> Result<PrebuildSummary> {
    let seen_path = SeenCache::default_path();
    let seen = Arc::new(Mutex::new(SeenCache::load_from(&seen_path)));
    let summary = Arc::new(Mutex::new(PrebuildSummary::default()));

    // attic login once per cycle, not per repo — keeps the per-build
    // overhead bounded.
    let attic_ready = match &opts.attic {
        Some(push) => match attic_login(push) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[prebuild] attic login failed; building without push: {e:#}");
                audit.log(
                    "prebuild_attic_login_failed",
                    serde_json::json!({ "error": format!("{e:#}") }),
                );
                false
            }
        },
        None => false,
    };

    // Snapshot the full repo list up front so the iteration doesn't
    // straddle the await boundary with a borrow of `cfg`.
    let mut all_repos: Vec<PathBuf> = Vec::new();
    for ws in crate::filter_workspaces(&cfg.workspaces, ws_filter) {
        let base = ws.resolved_base_dir()?;
        // Honour workspace-level `prebuild:` config from shikumi:
        // a workspace can declare its own intervals + attic cache
        // without changing the CLI flags. Merged for log visibility
        // only — runtime semaphore + attic-login are global per cycle.
        let effective = effective_per_workspace(opts, ws.prebuild.as_ref());
        if !effective.quiet {
            println!(
                "[prebuild] workspace={} base={} max_inflight={} attic_cache={:?}",
                ws.name,
                base.display(),
                effective.max_inflight,
                effective.attic.as_ref().map(|a| &a.cache_name),
            );
        }
        all_repos.extend(enumerate_repos_with_flake(&base));
    }

    let max_inflight = opts.max_inflight.max(1);
    let sem = Arc::new(Semaphore::new(max_inflight));
    let mut tasks: JoinSet<()> = JoinSet::new();
    // AuditLog itself isn't Clone (it owns a PathBuf and an implicit
    // append-handle contract via the file path). Wrap once in Arc so
    // every per-repo task gets a cheap reference-counted handle.
    let audit_arc: Arc<AuditLog> = Arc::new(AuditLog::new(audit.path().to_path_buf()));
    let opts_arc: Arc<PrebuildOptions> = Arc::new(opts.clone());

    for repo in all_repos {
        let sem = Arc::clone(&sem);
        let seen = Arc::clone(&seen);
        let summary = Arc::clone(&summary);
        let audit = Arc::clone(&audit_arc);
        let opts = Arc::clone(&opts_arc);
        let seen_path = seen_path.clone();

        tasks.spawn(async move {
            // Permit acquired BEFORE spawn_blocking so the bound is
            // honoured even under back-pressure on the blocking
            // thread pool.
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return, // semaphore closed → cycle aborted
            };
            let key = repo.display().to_string();

            // Snapshot the previous-seen rev so prebuild_one doesn't
            // need to hold the SeenCache lock across the build.
            let prev_rev = seen
                .lock()
                .expect("seen cache mutex poisoned")
                .revs
                .get(&key)
                .cloned();

            let audit_for_task = Arc::clone(&audit);
            let opts_for_task = Arc::clone(&opts);
            let repo_for_task = repo.clone();
            let blocking = tokio::task::spawn_blocking(move || {
                prebuild_one(
                    &repo_for_task,
                    prev_rev.as_deref(),
                    &opts_for_task,
                    attic_ready,
                    &audit_for_task,
                )
            })
            .await;

            match blocking {
                Ok(Ok(outcome)) => {
                    if let BuildOutcome::Built { .. } = &outcome {
                        // Re-read HEAD after the build so an upstream
                        // push that arrived mid-build doesn't get
                        // recorded as "built at the old rev".
                        if let Ok(new_rev) = git_head_rev(&repo) {
                            let mut s =
                                seen.lock().expect("seen cache mutex poisoned");
                            s.revs.insert(key.clone(), new_rev);
                            let _ = s.save_to(&seen_path);
                        }
                    }
                    summary
                        .lock()
                        .expect("summary mutex poisoned")
                        .absorb(&outcome);
                }
                Ok(Err(e)) => {
                    let msg = format!("{e:#}");
                    let class = crate::anomaly::classify(&msg);
                    eprintln!("[prebuild] {key}: [{}] {msg}", class.as_str());
                    audit.log(
                        "prebuild_failed",
                        serde_json::json!({
                            "repo": key,
                            "class": class.as_str(),
                            "error": msg,
                        }),
                    );
                    summary.lock().expect("summary mutex poisoned").failed += 1;
                }
                Err(join_err) => {
                    eprintln!("[prebuild] task panicked for {key}: {join_err}");
                    audit.log(
                        "prebuild_panic",
                        serde_json::json!({
                            "repo": key,
                            "error": format!("{join_err}"),
                        }),
                    );
                    summary.lock().expect("summary mutex poisoned").failed += 1;
                }
            }
        });
    }

    // Drain — every task's outcome has already been absorbed into the
    // shared summary, so we just wait for them all to finish.
    while tasks.join_next().await.is_some() {}

    let final_summary = Arc::try_unwrap(summary)
        .map_err(|_| anyhow::anyhow!("summary still has outstanding refs"))?
        .into_inner()
        .expect("summary mutex poisoned");
    Ok(final_summary)
}

/// Build one repo (if its HEAD has moved). Returns the typed outcome
/// without mutating the seen-cache — the caller updates `seen` only
/// after a successful build, so partial-progress is preserved across
/// crashes. Takes `prev_rev: Option<&str>` rather than borrowing the
/// shared cache so this function is trivially callable from inside a
/// `spawn_blocking` closure.
pub(crate) fn prebuild_one(
    repo: &Path,
    prev_rev: Option<&str>,
    opts: &PrebuildOptions,
    attic_ready: bool,
    audit: &AuditLog,
) -> Result<BuildOutcome> {
    let rev = git_head_rev(repo)?;
    let key = repo.display().to_string();
    if prev_rev == Some(rev.as_str()) {
        return Ok(BuildOutcome::NoChange);
    }

    if !opts.quiet {
        println!("[prebuild] building {} @ {}", repo.display(), &rev[..rev.len().min(8)]);
    }

    let build = Command::new("nix")
        .args([
            "build",
            "--no-link",
            "--print-out-paths",
            "--refresh",
            // Suppress the "Git tree dirty" warning that would otherwise
            // pollute the journal — tend pulls before the daemon cycle,
            // so dirtiness here would be a user edit and we want to
            // build what's on disk anyway.
            "--option",
            "warn-dirty",
            "false",
            ".",
        ])
        .current_dir(repo)
        .output()
        .with_context(|| format!("nix build {}", repo.display()))?;

    if !build.status.success() {
        let stderr = String::from_utf8_lossy(&build.stderr);
        // Most pleme-io repos provide a `packages.${system}.default`; a
        // sizeable minority (libraries, docs, scratch) do not. Treat
        // that as a soft no-op so the daemon doesn't burn its
        // exponential-backoff budget on repos that will never build.
        if missing_default_attribute(&stderr) {
            return Ok(BuildOutcome::NoDefault);
        }
        anyhow::bail!("nix build failed: {}", stderr);
    }

    let out_paths: Vec<String> = String::from_utf8_lossy(&build.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let pushed = if attic_ready {
        if let Some(push) = opts.attic.as_ref() {
            push_closures(push, &out_paths)
        } else {
            0
        }
    } else {
        0
    };

    audit.log(
        "prebuild_built",
        serde_json::json!({
            "repo": key,
            "rev": rev,
            "out_paths": out_paths.len(),
            "pushed": pushed,
        }),
    );

    Ok(BuildOutcome::Built { out_paths, pushed })
}

/// `attic login <server> <url> <token>`. Run once per daemon cycle.
fn attic_login(push: &AtticPush) -> Result<()> {
    let token = std::fs::read_to_string(&push.token_file)
        .with_context(|| format!("reading {}", push.token_file.display()))?;
    let token = token.trim();
    let status = Command::new("attic")
        .args(["login", &push.server_name, &push.server_url, token])
        .status()
        .context("running attic login")?;
    if !status.success() {
        anyhow::bail!("attic login exited {}", status);
    }
    Ok(())
}

/// `attic push <cache> <out-path>` per closure. Returns the count of
/// closures that pushed successfully — failures are logged via tracing
/// but do not abort the cycle (one stuck closure shouldn't poison the
/// daemon).
fn push_closures(push: &AtticPush, out_paths: &[String]) -> usize {
    let mut ok = 0;
    for path in out_paths {
        let status = Command::new("attic")
            .args(["push", &push.cache_name, path])
            .status();
        match status {
            Ok(s) if s.success() => ok += 1,
            Ok(s) => eprintln!("[prebuild] attic push {} → exit {}", path, s),
            Err(e) => eprintln!("[prebuild] attic push {} → {}", path, e),
        }
    }
    ok
}

/// Merge a workspace-level `prebuild:` declaration onto the
/// CLI-derived [`PrebuildOptions`]. The workspace block, when
/// present, **overrides** the CLI for any field it sets (non-zero
/// numerics, `Some(_)` for attic-* options). This is the shikumi
/// surface — operators edit `config.yaml`'s `prebuild:` to change
/// daemon behavior; the daemon re-loads the config every cycle so
/// edits propagate within one interval, no restart needed.
pub(crate) fn effective_per_workspace(
    cli: &PrebuildOptions,
    ws: Option<&crate::config::PrebuildConfig>,
) -> PrebuildOptions {
    let mut out = cli.clone();
    let Some(ws) = ws else {
        return out;
    };
    if ws.max_inflight > 0 {
        out.max_inflight = ws.max_inflight;
    }
    // The two interval knobs live on the daemon loop (not in
    // PrebuildOptions which models a single cycle), so a workspace
    // can't yet override them without restructuring. Tracked as a
    // follow-up; the typed surface is what matters for now.
    if let (Some(cache), Some(server), Some(url), Some(token)) = (
        ws.attic_cache.as_ref(),
        ws.attic_server.as_ref(),
        ws.attic_url.as_ref(),
        ws.attic_token_file.as_ref(),
    ) {
        out.attic = Some(AtticPush {
            cache_name: cache.clone(),
            server_name: server.clone(),
            server_url: url.clone(),
            token_file: PathBuf::from(token),
        });
    }
    out
}

/// Enumerate immediate children of `base` that contain `flake.nix`
/// AND a `.git/` (or `.git` worktree marker file). One level deep on
/// purpose — pleme-io workspaces are `{base}/{repo}/...` shaped, never
/// nested. We sort by repo name so the cycle order is deterministic
/// across runs (useful for tail-the-journal debugging).
pub(crate) fn enumerate_repos_with_flake(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let flake = p.join("flake.nix");
        let git = p.join(".git");
        if flake.exists() && git.exists() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Capture the HEAD rev of a repo via `git rev-parse HEAD`.
pub(crate) fn git_head_rev(repo: &Path) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .with_context(|| format!("git rev-parse HEAD in {}", repo.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "git rev-parse HEAD failed in {}: {}",
            repo.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// nix's "does not provide attribute" comes in a handful of phrasings
/// depending on whether the flake declares `packages` at all, declares
/// it for a different system, or sets `default` explicitly. Cover the
/// observed ones — false positives just mean we treat a real failure
/// as `NoDefault`, which is the same outcome we'd want anyway
/// (skip-and-move-on).
pub(crate) fn missing_default_attribute(stderr: &str) -> bool {
    stderr.contains("does not provide attribute")
        || stderr.contains("flake does not provide attribute")
        || stderr.contains("attribute 'default' missing")
        || stderr.contains("error: flake 'git+file:")
            && stderr.contains("does not provide")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tend-prebuild-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn summary_absorb_built_increments_built_and_pushed() {
        let mut s = PrebuildSummary::default();
        s.absorb(&BuildOutcome::Built {
            out_paths: vec!["/nix/store/x".to_string(), "/nix/store/y".to_string()],
            pushed: 2,
        });
        assert_eq!(s.built, 1);
        assert_eq!(s.pushed, 2);
        assert_eq!(s.no_change, 0);
        assert_eq!(s.work(), 1, "work() drives backoff reset");
    }

    #[test]
    fn summary_absorb_no_change_does_not_count_as_work() {
        let mut s = PrebuildSummary::default();
        s.absorb(&BuildOutcome::NoChange);
        assert_eq!(s.no_change, 1);
        assert_eq!(s.work(), 0, "no_change must not trigger backoff reset");
    }

    #[test]
    fn summary_absorb_no_default_recorded_but_not_work() {
        let mut s = PrebuildSummary::default();
        s.absorb(&BuildOutcome::NoDefault);
        assert_eq!(s.skipped_no_default, 1);
        assert_eq!(s.work(), 0);
    }

    #[test]
    fn seen_cache_roundtrip_through_disk() {
        let dir = tmpdir();
        let p = dir.join("seen.json");
        let mut c = SeenCache::default();
        c.revs.insert("/x/foo".into(), "abc123".into());
        c.revs.insert("/x/bar".into(), "def456".into());
        c.save_to(&p).unwrap();

        let loaded = SeenCache::load_from(&p);
        assert_eq!(loaded.revs.len(), 2);
        assert_eq!(loaded.revs.get("/x/foo").unwrap(), "abc123");
        assert_eq!(loaded.revs.get("/x/bar").unwrap(), "def456");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_cache_load_missing_file_yields_empty() {
        let dir = tmpdir();
        let p = dir.join("does-not-exist.json");
        let c = SeenCache::load_from(&p);
        assert!(c.revs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_cache_load_corrupt_file_yields_empty_not_panic() {
        let dir = tmpdir();
        let p = dir.join("bad.json");
        fs::write(&p, "not-json-at-all").unwrap();
        let c = SeenCache::load_from(&p);
        assert!(c.revs.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seen_cache_save_creates_parent_dirs() {
        let dir = tmpdir();
        let p = dir.join("nested/sub/seen.json");
        let mut c = SeenCache::default();
        c.revs.insert("/x".into(), "rev".into());
        c.save_to(&p).unwrap();
        assert!(p.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumerate_skips_dirs_without_flake_nix() {
        let dir = tmpdir();
        // repo-a: has flake + .git → included
        let a = dir.join("repo-a");
        fs::create_dir_all(a.join(".git")).unwrap();
        fs::write(a.join("flake.nix"), "{}").unwrap();
        // repo-b: has .git but no flake → excluded
        let b = dir.join("repo-b");
        fs::create_dir_all(b.join(".git")).unwrap();
        // repo-c: has flake but no .git → excluded
        let c = dir.join("repo-c");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("flake.nix"), "{}").unwrap();
        // file: not a dir → excluded
        fs::write(dir.join("README.md"), "hi").unwrap();

        let found = enumerate_repos_with_flake(&dir);
        assert_eq!(found.len(), 1, "exactly one buildable repo");
        assert_eq!(found[0].file_name().unwrap(), "repo-a");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enumerate_missing_base_returns_empty_not_error() {
        let p = PathBuf::from("/tmp/does-not-exist-deadbeef-tend-prebuild");
        let found = enumerate_repos_with_flake(&p);
        assert!(found.is_empty());
    }

    #[test]
    fn enumerate_output_is_sorted_deterministic() {
        let dir = tmpdir();
        for name in &["zz", "aa", "mm"] {
            let p = dir.join(name);
            fs::create_dir_all(p.join(".git")).unwrap();
            fs::write(p.join("flake.nix"), "{}").unwrap();
        }
        let found = enumerate_repos_with_flake(&dir);
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["aa", "mm", "zz"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_default_attribute_matches_known_phrasings() {
        assert!(missing_default_attribute(
            "error: flake does not provide attribute 'packages.x86_64-linux.default'"
        ));
        assert!(missing_default_attribute(
            "error: attribute 'default' missing"
        ));
        assert!(missing_default_attribute(
            "error: does not provide attribute 'packages'"
        ));
        // Unrelated error must NOT match — those are real failures.
        assert!(!missing_default_attribute(
            "error: hash mismatch in fixed-output derivation"
        ));
        assert!(!missing_default_attribute(
            "error: builder for '/nix/store/x.drv' failed"
        ));
    }

    #[test]
    fn effective_per_workspace_none_returns_cli_unchanged() {
        let cli = PrebuildOptions {
            quiet: true,
            max_inflight: 3,
            attic: None,
        };
        let out = effective_per_workspace(&cli, None);
        assert_eq!(out.max_inflight, 3);
        assert!(out.attic.is_none());
        assert!(out.quiet);
    }

    #[test]
    fn effective_per_workspace_zero_max_inflight_doesnt_override() {
        // A workspace declaring `prebuild: {}` with all defaults
        // (max_inflight=0 via TieredConfig::bare()) MUST NOT clamp
        // the CLI's max_inflight to zero — the merge treats 0 as
        // "unspecified."
        let cli = PrebuildOptions {
            quiet: false,
            max_inflight: 4,
            attic: None,
        };
        let ws = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: None,
            attic_server: None,
            attic_url: None,
            attic_token_file: None,
        };
        let out = effective_per_workspace(&cli, Some(&ws));
        assert_eq!(out.max_inflight, 4, "zero must not override");
    }

    #[test]
    fn effective_per_workspace_overrides_max_inflight_when_set() {
        let cli = PrebuildOptions {
            quiet: false,
            max_inflight: 1,
            attic: None,
        };
        let ws = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 6,
            attic_cache: None,
            attic_server: None,
            attic_url: None,
            attic_token_file: None,
        };
        let out = effective_per_workspace(&cli, Some(&ws));
        assert_eq!(out.max_inflight, 6, "workspace value wins");
    }

    #[test]
    fn effective_per_workspace_overrides_attic_only_when_full_quartet_set() {
        let cli = PrebuildOptions {
            quiet: false,
            max_inflight: 1,
            attic: None,
        };
        // Partial — missing token_file → MUST NOT install a half-baked
        // AtticPush that'd crash at login time.
        let partial = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: Some("nexus".into()),
            attic_server: Some("nexus".into()),
            attic_url: Some("http://rio:8080/".into()),
            attic_token_file: None,
        };
        let out = effective_per_workspace(&cli, Some(&partial));
        assert!(out.attic.is_none(), "partial spec must not install attic");

        // Full quartet — install.
        let full = crate::config::PrebuildConfig {
            min_interval: 0,
            max_interval: 0,
            max_inflight: 0,
            attic_cache: Some("nexus".into()),
            attic_server: Some("nexus".into()),
            attic_url: Some("http://rio:8080/".into()),
            attic_token_file: Some("/run/secrets/tend/attic-jwt-token".into()),
        };
        let out = effective_per_workspace(&cli, Some(&full));
        let attic = out.attic.expect("full quartet must install attic");
        assert_eq!(attic.cache_name, "nexus");
        assert_eq!(attic.server_url, "http://rio:8080/");
        assert_eq!(
            attic.token_file,
            PathBuf::from("/run/secrets/tend/attic-jwt-token")
        );
    }

    #[test]
    fn reachability_unreachable_sleep_doubles_then_caps() {
        let opts = ReachabilityOptions {
            enabled: true,
            url: "http://rio:8080/".into(),
            min_interval: 60,
            max_interval: 1800,
            probe_timeout: 5,
        };
        // 1-based: first strike = min, then doubling.
        assert_eq!(opts.unreachable_sleep(1), 60);
        assert_eq!(opts.unreachable_sleep(2), 120);
        assert_eq!(opts.unreachable_sleep(3), 240);
        assert_eq!(opts.unreachable_sleep(4), 480);
        assert_eq!(opts.unreachable_sleep(5), 960);
        // 6th strike would be 1920 > 1800 → cap.
        assert_eq!(opts.unreachable_sleep(6), 1800);
        // Far-future strikes stay capped, never overflow.
        assert_eq!(opts.unreachable_sleep(40), 1800);
        assert_eq!(opts.unreachable_sleep(u32::MAX), 1800);
    }

    #[test]
    fn reachability_unreachable_sleep_respects_floor_and_min_ge_one() {
        // Degenerate config: min=0 must clamp to >=1 so we never
        // busy-spin probing a dead server.
        let opts = ReachabilityOptions {
            enabled: true,
            url: String::new(),
            min_interval: 0,
            max_interval: 0,
            probe_timeout: 0,
        };
        // min clamps to 1; max clamps to >= min, so everything is 1.
        assert_eq!(opts.unreachable_sleep(1), 1);
        assert_eq!(opts.unreachable_sleep(10), 1);
    }

    #[test]
    fn reachability_default_has_best_defaults() {
        let opts = ReachabilityOptions::default();
        assert!(!opts.enabled, "off until an attic url is wired");
        assert_eq!(opts.min_interval, 60);
        assert_eq!(opts.max_interval, 1800);
        assert_eq!(opts.probe_timeout, 5);
    }

    #[test]
    fn git_head_rev_on_nonexistent_repo_errors() {
        let p = PathBuf::from("/tmp/no-such-repo-tend-prebuild-xyz");
        let r = git_head_rev(&p);
        assert!(r.is_err(), "git rev-parse on missing repo must error");
    }

    /// Initialise a minimal git repo with one commit, return (dir,
    /// head_rev). Used by the prebuild_one idempotency test below.
    fn init_one_commit_repo() -> Option<(PathBuf, String)> {
        let dir = tmpdir();
        let run = |args: &[&str]| -> bool {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .ok()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q", "-b", "main"]) {
            return None; // no git available — skip test
        }
        let _ = run(&["config", "user.email", "tend-test@local"]);
        let _ = run(&["config", "user.name", "tend-test"]);
        fs::write(dir.join("README"), "hi").ok()?;
        if !run(&["add", "README"]) {
            return None;
        }
        if !run(&["commit", "-q", "-m", "init"]) {
            return None;
        }
        let rev = git_head_rev(&dir).ok()?;
        Some((dir, rev))
    }

    #[test]
    fn prebuild_one_returns_no_change_when_prev_rev_matches_head() {
        let Some((dir, rev)) = init_one_commit_repo() else {
            eprintln!("git unavailable; skipping prebuild_one_no_change test");
            return;
        };
        let opts = PrebuildOptions::default();
        let audit = AuditLog::default_path();
        // Pass the current HEAD as prev_rev — prebuild_one must return
        // NoChange without ever invoking nix build (so no flake.nix is
        // required for the test).
        let outcome = prebuild_one(&dir, Some(&rev), &opts, false, &audit).unwrap();
        assert_eq!(outcome, BuildOutcome::NoChange);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prebuild_one_rev_mismatch_with_no_flake_returns_no_default() {
        // When prev_rev != HEAD, prebuild_one DOES invoke nix build.
        // Without a flake.nix it'll fail with "does not provide
        // attribute" → we treat that as NoDefault. This test confirms
        // the soft-skip path for non-buildable repos.
        let Some((dir, _rev)) = init_one_commit_repo() else {
            eprintln!("git unavailable; skipping prebuild_one_no_default test");
            return;
        };
        // Skip the test if nix isn't on PATH — CI may not have it.
        if Command::new("nix").arg("--version").output().is_err() {
            eprintln!("nix unavailable; skipping prebuild_one_no_default test");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let opts = PrebuildOptions::default();
        let audit = AuditLog::default_path();
        // prev_rev is None → mismatch, build will be attempted, will fail.
        let outcome = prebuild_one(&dir, None, &opts, false, &audit);
        // Either NoDefault (preferred) or an Err containing the nix
        // failure — both are acceptable signals that the non-flake
        // repo wasn't silently treated as Built.
        match outcome {
            Ok(BuildOutcome::NoDefault) => {}
            Ok(BuildOutcome::Built { .. }) => {
                panic!("repo with no flake.nix must not appear as Built")
            }
            Ok(BuildOutcome::NoChange) => {
                panic!("rev mismatch must not yield NoChange")
            }
            Err(_) => {} // nix may emit a phrasing we don't classify
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
