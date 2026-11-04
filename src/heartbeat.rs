use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Heartbeat {
    pub pid: u32,
    pub tick_unix: u64,
    pub state: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Liveness {
    Running {
        pid: u32,
        tick_age_secs: u64,
        state: String,
        why: String,
    },
    Gone {
        pid: u32,
    },
    Unseen,
}

pub(crate) fn path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("daemon")
        .join("heartbeat.json")
}

pub(crate) fn unix(now: SystemTime) -> u64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn beat(path: &Path, state: &str, why: &str, now: SystemTime) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let beat = Heartbeat {
        pid: std::process::id(),
        tick_unix: unix(now),
        state: state.to_owned(),
        why: why.to_owned(),
    };
    std::fs::write(&tmp, serde_json::to_vec(&beat)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read(path: &Path) -> Option<Heartbeat> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub(crate) fn process_named_tend(pid: u32) -> Option<bool> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).contains("tend"))
}

pub(crate) fn observe(
    path: &Path,
    now: SystemTime,
    alive: impl Fn(u32) -> Option<bool>,
) -> Liveness {
    let Some(beat) = read(path) else {
        return Liveness::Unseen;
    };
    match alive(beat.pid) {
        Some(true) => Liveness::Running {
            pid: beat.pid,
            tick_age_secs: unix(now).saturating_sub(beat.tick_unix),
            state: beat.state,
            why: beat.why,
        },
        Some(false) => Liveness::Gone { pid: beat.pid },
        None => Liveness::Unseen,
    }
}

pub(crate) fn restart_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchctl kickstart -k gui/$(id -u)/io.pleme.tend-daemon"
    } else {
        "restart tend's daemon service"
    }
}

fn check_command() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchctl print gui/$(id -u)/io.pleme.tend-daemon"
    } else {
        "ask the service manager for tend's daemon"
    }
}

impl Liveness {
    pub(crate) fn sentence(&self) -> String {
        match self {
            Self::Running {
                pid,
                tick_age_secs,
                state,
                why,
            } => {
                let mut s = format!(
                    "The daemon runs (pid {pid}, its last cycle began {} ago",
                    crate::reach::human_age(*tick_age_secs)
                );
                if state != "proceed" {
                    s.push_str(&format!(", {state}: {why}"));
                }
                s.push_str("); the snapshot is overdue, the daemon is not dead, so do not restart it.");
                s
            }
            Self::Gone { pid } => format!(
                "No tend process holds the daemon's last pid ({pid}), so it is not running; restart it: {}.",
                restart_command()
            ),
            Self::Unseen => format!(
                "The daemon left no readable heartbeat, so its state is unseen, not dead; check: {}.",
                check_command()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const NOW: u64 = 2_000_000_000;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn no_heartbeat_is_unseen_never_dead() {
        let tmp = tempfile::TempDir::new().unwrap();
        let seen = observe(&tmp.path().join("none.json"), at(NOW), |_| Some(true));
        assert_eq!(seen, Liveness::Unseen);
        assert!(seen.sentence().contains("unseen, not dead"));
        assert!(!seen.sentence().contains("restart it"));
    }

    #[test]
    fn a_beat_round_trips_and_a_live_pid_is_running_with_its_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("deep/heartbeat.json");
        beat(&path, "throttle", "load average is 300%", at(NOW)).unwrap();
        let seen = observe(&path, at(NOW + 7200), |pid| {
            assert_eq!(pid, std::process::id());
            Some(true)
        });
        match &seen {
            Liveness::Running {
                tick_age_secs,
                state,
                ..
            } => {
                assert_eq!(*tick_age_secs, 7200);
                assert_eq!(state, "throttle");
            }
            other => panic!("{other:?}"),
        }
        let text = seen.sentence();
        assert!(text.contains("The daemon runs"), "{text}");
        assert!(text.contains("throttle: load average is 300%"), "{text}");
        assert!(text.contains("do not restart it"), "{text}");
    }

    #[test]
    fn a_dead_pid_is_gone_and_says_restart() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("heartbeat.json");
        beat(&path, "proceed", "", at(NOW)).unwrap();
        let seen = observe(&path, at(NOW), |_| Some(false));
        assert!(matches!(seen, Liveness::Gone { .. }));
        assert!(seen.sentence().contains("restart it"));
    }

    #[test]
    fn a_probe_that_could_not_run_is_unseen() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("heartbeat.json");
        beat(&path, "proceed", "", at(NOW)).unwrap();
        assert_eq!(observe(&path, at(NOW), |_| None), Liveness::Unseen);
    }

    #[test]
    fn the_process_probe_finds_this_test_binary_and_not_a_free_pid() {
        assert_eq!(process_named_tend(std::process::id()), Some(true));
        assert_eq!(process_named_tend(999_999), Some(false));
    }
}
