# tend

Workspace repository manager -- keeps your repos in sync.

## Overview

Tend discovers, clones, and tracks the status of repositories across GitHub organizations. It reads a YAML config file defining workspaces (org, base directory, clone method), discovers repos via the GitHub API, clones missing ones, and reports status. Integrates with direnv via `use_tend` for automatic sync on directory entry, and with Claude Code through `tend hook` so sessions do not leave uncommitted or unpushed work behind.

## Usage

```bash
# Sync all workspaces (clone missing repos)
tend sync

# Sync a specific workspace
tend sync --workspace pleme-io

# Bypass discovery cache
tend sync --refresh

# Show repo status across all workspaces
tend status

# Show status for one workspace
tend status --workspace pleme-io

# Only the repos that need attention (everything but clean and unborn)
tend status --problems

# Machine-readable rows (izumi's tend-repos board reads this)
tend status --json
```

## Status states

| State | Meaning |
|-------|---------|
| `clean` | Tree clean, branch equal to its upstream (or a detached HEAD some remote holds) |
| `dirty` | Uncommitted changes, untracked files included |
| `ahead` | Committed work not on the upstream (`@{u}..HEAD > 0`), diverged included; a detached HEAD with commits on no remote |
| `no-upstream` | Has commits, but the branch tracks nothing (or its upstream ref is gone) |
| `behind` | Upstream has commits the branch lacks, nothing local to push |
| `unborn` | No commits yet (a clone of an empty repo); not a problem |
| `stuck` | Mid rebase, merge, cherry-pick or bisect |
| `no-remote` | No remote at all: the history exists on one machine |
| `missing` | Declared but not cloned |
| `unknown` | On disk but not declared |

Precedence when several apply: no-remote > stuck > dirty > unborn > ahead > no-upstream > behind > clean.

`tend status --json` emits an array of rows. The keys `name`, `path`, `state`, `clean_against_remote` (and `unreachable_because` on a synthetic workspace row) are unchanged; the counts are additive and omitted when not measured:

```json
{"name":"tend","path":"/Users/me/code/github/pleme-io/tend","state":"ahead","clean_against_remote":null,"ahead":2,"behind":0,"dirty_count":0}
```

`ahead`/`behind` compare against the branch's upstream. On a `no-upstream` branch or a detached HEAD, `ahead` counts commits on no remote-tracking ref and `behind` is absent.

## Claude Code hooks

`tend hook stop` and `tend hook session-start` read the hook payload as JSON on stdin and print the hook's JSON answer on stdout, or nothing. They never exit non-zero; a broken payload or config becomes a `systemMessage`.

| Command | Hook | Answer |
|---------|------|--------|
| `tend hook stop` | `Stop` | `{"decision":"block","reason":"..."}` when repos touched this session are dirty, stuck or hold unpushed commits; `{"systemMessage":"..."}` instead when `stop_hook_active` is true (never blocks twice); nothing when there is nothing to report |
| `tend hook session-start` | `SessionStart` | `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"..."}}` listing at most 15 problem repos worst first (stuck, dirty, ahead, no-upstream, no-remote, behind) with totals; nothing when the workspace is clean and fully checked |
| `tend hook snapshot` | none | Refreshes the status snapshot `session-start` reads |

A repo is *touched* this session when it contains the payload's `cwd`, or when its `.git/index`, `.git/HEAD` or `.git/refs/heads/<branch>` changed after the transcript file was created (its mtime when the filesystem has no birth time). Edits to files that never reach git (no `git add`, no commit) move none of those, so a repo edited only that way is caught when it is the `cwd` repo, by the next `session-start`, or by `tend status`.

Every hook scans the repos on disk under each workspace's `base_dir` (no discovery, no network), including workspaces with `track_repos: false`. Git runs only on candidates, with a 1.2 s classification budget; repos not reached are counted in the answer rather than dropped.

`session-start` cannot `git status` 1,300 repos within its budget (about 6 s on a 14-core Mac, kernel-bound), so the daemon writes a snapshot every cycle to `~/.cache/tend/status/local.json`. A snapshot younger than `max_age_minutes` is trusted for repos it saw clean whose git state has not moved since; its problem repos, repos new since it was taken and the `cwd` repo are re-checked live. With no fresh snapshot the hook checks live, most recently active first, and says how many it reached.

The instruction in each reason comes from the workspace's `push_policy`: `main` says "commit the authorized work and push main"; `pr` says "commit on a branch and open a PR; never push main", naming `pr_skill` when set. Every reason is phrased "if this is your work", since tend cannot tell which session owns a change.

Wiring (settings.json):

```json
{
  "hooks": {
    "Stop": [{ "hooks": [{ "type": "command", "command": "tend hook stop" }] }],
    "SessionStart": [{ "hooks": [{ "type": "command", "command": "tend hook session-start" }] }]
  }
}
```

## Configuration

tend's config is one typed shape (`src/config.rs`), resolved through
[shikumi](https://github.com/pleme-io/shikumi)'s fold. Lowest first:

1. built-in defaults (what `tend config-show default` prints)
2. the config file: `$TEND_CONFIG`, else `~/.config/tend/tend.yaml`, else `~/.config/tend/config.yaml`
3. each `--config FILE` (a merge-override, repeatable; later wins)
4. `TEND_*` env, `__` nesting (`TEND_DAEMON__INTERVAL=60`). Only names whose first segment is a config key are read; runtime variables such as `TEND_STATE_DIR` are not config.
5. the subcommand's flags (each flag is a partial: `tend daemon --interval 60` is `daemon.interval: 60`)
6. each `--set PATH=VALUE` (VALUE is YAML)

Maps merge per key; scalars and lists replace. An unknown or ill-typed key is refused, naming the layer that wrote it.

```sh
tend config-show --effective               # what this invocation resolves
tend config-show --effective --provenance  # ...and which layer set each value
gen kanshou query tend config              # what the RUNNING daemon resolved, its flags included
tend config-schema                         # the JSON Schema (schema/tend-config.schema.json)
```

The JSON Schema is committed at `schema/tend-config.schema.json` (a test fails when it is stale; `BLESS=1 cargo test schema` regenerates it) and exported by the flake as `lib.configSchema`, so Nix modules generate their options from it.

```yaml
status_snapshot:
  enable: true          # daemon writes the snapshot each cycle
  max_age_minutes: 30   # session-start trusts a snapshot this young
  workers: 4            # parallel git processes for the daemon's pass
workspaces:
  - name: pleme-io
    provider: github
    base_dir: ~/code/github/pleme-io
    clone_method: ssh
    discover: true
    org: pleme-io
    push_policy: main      # main | pr (default main)
    push_ahead: false      # daemon fast-forward-pushes ahead-only default branches
  - name: akeylesslabs
    base_dir: ~/code/github/akeylesslabs
    push_policy: pr
    pr_skill: akeyless-pr-standards
```

Daemon and prebuild settings are config too; their flags only override:

```yaml
daemon:              # tend daemon
  interval: 300
  pull: true
  fetch: true
  quiet: false
reconcile:
  max_inflight: 16   # concurrent git pulls (daemon, reconcile, pressure)
flake_update_daemon:
  min_interval: 60
  max_interval: 3600
prebuild:            # tend prebuild / prebuild-daemon
  max_inflight: 1
  packages: all
  repro: trusting
  attic_cache: nexus
  attic_url: http://rio:8080/
  attic_token_file: /run/secrets/attic
  probe: {enable: true, min_interval: 60, max_interval: 1800, timeout: 5}
```

### GitHub credential

Every GitHub call tend makes (discovery, clone, REST, nix `access-tokens`) resolves one ordered list of sources; the first that yields a token wins. A token is never logged; which source answered is written to the audit log once per cycle (`tend audit-log --event github_auth_resolved`).

```yaml
github_auth:                     # default shown
  - token: {env: TEND_GITHUB_TOKEN}
  - token: {env: GITHUB_TOKEN}
  - token: {env: GH_TOKEN}
  - gh_cli: {host: github.com}   # gh auth token
  - token: {file: ~/.config/github/token}
```

A GitHub App mints and refreshes installation tokens itself:

```yaml
github_auth:
  - app:
      app_id: 123456                       # or a secret source: {env: APP_ID}
      private_key: {file: /run/secrets/app.pem}
      owner: pleme-io                      # or installation_id: 987
```

A `token:` or `private_key:` takes any shikumi secret source (`env`, `file`, `command`, `sops`, `op`, `akeyless`, `vault`, ...). Writing `github_auth` anywhere replaces the whole list. The flags `--github-token-file PATH` and `--github-app-id/--github-app-key-file/--github-app-owner/--github-app-installation-id` work on every subcommand and replace `github_auth` for that run.

`push_ahead` (default `false`): when `true` and `push_policy` is `main`, the daemon pushes a repo after its pull step only if the repo is clean, ahead of its upstream and not behind, on the remote's default branch (`refs/remotes/<remote>/HEAD`), and tracking the same-named upstream branch. The push is a plain fast-forward (`git push <remote> refs/heads/<b>:refs/heads/<b>`), never forced. Every push is logged as an `ahead_pushed` audit event.

## Features

- GitHub org discovery (auto-discovers repos via API)
- SSH and HTTPS clone methods
- Discovery caching (skip API calls on repeat syncs)
- direnv integration (`use_tend` shell function)
- Colored status output with ahead/behind/dirty counts
- Claude Code Stop and SessionStart hooks

## License

MIT
