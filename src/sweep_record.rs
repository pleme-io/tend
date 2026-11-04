use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

use crate::heartbeat::unix;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Outcome {
    Ok,
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SweepRecord {
    pub outcome: Outcome,
    pub why: String,
    pub at_unix: u64,
    pub refused_since_unix: Option<u64>,
    pub refused_count: u64,
    pub version: String,
}

pub(crate) fn path() -> PathBuf {
    crate::cache::tend_cache_root()
        .join("daemon")
        .join("sweep.json")
}

pub(crate) fn read(path: &Path) -> Option<SweepRecord> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write(path: &Path, record: &SweepRecord) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(record)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn note_ok(path: &Path, now: SystemTime) -> anyhow::Result<()> {
    write(
        path,
        &SweepRecord {
            outcome: Outcome::Ok,
            why: String::new(),
            at_unix: unix(now),
            refused_since_unix: None,
            refused_count: 0,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    )
}

pub(crate) fn note_refused(path: &Path, why: &str, now: SystemTime) -> anyhow::Result<()> {
    let before = read(path).filter(|r| r.outcome == Outcome::Refused);
    write(
        path,
        &SweepRecord {
            outcome: Outcome::Refused,
            why: why.to_owned(),
            at_unix: unix(now),
            refused_since_unix: before
                .as_ref()
                .and_then(|r| r.refused_since_unix)
                .or(Some(unix(now))),
            refused_count: before.map_or(0, |r| r.refused_count) + 1,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    )
}

pub(crate) fn policy_probe() -> Result<(), String> {
    crate::blue_policy::load_pure(None, crate::cargo_target::POLICY_SOURCE, "cargo_target.b")
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

pub(crate) fn note_probe(
    path: &Path,
    probe: Result<(), String>,
    now: SystemTime,
) -> Option<String> {
    let noted = match &probe {
        Ok(()) => note_ok(path, now),
        Err(why) => note_refused(path, why, now),
    };
    let unwritten = noted
        .err()
        .map(|e| format!("sweep record unwritten: {e:#}"));
    probe.err().or(unwritten)
}

pub(crate) fn status(
    record: Option<&SweepRecord>,
    probe: Result<(), String>,
    now: SystemTime,
) -> Json {
    let policy = match probe {
        Ok(()) => json!({"loads": true}),
        Err(why) => json!({"loads": false, "why": why}),
    };
    let Some(r) = record else {
        return json!({"outcome": "unseen", "policy": policy});
    };
    json!({
        "outcome": r.outcome,
        "why": r.why,
        "age_secs": unix(now).saturating_sub(r.at_unix),
        "refused_since_unix": r.refused_since_unix,
        "refused_count": r.refused_count,
        "version": r.version,
        "policy": policy,
    })
}

pub(crate) fn session_note(record: Option<&SweepRecord>) -> Option<String> {
    let r = record.filter(|r| r.outcome == Outcome::Refused)?;
    Some(format!(
        "tend: the disk sweep is REFUSED, so nothing frees disk ({} time(s), tend {}): {}",
        r.refused_count, r.version, r.why
    ))
}

pub(crate) fn line(record: Option<&SweepRecord>) -> String {
    match record {
        None => "sweep         unseen: no daemon sweep recorded".to_owned(),
        Some(r) if r.outcome == Outcome::Ok => "sweep         ok".to_owned(),
        Some(r) => format!(
            "sweep         REFUSED {} time(s), nothing frees disk: {}",
            r.refused_count, r.why
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    const JUNJO: &str = "cargo_target.b: import error: cannot load bidama \"junjo\"";

    #[test]
    fn refusals_count_up_from_their_first_and_an_ok_clears_them() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("daemon/sweep.json");
        note_refused(&p, JUNJO, at(100)).unwrap();
        note_refused(&p, JUNJO, at(400)).unwrap();
        let r = read(&p).unwrap();
        assert_eq!(r.outcome, Outcome::Refused);
        assert_eq!(r.refused_count, 2);
        assert_eq!(r.refused_since_unix, Some(100));
        assert_eq!(r.at_unix, 400);

        note_ok(&p, at(500)).unwrap();
        let r = read(&p).unwrap();
        assert_eq!(r.outcome, Outcome::Ok);
        assert_eq!(r.refused_count, 0);
        assert_eq!(r.refused_since_unix, None);

        note_refused(&p, JUNJO, at(600)).unwrap();
        assert_eq!(read(&p).unwrap().refused_since_unix, Some(600));
    }

    #[test]
    fn a_refused_load_is_loud_on_every_surface() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("sweep.json");
        note_refused(&p, JUNJO, at(100)).unwrap();
        let r = read(&p);

        let s = status(r.as_ref(), Ok(()), at(160));
        assert_eq!(s["outcome"], "refused");
        assert_eq!(s["why"], JUNJO);
        assert_eq!(s["age_secs"], 60);
        assert_eq!(s["refused_count"], 1);

        assert!(session_note(r.as_ref()).unwrap().contains("junjo"));
        assert!(line(r.as_ref()).contains("REFUSED"));
        assert!(line(r.as_ref()).contains("junjo"));
    }

    #[test]
    fn a_clean_sweep_is_quiet_and_no_record_is_unseen_never_ok() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("sweep.json");
        note_ok(&p, at(100)).unwrap();
        let r = read(&p);
        assert_eq!(session_note(r.as_ref()), None);
        assert_eq!(status(r.as_ref(), Ok(()), at(100))["outcome"], "ok");

        assert_eq!(status(None, Ok(()), at(100))["outcome"], "unseen");
        assert!(line(None).contains("unseen"));
        assert_eq!(session_note(None), None);
    }

    #[test]
    fn a_broken_binary_shows_in_the_live_probe_even_with_a_stale_ok_record() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("sweep.json");
        note_ok(&p, at(100)).unwrap();
        let s = status(read(&p).as_ref(), Err(JUNJO.to_owned()), at(100));
        assert_eq!(s["policy"]["loads"], false);
        assert_eq!(s["policy"]["why"], JUNJO);
    }

    #[test]
    fn the_startup_probe_records_and_returns_a_refusal() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("sweep.json");
        let loud = note_probe(&p, Err(JUNJO.to_owned()), at(100));
        assert_eq!(loud.as_deref(), Some(JUNJO));
        assert_eq!(read(&p).unwrap().outcome, Outcome::Refused);
        assert_eq!(note_probe(&p, Ok(()), at(200)), None);
        assert_eq!(read(&p).unwrap().outcome, Outcome::Ok);
    }

    #[test]
    fn this_binary_loads_its_own_policy() {
        assert_eq!(policy_probe(), Ok(()));
    }
}
