# wip branches

`src/wip.rs` reads the `wip/<domain-slug>` branches from each repo's last
fetch. `blue/wip.b` judges each one: active, lingering or landed. The `wip:`
config section sets the thresholds.

| Surface | What it shows |
|---|---|
| `tend status` | after the repo table, the `wip/*` branches on origin (`wip:` lines) |
| `tend hook session-start` | a warning for each lingering `wip/*` branch; it never blocks |
| `tend_wip` MCP tool | the same verdicts, typed |
