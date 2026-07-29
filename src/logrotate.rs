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

use std::path::{Path, PathBuf};

/// Default cap per log file before rotation.
///
/// 16 MiB still holds well over `tend report`'s 7-day default window at
/// observed fleet volume — a week of transitions measured ~8 MiB once
/// the 4.8 GB backlog cleared — so rotation cannot silently discard
/// what the reader would have shown.
pub(crate) const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Default number of rotated generations to keep (`.1` … `.N`).
pub(crate) const DEFAULT_KEEP: usize = 2;

/// Total bytes the log directory may occupy.
///
/// **This is the guarantee; the per-file cap is only the shape.** A
/// per-file limit cannot bound a directory: at the previous 64 MiB x 3
/// generations across 3 logs the policy permitted 768 MiB while
/// appearing bounded, and the directory grew 68 -> 75 MB in an hour
/// under it. Adding a fourth log would have raised the ceiling again
/// with no code change.
///
/// [`enforce_budget`] prunes rotated generations, oldest first, until
/// the directory fits. Live files are never removed — dropping the file
/// currently being appended to would lose the newest data to save the
/// oldest, which is backwards.
pub(crate) const DEFAULT_TOTAL_BUDGET: u64 = 100 * 1024 * 1024;

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

/// What a budget sweep did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Budget {
    /// Directory absent, unreadable, or already inside the budget.
    WithinBudget { total_bytes: u64 },
    /// Rotated generations were removed to fit.
    Pruned {
        removed: Vec<PathBuf>,
        before_bytes: u64,
        after_bytes: u64,
    },
    /// Still over budget with no rotated generations left to drop, so
    /// the live files alone exceed it. That means the per-file cap is
    /// wrong; it is reported rather than resolved by deleting live data.
    LiveFilesExceedBudget { total_bytes: u64 },
}

/// The trailing `.<n>` of a rotated generation, or `None` for a live
/// log. `foo.jsonl` is live; `foo.jsonl.2` is generation 2.
fn generation_of(path: &Path) -> Option<usize> {
    path.extension()?.to_str()?.parse::<usize>().ok()
}

/// Prune rotated generations in `dir` until it fits within `total_max`.
///
/// Only `<name>.<n>` files are candidates; a live log is never removed.
/// Highest generation first, because `.3` is older than `.1` under the
/// rename chain in [`rotate_if_needed`].
pub(crate) fn enforce_budget(dir: &Path, total_max: u64) -> Budget {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Budget::WithinBudget { total_bytes: 0 };
    };

    let mut live: u64 = 0;
    let mut rotated: Vec<(usize, PathBuf, u64)> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        match generation_of(&path) {
            Some(n) => rotated.push((n, path, size)),
            None => live += size,
        }
    }

    let mut total: u64 = live + rotated.iter().map(|(_, _, s)| *s).sum::<u64>();
    if total <= total_max {
        return Budget::WithinBudget { total_bytes: total };
    }

    let before = total;
    rotated.sort_by(|a, b| b.0.cmp(&a.0));

    let mut removed = Vec::new();
    for (_, path, size) in rotated {
        if total <= total_max {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed.push(path);
        }
    }

    if total > total_max {
        return Budget::LiveFilesExceedBudget { total_bytes: total };
    }
    Budget::Pruned {
        removed,
        before_bytes: before,
        after_bytes: total,
    }
}

/// [`enforce_budget`] with the module default.
pub(crate) fn enforce_default_budget(dir: &Path) -> Budget {
    enforce_budget(dir, DEFAULT_TOTAL_BUDGET)
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


    // ── the total budget ──────────────────────────────────────────────

    /// A directory inside the budget is left completely alone.
    #[test]
    fn directory_within_budget_is_untouched() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("a.jsonl"), 100);
        write(&tmp.path().join("a.jsonl.1"), 100);

        assert_eq!(
            enforce_budget(tmp.path(), 1000),
            Budget::WithinBudget { total_bytes: 200 }
        );
        assert!(tmp.path().join("a.jsonl.1").exists());
    }

    /// The property the per-file cap could not give: a hard ceiling on
    /// the directory, regardless of how many logs live in it.
    #[test]
    fn budget_prunes_until_the_directory_fits() {
        let tmp = TempDir::new().unwrap();
        for name in ["a.jsonl", "b.jsonl", "c.jsonl"] {
            write(&tmp.path().join(name), 100);
            write(&tmp.path().join(format!("{name}.1")), 100);
            write(&tmp.path().join(format!("{name}.2")), 100);
        }
        // 900 bytes across 9 files.
        let out = enforce_budget(tmp.path(), 500);

        let Budget::Pruned { after_bytes, .. } = out else {
            panic!("expected Pruned, got {out:?}");
        };
        assert!(after_bytes <= 500, "still over budget: {after_bytes}");

        let total: u64 = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= 500, "on-disk total still over: {total}");
    }

    /// Oldest first. `.2` is older than `.1` under the rename chain, so
    /// it must go first — pruning newest-first would keep the least
    /// useful history.
    #[test]
    fn budget_prunes_oldest_generation_first() {
        let tmp = TempDir::new().unwrap();
        let live = tmp.path().join("a.jsonl");
        write(&live, 100);
        write(&gen(&live, 1), 100);
        write(&gen(&live, 2), 100);

        enforce_budget(tmp.path(), 250);

        assert!(live.exists(), "live file must never be pruned");
        assert!(gen(&live, 1).exists(), "newer generation should survive");
        assert!(!gen(&live, 2).exists(), "oldest generation should go first");
    }

    /// A live file is never removed, even when it alone blows the
    /// budget. Deleting the file being appended to would lose the
    /// newest data to save the oldest.
    #[test]
    fn live_files_are_never_pruned() {
        let tmp = TempDir::new().unwrap();
        let live = tmp.path().join("a.jsonl");
        write(&live, 1000);

        let out = enforce_budget(tmp.path(), 100);
        assert_eq!(out, Budget::LiveFilesExceedBudget { total_bytes: 1000 });
        assert!(live.exists());
    }

    /// The reported outcome must match what is actually on disk — a
    /// sweep that claims success while the directory is still over is
    /// worse than one that reports the overage.
    #[test]
    fn reported_after_bytes_matches_disk() {
        let tmp = TempDir::new().unwrap();
        let live = tmp.path().join("a.jsonl");
        write(&live, 50);
        write(&gen(&live, 1), 300);

        let Budget::Pruned { after_bytes, before_bytes, removed } =
            enforce_budget(tmp.path(), 100)
        else {
            panic!("expected Pruned");
        };
        assert_eq!(before_bytes, 350);
        assert_eq!(removed.len(), 1);

        let disk: u64 = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert_eq!(after_bytes, disk);
    }

    #[test]
    fn absent_directory_is_not_an_error() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(
            enforce_budget(&tmp.path().join("nope"), 100),
            Budget::WithinBudget { total_bytes: 0 }
        );
    }

    /// The defaults must actually deliver the stated ceiling. Three
    /// logs at the per-file cap plus their kept generations exceed
    /// 100 MiB on paper — which is precisely why the budget, not the
    /// cap, is the guarantee.
    #[test]
    fn defaults_state_the_real_policy() {
        assert_eq!(DEFAULT_MAX_BYTES, 16 * 1024 * 1024);
        assert_eq!(DEFAULT_KEEP, 2);
        assert_eq!(DEFAULT_TOTAL_BUDGET, 100 * 1024 * 1024);

        let per_file_worst = DEFAULT_MAX_BYTES * (DEFAULT_KEEP as u64 + 1);
        assert!(
            per_file_worst * 3 > DEFAULT_TOTAL_BUDGET,
            "if per-file caps alone fit the budget the sweep is untested in practice"
        );
    }
}
