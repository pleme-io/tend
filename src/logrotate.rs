//! Size-bounded rotation for tend's append-only JSONL logs.
//!
//! # Why
//!
//! tend writes three append-only logs under its data dir, none of
//! which had any bound:
//!
//! | log | writer | reader |
//! |---|---|---|
//! | `scheduler-transitions.jsonl` | shigoto `AuditFileEmitter` | `tend report` |
//! | `drift-events.jsonl` | `AuditFileDriftSink` | `tend doctor` |
//! | `audit.jsonl` | `audit::AuditLog` | operator, ad hoc |
//!
//! On 2026-07-29 the transitions log was **4.8 GB across 22M lines**,
//! spanning ten weeks. Almost none of it is reachable: `tend report`
//! defaults to a 7-day window, so >85% of the file could not affect
//! any output. It is near-entirely routine success chatter — three
//! reasons (`GateEvaluation`, `BudgetAllocated`, `ExecutionSucceeded`)
//! across four job kinds, emitted for every repo on every cycle, at
//! ~100-300K entries/day and accelerating with the repo count.
//!
//! The growth had already broken its only consumer: `build_report`
//! slurped the whole file to compute that 7-day window, peaking at
//! 4.95 GB RSS. Fine on a workstation, an OOM kill in the operator
//! pod. That read is now streamed; this module stops the file from
//! growing without bound in the first place.
//!
//! # What this is not
//!
//! Not a replacement for logrotate(8) or a journald sink. tend writes
//! these files itself, to a path it chooses, on hosts that may have
//! neither — so the bound belongs where the writer is. Rotation is
//! checked at sink-construction time (once per reconcile cycle, not
//! per line), which is frequent enough to bound growth and cheap
//! enough to ignore.

use std::path::Path;

/// Default cap per log file before rotation. 64 MiB holds roughly a
/// week of transitions at observed fleet volume — comfortably more
/// than `tend report`'s 7-day default window, so rotation cannot
/// silently truncate what the reader would have shown.
pub(crate) const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Default number of rotated generations to keep (`.1` … `.N`).
/// Three generations plus the live file bounds a log at ~256 MiB.
pub(crate) const DEFAULT_KEEP: usize = 3;

/// What a rotation check decided. Returned rather than logged so
/// callers can surface it and tests can assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rotation {
    /// File absent or under the cap.
    NotNeeded,
    /// Rotated; carries the size that triggered it.
    Rotated { was_bytes: u64 },
    /// Rotation failed. Never fatal: a log we cannot rotate is still a
    /// log we can append to, and losing observability is a worse
    /// outcome than an oversized file.
    Failed { error: String },
}

/// Rotate `path` if it exceeds `max_bytes`, keeping `keep` generations.
///
/// `foo.jsonl.2` → `foo.jsonl.3`, `foo.jsonl.1` → `foo.jsonl.2`,
/// `foo.jsonl` → `foo.jsonl.1`, and the oldest is dropped. Renames
/// walk oldest-first so no generation is overwritten before it moves.
///
/// The live path is left absent afterward; every writer here opens
/// append-and-create, so the next write recreates it. Recreating an
/// empty file eagerly would be a second failure mode for no gain.
pub(crate) fn rotate_if_needed(path: &Path, max_bytes: u64, keep: usize) -> Rotation {
    let Ok(meta) = std::fs::metadata(path) else {
        return Rotation::NotNeeded;
    };
    let size = meta.len();
    if size <= max_bytes {
        return Rotation::NotNeeded;
    }

    let generation = |n: usize| {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{n}"));
        std::path::PathBuf::from(p)
    };

    if keep == 0 {
        return match std::fs::remove_file(path) {
            Ok(()) => Rotation::Rotated { was_bytes: size },
            Err(e) => Rotation::Failed {
                error: e.to_string(),
            },
        };
    }

    // Drop the generation that is about to fall off the end.
    let _ = std::fs::remove_file(generation(keep));

    // Shift the rest down, oldest first.
    for n in (1..keep).rev() {
        let from = generation(n);
        if from.exists() {
            if let Err(e) = std::fs::rename(&from, generation(n + 1)) {
                return Rotation::Failed {
                    error: e.to_string(),
                };
            }
        }
    }

    match std::fs::rename(path, generation(1)) {
        Ok(()) => Rotation::Rotated { was_bytes: size },
        Err(e) => Rotation::Failed {
            error: e.to_string(),
        },
    }
}

/// [`rotate_if_needed`] with the module defaults. The call shape every
/// site uses, so the policy lives in one place rather than being
/// re-specified per log.
pub(crate) fn rotate(path: &Path) -> Rotation {
    rotate_if_needed(path, DEFAULT_MAX_BYTES, DEFAULT_KEEP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, bytes: usize) {
        std::fs::write(path, "x".repeat(bytes)).unwrap();
    }

    fn gen(path: &Path, n: usize) -> std::path::PathBuf {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{n}"));
        std::path::PathBuf::from(p)
    }

    #[test]
    fn absent_file_needs_no_rotation() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            rotate_if_needed(&tmp.path().join("nope.jsonl"), 100, 3),
            Rotation::NotNeeded
        );
    }

    #[test]
    fn file_under_cap_is_untouched() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");
        write(&log, 50);
        assert_eq!(rotate_if_needed(&log, 100, 3), Rotation::NotNeeded);
        assert!(log.exists());
    }

    /// Boundary: exactly at the cap is not over it.
    #[test]
    fn file_exactly_at_cap_is_untouched() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");
        write(&log, 100);
        assert_eq!(rotate_if_needed(&log, 100, 3), Rotation::NotNeeded);
        assert!(log.exists());
    }

    #[test]
    fn oversized_file_moves_to_generation_one() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");
        write(&log, 200);

        assert_eq!(
            rotate_if_needed(&log, 100, 3),
            Rotation::Rotated { was_bytes: 200 }
        );
        assert!(!log.exists(), "live path should be absent after rotation");
        assert_eq!(std::fs::metadata(gen(&log, 1)).unwrap().len(), 200);
    }

    /// Generations shift down and content follows the right file — the
    /// property a naive rename loop gets wrong by clobbering `.2` with
    /// `.1` before `.2` has moved.
    #[test]
    fn generations_shift_without_clobbering() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");

        std::fs::write(gen(&log, 1), "first").unwrap();
        std::fs::write(gen(&log, 2), "second").unwrap();
        write(&log, 200);

        rotate_if_needed(&log, 100, 3);

        assert_eq!(std::fs::metadata(gen(&log, 1)).unwrap().len(), 200);
        assert_eq!(std::fs::read_to_string(gen(&log, 2)).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(gen(&log, 3)).unwrap(), "second");
    }

    /// The bound actually binds: repeated rotation never accumulates
    /// more than `keep` generations.
    #[test]
    fn retention_is_bounded_across_many_rotations() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");

        for _ in 0..10 {
            write(&log, 200);
            rotate_if_needed(&log, 100, 3);
        }

        assert!(gen(&log, 3).exists());
        assert!(!gen(&log, 4).exists(), "retention exceeded keep=3");
        assert!(!gen(&log, 5).exists());
    }

    #[test]
    fn keep_zero_discards_outright() {
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");
        write(&log, 200);

        assert_eq!(
            rotate_if_needed(&log, 100, 0),
            Rotation::Rotated { was_bytes: 200 }
        );
        assert!(!log.exists());
        assert!(!gen(&log, 1).exists());
    }

    /// The live path must be recreatable by a plain append-open, since
    /// that is what every writer does after rotation.
    #[test]
    fn append_after_rotation_starts_a_fresh_file() {
        use std::io::Write as _;
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");
        write(&log, 200);
        rotate_if_needed(&log, 100, 3);

        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        writeln!(f, "{{\"fresh\":true}}").unwrap();
        drop(f);

        assert_eq!(std::fs::read_to_string(&log).unwrap(), "{\"fresh\":true}\n");
    }

    #[test]
    fn defaults_are_the_documented_policy() {
        assert_eq!(DEFAULT_MAX_BYTES, 64 * 1024 * 1024);
        assert_eq!(DEFAULT_KEEP, 3);
        let tmp = TempDir::new().unwrap();
        let log = tmp.path().join("a.jsonl");
        write(&log, 1024);
        assert_eq!(rotate(&log), Rotation::NotNeeded);
    }
}
