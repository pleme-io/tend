## Cargo target dirs (`src/cargo_target.rs`)

Local `cargo`/rust-analyzer output under the workspace repos is unmanaged by
nix and only grows (86.7 GiB across 35 dirs on the operator's Mac,
2026-09-29); agent builds also put `CARGO_TARGET_DIR` under `~/.cache`
(~20 GiB on ryn, 2026-10-23, outside every repo). The daemon sweeps every
`interval_minutes` when `enable` is set, and, with `auto_apply` (default on),
every `pressure_interval_minutes` while disk is pressured.

```yaml
cargo_target:
  enable: false          # scheduled daemon sweep; `tend cargo-target report` works regardless
  auto_apply: true       # relief sweep (apply) while disk is pressured, even with enable off
  pressure_interval_minutes: 5
  max_idle_days: 14      # newest mtime older than this -> removed whole
  budget_gib: 50         # total ceiling; least-recently-used removed first
  pressure_budget_gib: 10  # ceiling while pressure.rs reports disk pressure
  active_minutes: 60     # written this recently -> in use, never removed
  interval_minutes: 60   # minimum time between scheduled sweeps
  search_depth: 2        # <repo>/target and <repo>/<dir>/target
  extra_roots: []        # e.g. ["~/.cache"]: scanned one level deep, any name
  adopt_untagged: false  # also remove a `target` with cargo's layout but no CACHEDIR.TAG
  dry_run: false         # decide + report, delete nothing
```

- **Safety invariant:** only a `CargoTargetDir` can be removed, and
  `CargoTargetDir::recognize` is its only constructor. A real directory (not a
  symlink) in one of three forms: `named` (`target` + cargo's `CACHEDIR.TAG`),
  `layout` (any name + cargo's tag + `.rustc_info.json` with a
  `rustc_fingerprint` + a `.fingerprint/` at `<profile>/` or
  `<triple>/<profile>/`), `adopted` (`target` + that layout + no tag, only with
  `adopt_untagged`). `remove` re-recognizes under the same admission.
- **Why tags go missing:** cargo writes the tag only into a directory it
  creates. rust-analyzer's `--target-dir target/rust-analyzer` creates
  `target/` as a plain parent first (`blue/target`, `kensa/target`, measured
  2026-10-23). Such dirs report `untagged-cargo-layout`.
- **In use** (any one keeps a dir): a held cargo lock; a same-user build
  process (`cargo`, `rustc`, rust-analyzer, `cargo-*`, build scripts) whose
  cwd is under the dir's parent or whose `CARGO_TARGET_DIR` /
  `--target-dir` / argv overlaps it, or any same-user process running an
  executable inside it; processes unseen (blind: table or a same-user build
  process unreadable -> every dir kept); a write within `active_minutes`.
  Other users' processes (`_nixbld` rustc) are skipped; `ps` names the owners
  sysinfo cannot read. Removal holds every cargo lock exclusively.
- **Policy is blue:** `blue/cargo_target.b` (`decide`) picks the verdicts and
  the removal order (oldest first, larger first at equal age), loaded through
  `src/blue_policy.rs` like `blue/align.b`. Rust refuses any plan that removes
  a dir an in-use signal holds. Test the blue with the pinned CLI:
  `BLUE_PATH=<blue checkout at the Cargo.lock rev>/bidamas blue test blue/cargo_target.b`.
- The sweep runs BEFORE the pressure gate, since it frees disk. Each sweep
  logs `cargo_target_sweep`, each removal `cargo_target_removed` (path,
  bytes, form, verdict, trigger) plus a stderr line; `gen kanshou query tend
  cargo_target` returns the last report. MCP: `tend_cargo_targets`
  (read-only dry run, shows extra roots and process sight).
