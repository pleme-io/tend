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
