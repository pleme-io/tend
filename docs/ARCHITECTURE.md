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
