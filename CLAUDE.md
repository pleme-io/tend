# tend

Workspace repository manager — keeps repos in sync, detects upstream changes,
and automates version certification pipelines.

## Commands

| Command | Purpose |
|---------|---------|
| `sync` | Clone missing repos |
| `status` | Show repo status (clean/dirty/missing/unknown) |
| `list` | List configured repos |
| `discover` | Discover repos from a GitHub org |
| `watch` | Run watch cycle once (detect new versions) |
| `daemon` | Persistent loop: sync + fetch + watch (300s default) |
| `flake-update` | Propagate nix flake updates through dependency chain |
| `init` | Generate starter config |

## Architecture

```
src/
├── main.rs          # clap CLI dispatch (9 subcommands)
├── config.rs        # YAML config types (Workspace, WatchConfig, CloneMethod)
├── provider.rs      # GitHub API: discovery, HEAD, tags, language detection
├── sync.rs          # Repo resolution, cloning, status, fetching
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

## Testing

15 tests. Run: `cargo test`
