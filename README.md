# tend

Workspace repository manager -- keeps your repos in sync.

## Overview

Tend discovers, clones, and tracks the status of repositories across GitHub organizations. It reads a YAML config file defining workspaces (org, base directory, clone method), discovers repos via the GitHub API, clones missing ones, and reports status (clean, dirty, missing, unknown). Integrates with direnv via `use_tend` for automatic sync on directory entry.

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
```

## Configuration

Default config path: `~/.config/tend/config.yaml`

```yaml
workspaces:
  - name: pleme-io
    provider: github
    base_dir: ~/code/github/pleme-io
    clone_method: ssh
    discover: true
    org: pleme-io
```

## Features

- GitHub org discovery (auto-discovers repos via API)
- SSH and HTTPS clone methods
- Discovery caching (skip API calls on repeat syncs)
- direnv integration (`use_tend` shell function)
- Colored status output (clean/dirty/missing/unknown)

## License

MIT
