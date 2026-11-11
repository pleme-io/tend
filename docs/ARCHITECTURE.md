# tend Architecture

## Problem

82+ repos across multiple GitHub orgs need to be cloned, synced, and monitored
for upstream changes. Version updates to Akeyless packages require detection,
hash extraction, Nix generation, and propagation — a multi-step pipeline that
should be fully automated.

## Solution

A workspace-aware daemon that discovers repos via the GitHub API, keeps local
clones in sync, watches for upstream version changes, and drives the
akeyless-matrix certification pipeline.

## Daemon Architecture

```
tend daemon (300s cycle)
  │
  │  tokio::JoinSet (parallel workspaces)
  │
  ├─ pleme-io         (82 repos)  → sync + fetch
  ├─ akeyless-community (47 repos)  → sync + fetch + watch
  ├─ akeylesslabs     (35 repos)  → sync + fetch + watch
  └─ drzln                         → sync + fetch
       │
       ▼  per workspace:
  1. resolve_repos: GitHub API discovery + extra_repos - excludes
  2. sync_repos: git clone missing repos
  3. fetch_repos: git fetch --all --prune
  4. watch (if enabled):
       a. get_repo_head + get_latest_tag per repo (GitHub API)
       b. compare with cached state (~/.cache/tend/watch/)
       c. if changed: detect language, compute version, append to matrix.toml
       d. auto_certify: akeyless-matrix certify
       e. auto_commit: git add + commit + push
       f. auto_propagate: tend flake-update --changed
```

## Watch Configuration

```yaml
watch:
  enable: true
  matrix_file: ~/code/github/pleme-io/blackmatter-akeyless/matrix.toml
  auto_certify: true        # run akeyless-matrix certify
  auto_commit: true         # git add+commit+push all changes
  auto_propagate: blackmatter-akeyless  # tend flake-update --changed
```

## Tracking Modes

The watch reads `track` from matrix.toml per package:

| Mode | Detection | Version format | Example |
|------|-----------|---------------|---------|
| `tags` (default) | Latest git tag changed | `<tag sans v>` | `1.0.0` |
| `commits` | HEAD SHA changed | `<base>-unstable.<date>.<sha>` | `0.1.0-unstable.2026-03-14.d240017e` |

Tag-tracked repos ignore HEAD-only changes. Commit-tracked repos trigger on
every new commit regardless of tags.

## Flake Dependency Chain

`tend flake-update --changed <repo>` propagates updates through the dependency graph:

```
blackmatter-akeyless changed
  → nix depends on blackmatter-akeyless (via flake_deps)
  → tend runs: cd nix && nix flake update blackmatter-akeyless
  → tend runs: git add flake.lock && git commit && git push
```

Uses Kahn's topological sort for multi-hop dependency chains with cycle detection.

## Caching

| Cache | Location | TTL | Purpose |
|-------|----------|-----|---------|
| Discovery | `~/.cache/tend/discovery/{org}.json` | 6 hours | GitHub org repo lists |
| Watch state | `~/.cache/tend/watch/{workspace}.toml` | Persistent | HEAD SHAs, tags, languages per repo |

## Trait Architecture

```
main.rs / daemon.rs
  ├── HttpGitHubClient  ← GitHubClient trait (API calls)
  ├── FsWatchStateStore ← WatchStateStore trait (cache I/O)
  ├── TomlMatrixAppender ← MatrixAppender trait (matrix.toml editing)
  └── SystemGitOps      ← GitOps trait (git add/commit/push)
        │
        └── watch.rs: run_watch_cycle(ws, github, cache, appender, git_ops)
```

Tests substitute MockGitHub, MockCache, MockAppender, MockGitOps/RecordingGitOps.

## Concurrency

- Workspaces processed in parallel via `tokio::task::JoinSet`
- Within a workspace, repos are processed sequentially (GitHub API rate limits)
- Rate budget: ~160-240 API calls per cycle for 80 repos × 2-3 calls each
- With 5000 req/hr authenticated limit: supports ~20 cycles/hour

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
