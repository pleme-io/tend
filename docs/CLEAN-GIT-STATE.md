## Clean git state: status states, session hooks, push_ahead

The 2026-10-08 sweep of 1,335 repos found work `tend status` could not name: a
commit unpushed for 8 weeks, branches with commits and no upstream, clones of
empty repos. `status` now classifies from one `git status --porcelain=v2
--branch` per repo plus the remote witness: `ahead`, `no-upstream`, `behind`
and `unborn` join the old states (precedence and JSON shape: README). There is
no `archived-remote` state: discovery drops archived repos before caching, so
tend holds no archive fact to report, and `status` makes no API calls.

- **`tend hook stop`** — candidates are the `cwd` repo plus repos whose
  `.git/index`, `.git/HEAD` or `.git/refs/heads/<branch>` mtime is newer than
  the transcript's birth time; only stat runs on the other repos. Candidates
  are screened with one `git status` (`sync::observe_unsettled`: a clean,
  not-ahead branch is settled) and fully observed only when unsettled. Blocks
  once; with `stop_hook_active` it prints a `systemMessage` instead. A finding
  (repo label plus its description) blocks a session once: the hook records
  it under `<cache>/hook/stop/<session_id>.json`, and the same unchanged
  finding never blocks that session again, while a changed state blocks once
  more. Measured 2026-10-08: another session's edit in a shared akeylesslabs
  worktree blocked every stop of this one, since mtimes cannot say which
  session moved a repo.
  Measured on 1,364 repos (debug build, 2026-10-08): 0.13-0.6 s for a fresh
  session, 0.86-1.25 s for one started 12 h earlier (~160 repos touched,
  mostly by the daemon's fast-forwards). Classification stops at 1.2 s.
- **`tend hook session-start`** — a full live pass costs ~6 s here (git status
  ×1,364 is kernel-bound; more workers do not help), so it reads the daemon's
  snapshot (`status_snapshot`, `~/.cache/tend/status/local.json`) and
  re-checks live only the snapshot's problem rows, repos new since it, repos
  whose git state moved since it, and the `cwd` repo: 0.7-0.95 s measured.
  Without a fresh snapshot it checks live, most recently active first, and
  reports the coverage it reached.
- **Wording is data**: `push_policy: main | pr` and `pr_skill` per workspace.
  The default is `main`; tend has no per-name tier, so a PR-only org
  (akeylesslabs) must set `push_policy: pr` (and `pr_skill:
  akeyless-pr-standards`) in config.
- **`push_ahead`** (per workspace, default `false`): with `push_policy: main`,
  the daemon fast-forward-pushes a repo after its pull only through
  `push_ahead::guard` — verdict `ahead` with `behind == 0`, clean, on a branch
  equal to `refs/remotes/<remote>/HEAD`, tracking the same-named upstream.
  Unknown default branch refuses rather than guessing. Each push is an
  `ahead_pushed` audit event.
- **SIGTERM ends a cycle in progress** (`daemon::join_or_drain`): the
  workspace tasks are aborted and the daemon exits, rather than finishing a
  cycle that runs 15+ minutes under load. A rebuild boots the agent out and
  bootstraps the new plist at once; an old process still draining made that
  bootstrap fail (`I/O error (code 5)`) on three rebuilds on 2026-10-08,
  leaving the daemon unloaded while the rebuild printed `[OK]`.
- **The pull summary separates `failed` from `not reached`.** A job the
  throttled drain never ran (`Pending`, `Gated`, `Ready`, `Running` in the
  final snapshot; `reconcile::never_ran`) is not reached, never failed.
  Measured 2026-10-08 under load at `max_inflight` 2: a cycle printed
  `1056 failed` while its stderr held about 59 lines, one per real git
  failure; the rest had not run.
