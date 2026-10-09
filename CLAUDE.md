# tend

pending-unrep: RepoStatus::Clean carries a `RemoteWitness` proving a remote was
OBSERVED. That closes the
Tier ⊥ subclass A hole this repo actually shipped — a `clean` verdict over an
empty subject set, which reported `ferrite-zig` / `openclaw-publisher-pki` /
`pleme-app-core` healthy while their entire histories sat on one disk — but it
is not the full derived-verdict law. Two rows remain, each narrowed 2026-10-08:
(1) `Clean` is now reached only from an upstream comparison that came back
`+0 -0` (`git status --porcelain=v2 --branch`), or a detached HEAD some
remote-tracking ref contains; a tracked-nothing branch is `no-upstream`, never
`clean`. Still open: the witness carries neither compared sha, and there is no
fetch-recency bound, so "equal" means equal to the last fetched ref;
(2) `RepoStatus` is still one word chosen by precedence, but `RepoFacts`
(`ahead`, `behind`, `dirty_count`, `branch`) now travel beside it on every
observed row and in `--json`, so a dirty remote-less repo reports `no-remote`
WITH `dirty_count`, and a dirty repo shows its `ahead`. The destination is
unchanged: orthogonal axes (`{ local: Clean|Dirty|Stuck, backing: Backed(w)|None }`)
make the verdict word a projection rather than an ordering decision.
Tier landed: truly-unrepresentable OUTSIDE `sync.rs` (the witness field is
private to that module, so no other module can name `Clean` — proven by the
compile error this change produced in `jobs/status_repo.rs`); only-mitigated
WITHIN it, and the remote-set detection itself is a runtime `git remote` check,
i.e. only-mitigated. Do not round up.

> **★★★ CSE / Knowable Construction.** This repo operates under
> **Constructive Substrate Engineering** — canonical specification at
> [`pleme-io/theory/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md`](https://github.com/pleme-io/theory/blob/main/CONSTRUCTIVE-SUBSTRATE-ENGINEERING.md).
> The Compounding Directive (operational rules: solve once, load-bearing
> fixes only, idiom-first, models stay current, direction beats velocity)
> is in the org-level pleme-io/CLAUDE.md ★★★ section. Read both before
> non-trivial changes. Workspace repo manager + version watch daemon;
> the substrate's typed driver for fleet-wide flake.lock propagation.

Workspace repository manager — keeps repos in sync, detects upstream changes,
and automates version certification pipelines.

## Commands

| Command | Purpose |
|---------|---------|
| `sync` | Clone missing repos |
| `status` | Show repo status (clean/dirty/ahead/no-upstream/behind/unborn/stuck/no-remote/missing/unknown); `--problems` drops clean and unborn; `--json` adds `ahead`/`behind`/`dirty_count` |
| `hook stop\|session-start\|snapshot` | Claude Code hooks: block a Stop on touched dirty/unpushed repos; brief a new session on problem repos; refresh the snapshot SessionStart reads |
| `list` | List configured repos |
| `discover` | Discover repos from a GitHub org |
| `watch` | Run watch cycle once (detect new versions) |
| `daemon` | Persistent loop: sync + fetch + watch (`daemon.interval`, default 300s) |
| `flake-update` | Propagate nix flake updates through dependency chain |
| `init` | Generate starter config |
| `cargo-target report\|apply` | Size, idle days and verdict for every cargo `target/` dir; `apply` removes what the policy selects |
| `config-show [--effective [--provenance]]` | The config at a tier, or the one this invocation resolves and which layer set each leaf |
| `config-schema` | The config's JSON Schema (`schema/tend-config.schema.json`) |

## Config — one fold, flags are partials

`Config` (src/config.rs) resolves ONLY through `config_layers::ConfigLoader`
— shikumi's operator fold: computed tiers → discovered file (`$TEND_CONFIG`,
`tend/tend.yaml`, legacy `tend/config.yaml`) → each `--config` → `TEND_*`
env → the subcommand's typed flags (src/flags.rs) → each `--set`. A flag is
an `Option` whose serde shape IS the config path it sets (`--interval` →
`daemon.interval`); an absent flag contributes nothing, so there is one
default per knob, in `TieredConfig::prescribed_default`. Never give a config
flag a clap `default_value` — that silently beats the file again.

Three tend-specific rules in the loader: the env layer admits only `TEND_*`
names whose first segment is a config key (`TEND_STATE_DIR` and friends are
runtime env, and shikumi's strict extraction would otherwise refuse the whole
config); the legacy per-workspace `prebuild:` block is hoisted to the
top-level section in its own layer; `github_auth` is a LIST because shikumi
merges maps and a one-key-map sum type would merge into two keys.

The schema is golden-tested (`BLESS=1 cargo test schema` regenerates) and
held to what substrate's `jsonSchema.optionsFromJsonSchema` maps (no
`pattern`/`minLength`/tuples/...); the flake exports it as `lib.configSchema`.

## GitHub credential — one chain, one handle

`github_auth:` (default: shikumi's `GithubAuth::default_chain("tend")`)
is resolved by `gh_auth` with ONE process-lifetime `GithubAuthResolver`
(App installation tokens cached + refreshed). Every caller asks
`gh_auth::token()` and renders through `GithubToken` (`expose_for_header`
for REST, `gh_auth::git_auth` = per-process `GIT_CONFIG_*` extraheader for
git, `nix_access_tokens_line` for nix). Never `set_var("GITHUB_TOKEN")`,
never read the env for a token directly, never put a token in argv or a URL.
The token is memoized 60s; `begin_cycle()` (daemon cycle, SIGHUP) re-walks
the chain and logs the answering source once (`github_auth_resolved`).

## Architecture

```
src/
├── main.rs          # clap CLI dispatch (9 subcommands)
├── config.rs        # Config types (Workspace, DaemonConfig, PrebuildConfig, GithubAuthSources)
├── config_layers.rs # The one config load path (shikumi fold, env filter, legacy hoist)
├── flags.rs         # CLI flags as typed partials of Config
├── gh_auth.rs       # The one GitHub credential handle (shikumi GithubAuth chain)
├── schema.rs        # Config JSON Schema (golden: schema/tend-config.schema.json)
├── provider.rs      # GitHub API: discovery, HEAD, tags, language detection
├── sync.rs          # Repo resolution, cloning, status (RepoObservation → verdict + RepoFacts), fetching
├── scan.rs          # On-disk repo enumeration, bounded parallel classification, touched filter, status snapshot
├── hook.rs          # `tend hook stop|session-start`: Claude Code hook contracts
├── push_ahead.rs    # Daemon knob: fast-forward-push ahead-only default branches (guard + push)
├── daemon.rs        # Persistent loop (parallel workspaces via JoinSet)
├── watch.rs         # Version detection + matrix appending + auto-certify/commit/propagate
├── watch_cache.rs   # Watch state persistence (~/.cache/tend/watch/)
├── github.rs        # GitHubClient trait (abstracts API calls)
├── git.rs           # GitOps trait (abstracts git add/commit/push)
├── flake.rs         # Nix flake dependency chain (topological sort + execution)
├── cache.rs         # GitHub discovery cache (6-hour TTL)
└── display.rs       # Colored terminal output
```

## Watch Feature

Detects new upstream versions and feeds them into the akeyless-matrix certification pipeline.

### Configuration

```yaml
- name: akeyless-community
  provider: github
  base_dir: ~/code/github/akeyless-community
  clone_method: https
  discover: true
  org: akeyless-community
  watch:
    enable: true
    matrix_file: ~/code/github/pleme-io/blackmatter-akeyless/matrix.toml
    auto_certify: true       # run akeyless-matrix certify
    auto_commit: true        # git add + commit + push all changes
    auto_propagate: blackmatter-akeyless  # tend flake-update --changed
```

### Automated Cycle

```
daemon (300s, workspaces in parallel)
  → GitHub API: detect new tags or HEAD commits
  → append pending entry to matrix.toml (with rev)
  → auto_certify: akeyless-matrix certify (hash extraction + Nix generation)
  → auto_commit: git add (matrix.toml, lib/, builds/, certifications.toml) + commit + push
  → auto_propagate: tend flake-update → propagate to nix repo
```

### Tracking Modes

Packages in matrix.toml declare how they're tracked:

| Mode | Triggers on | Version format |
|------|-------------|---------------|
| `tags` (default) | New git tag | `1.0.0` (tag without v) |
| `commits` | HEAD SHA change | `0.1.0-unstable.2026-03-14.d240017e` |

### Self-Healing

When a build fails, the entry is marked `broken`. When upstream fixes the issue
and cuts a new tag, the next cycle creates a fresh `pending` entry that builds
from the new rev. Broken entries are excluded from generated Nix files.

## Trait Abstractions

| Trait | Purpose | Production impl |
|-------|---------|-----------------|
| `GitHubClient` | GitHub API (HEAD, tags, language) | `HttpGitHubClient` |
| `WatchStateStore` | Cache persistence | `FsWatchStateStore` |
| `MatrixAppender` | matrix.toml editing | `TomlMatrixAppender` |
| `GitOps` | git add/commit/push | `SystemGitOps` |

## File Watches

Monitor specific files in GitHub repos for content changes (e.g., OpenAPI specs).
When a file's SHA changes, tend downloads the new version, caches the SHA, and
runs post-hooks.

### Configuration

```yaml
watch:
  enable: true
  file_watches:
    - name: akeyless-openapi-spec
      org: akeylesslabs
      repo: akeyless-go
      path: api/openapi.yaml
      download_to: ~/code/github/pleme-io/akeyless-terraform-resources/specs
      post_hooks:
        - trigger: on_change
          command: iac-forge
          args: ["sync", "--spec-old", "$PREVIOUS_FILE", "--spec-new", "$CURRENT_FILE",
                 "--resources", "resources/", "--output", "out/", "--auto-scaffold"]
          working_dir: ~/code/github/pleme-io/akeyless-terraform-resources
```

### How It Works

1. `get_file_sha()` queries GitHub API for the file's blob SHA and size
2. Compares against cached SHA from `~/.cache/tend/watch/`
3. If changed: downloads file to `{download_to}/{sha[..12]}.{ext}`
4. Runs configured post-hooks with variable substitution
5. Updates cached SHA

### Variable Substitution in Post-Hooks

Post-hook `args` support these placeholders:
- `$VERSION` -- detected version string
- `$REPO` -- repository name
- `$REV` -- git revision/SHA
- `$MATRIX_FILE` -- path to matrix.toml
- `$PREVIOUS_FILE` -- previous downloaded file path (file watches)
- `$CURRENT_FILE` -- newly downloaded file path (file watches)
- `$FILE_SHA` -- new file SHA (file watches)

## Cargo target dirs (`src/cargo_target.rs`)

Local `cargo`/rust-analyzer output under the workspace repos is unmanaged by
nix and only grows (86.7 GiB across 35 dirs on the operator's Mac,
2026-09-29). The daemon bounds it when `cargo_target.enable` is set.

```yaml
cargo_target:
  enable: false          # daemon sweep; `tend cargo-target report` works regardless
  max_idle_days: 14      # newest mtime older than this -> removed whole
  budget_gib: 50         # total ceiling; least-recently-used removed first
  pressure_budget_gib: 10  # ceiling while pressure.rs reports disk pressure
  active_minutes: 60     # written this recently -> in use, never removed
  interval_minutes: 60   # minimum time between daemon sweeps
  search_depth: 2        # <repo>/target and <repo>/<dir>/target
  dry_run: false         # decide + report, delete nothing
```

- **Safety invariant:** only a `CargoTargetDir` can be removed, and
  `CargoTargetDir::recognize` is its only constructor: name exactly `target`,
  a real directory (not a symlink), holding a `CACHEDIR.TAG` with the cachedir
  signature AND cargo's "created by cargo" line. `remove` re-recognizes before
  deleting.
- **In use:** a held cargo build lock (`<target>/<profile>/.cargo-lock`,
  probed with a non-blocking shared `flock`) or a write within
  `active_minutes`. Removal holds every cargo lock exclusively while it
  deletes.
- The sweep runs BEFORE the pressure gate, since it frees disk. Each sweep
  logs a `cargo_target_sweep` audit event. MCP: `tend_cargo_targets`
  (read-only dry run).

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

## Post-Hooks

Configurable shell commands triggered at specific points in the watch cycle.

### Triggers

| Trigger | When |
|---------|------|
| `after_certify` | After `akeyless-matrix certify` completes |
| `after_commit` | After git commit+push |
| `after_propagate` | After `tend flake-update` propagation |
| `after_all` | After all steps complete |
| `on_change` | When a file watch detects a change |

### Configuration

```yaml
post_hooks:
  - trigger: after_certify
    command: notify-send
    args: ["tend", "Certification complete for $REPO $VERSION"]
    continue_on_error: true
  - trigger: after_all
    command: ./scripts/post-sync.sh
    working_dir: ~/code/github/pleme-io/nix
```

## Structured Audit Log

All watch cycle events are recorded in JSONL format at
`~/.local/share/tend/audit.jsonl`.

### Event Types

| Event | Data Fields |
|-------|-------------|
| `version_detected` | org, repo, version, rev, tracking |
| `matrix_entry_appended` | package, version, status |
| `hook_executed` | trigger, command, exit_code, duration_ms |
| `file_change_detected` | org, repo, path, old_sha, new_sha, file_size |
| `spec_downloaded` | org, repo, path, sha, local_path, size |
| `commit_pushed` | repo, commit, message |
| `ahead_pushed` | workspace, repo, remote, branch, ahead, ok, error |
| `certify_complete` | package, version, status, duration_ms |

### audit-log CLI Command

```bash
# Show last 20 events (default)
tend audit-log

# Filter by event type
tend audit-log --event version_detected

# Show last 50 entries
tend audit-log --last 50

# Filter by date
tend audit-log --since 2026-03-14

# Raw JSONL output (for piping to jq)
tend audit-log --json
```

## Testing

Run: `cargo test`
