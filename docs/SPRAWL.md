# sprawl

`tend sprawl` keeps agents aware of branches, stashes, worktrees and backup
refs on every workspace repo. It only reports. Nothing here deletes or changes
a ref, and a suggested command is text.

`src/sprawl.rs` reads facts from each repo's last fetch, with no network and no
judgement. `blue/sprawl.b` judges them in a pure frame. `git` runs with
`GIT_OPTIONAL_LOCKS=0`, so a scan does not write to the repo it reads.

## Surfaces

| Surface | What it gives | Off switch |
|---|---|---|
| `tend sprawl [--json] [--org O] [--repo R] [--verdict V] [--limit N]` | per-class counts, then items with their next command; a first run scans | `sprawl.enabled` |
| `tend sprawl refresh` | a scan now; refused when host pressure says run nothing, narrowed when it says throttle | `sprawl.enabled` |
| `tend sprawl validate [--json]` | runs every rule's examples; exits non-zero on a wrong example or a refused entry | none |
| `tend_sprawl` MCP tool | the same report, read-only, from the snapshot; takes org, repo, verdict, limit | `sprawl.enabled` |
| `tend hook session-start` | one line per class with a count, never per branch | `sprawl.hook_session_start` |
| `tend hook stop` | a `systemMessage` per touched repo, once per session; it never blocks | `sprawl.hook_stop` |
| `tend status` | one summary line | `sprawl.status_line` |
| the daemon | refreshes the snapshot each cycle, with workers bounded by the pressure verdict | `sprawl.daemon` |

The snapshot is `<cache>/sprawl/snapshot.json`. A report carries `age_secs`,
`stale`, `stale_why`, and an `outcome`:

- `found`: at least one item matches.
- `empty`: every repo in scope was read and nothing matches. This is a finding.
- `blind`: no snapshot, or a repo in scope was not read. Conclude nothing.

An unread repo is `blind`, never zero. A repo past `sprawl.scan_minutes` is
blind too, and names why.

## Facts

For each repo, deduplicated across linked worktrees:

- Local and `origin` branches: tip date, author, upstream gone, ahead and
  behind the default branch, patch-unique commits
  (`rev-list --cherry-pick --right-only --no-merges`), merges ahead, and whether
  the three-dot diff is empty.
- Stashes: age, author, message.
- Linked worktrees: path exists, dirty, branch.
- `refs/backup/*`.
- The default branch, only when it differs from origin and has no merge base.

The three patch counts cost a process each, so they are kept per `(path, ref,
sha, base sha)` in the snapshot and read again only when a ref or the base
moves. Each repo gets `sprawl.deep_repo_seconds` for them. A count not read
stays unknown, is reported as `deep_unknown`, and is judged lingering, never
landed.

## Verdicts

The first rule that holds names the verdict. Order, with the operator's rules
first:

| Verdict | Severity | When |
|---|---|---|
| `default-branch-diverged` | alert | no merge base with origin: a rewritten origin |
| `dead-worktree` | warn | the worktree's path is gone |
| `dirty-worktree` | warn | the worktree has uncommitted paths |
| `idle-worktree` | info | any other linked worktree |
| `bulk-sweep-stash` | info | message holds `retire-spec-autostash-` or `claude-fleet-clean-` |
| `individual-stash` | warn | any other stash |
| `backup-ref` | info | any `refs/backup/*` |
| `foreign-author` | info | the tip's author is not the operator, or was not read |
| `checked-out` | info | a local branch some worktree has checked out |
| `active` | info | younger than 72 hours |
| `landed` | warn | nothing new against the base: ahead 0, an empty three-dot diff, or no unique patch and no merge |
| `lingering` | warn | any other branch |
| `unclassified` | warn | no rule held: the rules leave this item unseen, so it stays visible |

`checked-out` and `active` do not print in the session lines (`announce: false`).

**Unknown is never landed.** A clause on a field not read holds for nothing but
the op `unknown`. A branch of merges alone is not landed by the patch
comparison, since a merge can carry work no patch shows.

## House rule

Every akeylesslabs repo, and every branch by another author, is report-only.
`next` for such an item is a read-only look, whatever a rule says, so no rule
can suggest deleting it. `sprawl.report_only_orgs` adds orgs to the house list
and cannot empty it. `sprawl.own_authors` adds names and emails to the global
`git config` identity.

## Rules

Each rule is a `kousei` entry, on, tuned, off or refused alone. Operators extend
them from `tend.b`:

```text
use("kousei", %i[entry off tune])

def config()
  {
    sprawl: {
      rules: [
        off(:rule, "bulk-sweep-stash"),
        tune(:rule, "active", {scope: {max_age_hours: 24}}),
        entry(
          :rule,
          "wip-branch",
          {
            verdict: "wip-branch",
            kinds: ["remote-branch"],
            scope: {ref: ["origin/wip/*"], org: ["pleme-io"]},
            when: [["unique", "gt", 0]],
            severity: "warn",
            next: "git -C {path} log --oneline {base}..{ref}",
            examples: [
              {
                item: {kind: "remote-branch", ref: "origin/wip/x", unique: 2},
                expect: "wip-branch"
              }
            ]
          }
        )
      ]
    }
  }
end
```

| Field | Meaning |
|---|---|
| `verdict` | the class this rule names |
| `kinds` | any of `branch`, `remote-branch`, `stash`, `worktree`, `backup-ref`, `default-branch` |
| `when` | clauses, all must hold: `[field, op, value]`, op one of `eq ne lt le gt ge glob contains_any unknown`; always three long |
| `scope` | `org`, `repo`, `ref`, `author` (glob lists) and `min_age_hours`, `max_age_hours` |
| `severity` | `alert`, `warn` or `info` |
| `next` | the command as text; `{path} {repo} {ref} {branch} {base} {author}` fill in |
| `announce` | `false` keeps the class out of the session lines |
| `examples` | required: `{item:, expect:}`, run through the whole chain by `tend sprawl validate` |

An operator rule named like a built-in replaces it. `sprawl.rules` in YAML or
`tend.b` is the only configuration of a rule; it has no rendered copy.

## A stale snapshot

A snapshot past `sprawl.max_age_minutes` says so and asks the daemon. The daemon
writes `<cache>/daemon/heartbeat.json` at its pressure gate on each cycle, with
its pid and the verdict, and the hook checks the pid with `ps`:

| Probe | Sentence |
|---|---|
| the pid is a tend process | it runs; the state and reason of the last cycle; do not restart |
| the pid is gone | it is not running; the restart command |
| no heartbeat, or `ps` failed | its state is unseen, not dead; the command to check |

`tend hook session-start` uses the same probe for the status snapshot. It no
longer infers "probably not running" from the snapshot's age: on 2026-11-03 it
said that while the daemon ran and throttled by load. Silence is the other
failure: on 2026-10-22 the daemon had been unloaded since 06:54 and each session
start said only "no fresh daemon snapshot".
