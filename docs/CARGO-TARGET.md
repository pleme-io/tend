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
  extra_roots: []        # e.g. ["~/.cache"]: any name, extra_root_depth levels deep
  extra_root_depth: 2    # <root>/<x> and <root>/<x>/<y> (~/.cache/blue-target/<n>)
  adopt_untagged: false  # also remove a `target` with cargo's layout but no CACHEDIR.TAG
  dry_run: false         # decide + report, delete nothing
```

- **Safety invariant:** only a `CargoTargetDir` can be removed, and
  `CargoTargetDir::recognize` is its only constructor. A real directory (not a
  symlink) in one of four forms: `named` (`target` + cargo's `CACHEDIR.TAG`),
  `layout` (any name + cargo's tag + `.rustc_info.json` with a
  `rustc_fingerprint` + a `.fingerprint/` at `<profile>/` or
  `<triple>/<profile>/`), `output` (any name + cargo's tag + a cargo lock file
  at `<profile>/` or `<triple>/<profile>/`), `adopted` (`target` + that layout +
  no tag, only with `adopt_untagged`). `remove` re-recognizes under the same
  admission.
- **Any name, everywhere:** repos and extra roots use one walk. Agents make
  `target-*` dirs inside repos (`sui/target-clean` 26 GiB, measured on cid,
  2026-11-04), so every directory at search depth is offered to `recognize`. A
  name that merely looks similar proves nothing and is never a candidate. A
  directory with cargo's tag but no layout is reported as `no-cargo-layout` and
  not entered.
- **Why `output` exists:** with `build.build-dir` set, cargo keeps
  `.rustc_info.json` and `.fingerprint/` in the build dir. The target dir holds
  only the tag, the locks and the final artifacts. 43 of the 44 dirs in
  `~/.cache/blue-target/<n>/` on cid have this shape (2026-11-04).
- **Why tags go missing:** cargo writes the tag only into a directory it
  creates. rust-analyzer's `--target-dir target/rust-analyzer` creates
  `target/` as a plain parent first (`blue/target`, `kensa/target`, measured
  2026-10-23). Such dirs report `untagged-cargo-layout`.
- **In use** (any one keeps a dir): a held cargo lock; a same-user build
  process (`cargo`, `rustc`, rust-analyzer, `cargo-*`, build scripts) whose
  cwd is under the dir's parent or whose `CARGO_TARGET_DIR` /
  `CARGO_BUILD_BUILD_DIR` / `--target-dir` / argv overlaps it, or any same-user process running an
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

## The disk block (`src/disk.rs`)

Pressure reacts near empty: about 28 GiB free on a 926 GiB disk (`halt 20 +
inflight x 1`). Measured on cid, 2026-11-04: tend did nothing between 157 GiB
and 28 GiB free, and it knew no cache but cargo's (`Mozilla.sccache` 20 GiB,
`~/go/pkg` 19, nvim mason 13, `~/.cache/sui/inputs` 12). `disk:` keeps a disk
comfortably free. It is off by default, and off changes nothing.

```yaml
disk:
  enable: false
  free_goal_pct: 0       # 1 to 99; 0 = no goal; anything else is refused alone
  active_minutes: 60     # a child written this recently is in use
  dry_run: false         # caches only: decide + report, delete nothing
  caches:
    - name: go-mod
      path: ~/go/pkg/mod
      budget_gib: 8
      max_idle_days: 30  # default 14
      enable: true       # default false
      owners: [go]       # process names that hold the whole entry; optional
```

- **The goal:** `goal_of` in `blue/cargo_target.b` answers `off`, `refused`,
  `blind`, `met`, `below` or `pressure`. `below` and `pressure` tighten: the
  target sweep uses `pressure_budget_gib` and caches evict. The emergency floor
  in `pressure.rs` stays as it was and wins over the goal.
- **The clock:** below the goal the daemon sweeps every
  `pressure_interval_minutes`. A sweep that freed nothing waits for
  `interval_minutes` instead (`goal_sweep_due`), because measuring walks every
  inode. With `auto_apply` off, a goal sweep leaves the target dirs in dry run.
- **Eviction:** `plan_caches` decides each entry with the same `decide` as the
  target dirs: an in-use child stays, a child past `max_idle_days` goes, then
  the oldest go until the entry is under `budget_gib`. Those removals then run
  oldest first across all entries and stop when the goal is reached. What the
  target sweep frees counts first. A disk at its goal evicts nothing.
- **Safety invariant:** only a `CacheChild` can be removed, only
  `CacheDir::children` makes one, and `CacheDir::recognize` is the only way to a
  `CacheDir`. Recognize refuses a path that is relative, has `..`, is missing,
  is a symlink, is not a directory, resolves outside the home or
  `/private/var/cache`, is the home or that root itself, is user data (`.ssh`,
  `.gnupg`, `.config`, `.claude`, `Documents`, `Desktop`, `Downloads`,
  `Pictures`, `Movies`, `Music`, `Public`, `code`, anything inside them) or an
  aggregate of many tools' state (`.cache`, `.local`, `.local/share`,
  `.local/state`, `Library`, `Library/Caches`, `Library/Application Support`),
  or has a `.git` in it or in a parent below the home.
- **Children:** a symlink child is not a candidate and a symlink inside a child
  is never followed. A child holding a `.git` at any depth, or a directory that
  cannot be read, is `protected`: reported, kept, counted against the budget.
  `remove_child` re-recognizes the root, re-checks the child and re-walks the
  tree first. It then adds `u+rwx` through a handle proven to be the same inode
  as the lstat, so `go/pkg/mod`'s read-only trees delete.
- **Owner live:** a same-user process named in `owners`, or whose executable,
  working directory or an absolute argv path lies inside the entry, keeps the
  whole entry (`owner-live`). A blind process table keeps every entry (`blind`).
- **Refused alone:** a malformed entry, a typo in a key, an empty or a duplicate
  name, or a refused path is listed under `disk.refused` with its reason. Its
  siblings still run. An entry with `enable` false is listed under `disk.off`.
- **Granularity:** a child is one direct entry of `path`. `~/go/pkg/mod/github.com`
  is one child, so name the level you want evicted.
- **Reports:** `tend cargo-target report|apply` and `tend_cargo_targets` carry
  `disk`. `apply --dry-run` sets `dry_run` on both blocks. The daemon logs
  `disk_cache_removed` per child; `cargo_target_sweep` gains `trigger` `goal`
  and a `disk` summary.
- **Policy tests:** the goal and the cache plan are in `blue/cargo_target.b`, so
  the `blue test` line above covers them.
