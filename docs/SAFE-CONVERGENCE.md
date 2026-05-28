# Safe Convergence — typed DriftEvent + Reaction matrix

> **Status:** draft (2026-05-28). Authored after the 233-repo mid-rebase
> wedge incident (every pleme-io repo stuck on a substrate-bump rebase
> with flake.lock conflict, none auto-recovered). The design below
> answers the operator's directive *"design tend so it is always
> achieving state safely"* — typed primitives that turn every observed
> drift class into an auto-action OR a typed escalation, never silent
> failure or work loss.

This document complements
[`OPERATOR-DESIGN.md`](OPERATOR-DESIGN.md) (the K8s operator surface)
and [`ARCHITECTURE.md`](ARCHITECTURE.md) (the workspace shape). It
extends the existing M5 `DriftEvent` + M6 reaction loop with a
complete typed coverage of every drift class observed in the field.

---

## 1. Problem statement

Across ~775 pleme-io repos + akeyless org workspaces, `tend pull` /
`tend reconcile` today produces opaque "dirty skipped" / "pull failed"
buckets that require per-repo operator investigation. The 2026-05-28
incident exposed the failure mode:

- A fleet operation (substrate-bump) started rebases on 233 repos.
- Each rebase conflicted on `flake.lock` (single file, every repo).
- All 233 sat in stuck state until a single `git rebase --abort`
  swept them clean.
- During that wedge, `tend pull` reported "245 dirty skipped" with no
  signal that the dirty class was *uniformly recoverable*.

**Root cause:** tend treats every "dirty tree" as opaque. The operator
has no typed view of *why* a repo is dirty, and tend has no typed
action to take when the *why* is mechanical.

---

## 2. Typed DriftEvent enum

```rust
// tend-types/src/drift.rs
#[derive(Debug, Clone, Serialize, Deserialize, DeriveTataraDomain)]
#[tatara(keyword = "defdrift")]
pub enum DriftEvent {
    // ── Auto-recoverable ─────────────────────────────────────────────
    /// Local branch has no upstream tracking — auto-set from remote HEAD.
    NoUpstream { branch: String },

    /// Upstream renamed its default branch (master→main, etc).
    BranchRenamed { local: String, remote_head: String },

    /// Local is N commits behind origin — fast-forward applies.
    Behind { commits: u32 },

    /// Working tree has only flake.lock or lock-file-class changes.
    /// Safe to discard (we re-derive locks from flakes anyway).
    UncommittedLockfileOnly { paths: Vec<PathBuf> },

    /// In-progress rebase with conflicts ONLY in known lock/generated
    /// files (flake.lock, Cargo.nix, package-lock.json, …). The
    /// dominant 2026-05-28 wedge class.
    MidRebaseLockOnly {
        onto: ObjectId,
        pending: ObjectId,
        conflict_files: Vec<PathBuf>,
    },

    // ── Operator decision ───────────────────────────────────────────
    /// Local is ahead of origin — operator decides push vs reset.
    Ahead { commits: u32 },

    /// Diverged: ahead AND behind — operator decides merge/rebase/reset.
    DivergedHistory { ahead: u32, behind: u32 },

    /// Working tree has substantive uncommitted changes (not lock-only).
    UncommittedSubstantive { stats: DiffStat },

    /// In-progress rebase with conflicts in source files.
    MidRebaseSubstantive {
        onto: ObjectId,
        pending: ObjectId,
        conflict_files: Vec<PathBuf>,
    },

    /// In-progress merge or cherry-pick — operator started for a reason.
    MidMerge { merge_head: ObjectId, conflict_files: Vec<PathBuf> },
    MidCherryPick { commit: ObjectId },

    /// Stash present — preserve operator's WIP context.
    StashPresent { count: u32 },

    // ── Cannot self-heal ────────────────────────────────────────────
    /// Upstream returned 404 / removed / archived.
    UpstreamGone { last_known_url: String },

    /// Directory exists but is not a git repo (placeholder / stub).
    StubDirectory,

    /// `.git/` is corrupted (no HEAD, dangling refs, etc).
    GitFsCorruption { details: String },
}
```

Every variant is typed-emission-compliant: pattern-match exhaustiveness
prevents adding a new drift class without also adding its policy entry.

## 3. Typed Reaction matrix

```rust
#[derive(Debug, Clone, Serialize, Deserialize, DeriveTataraDomain)]
#[tatara(keyword = "defreaction")]
pub enum Reaction {
    /// Nothing to do — log and continue.
    NoOp,

    /// Auto-apply a typed remediation. Reflog-recoverable.
    AutoAct {
        action: RemediationAction,
        pre_state: ObjectId,         // for reflog recovery
        evidence: String,             // why this is safe
    },

    /// Surface to operator. Job ends in a typed "needs operator" phase.
    Escalate {
        reason: String,
        suggested_commands: Vec<String>,
    },

    /// Mark repo as orphaned in workspace config. Skip future ticks
    /// until operator un-quarantines.
    Quarantine { reason: String, marker_path: PathBuf },
}

pub enum RemediationAction {
    AbortRebase,
    AbortMerge,
    AbortCherryPick,
    SetUpstream { remote: String, branch: String },
    FastForward,
    Fetch,
    DiscardLockOnly { paths: Vec<PathBuf> },
    RenameBranch { from: String, to: String },
}
```

### Default policy

| DriftEvent | Default Reaction |
|---|---|
| `NoUpstream` | `AutoAct(SetUpstream)` — detect remote HEAD via `git remote show origin`, set tracking, retry pull |
| `BranchRenamed` | `AutoAct(RenameBranch + Fetch + FastForward)` |
| `Behind` | `AutoAct(FastForward)` |
| `UncommittedLockfileOnly` | `AutoAct(DiscardLockOnly)` — lockfiles are derived, never source-of-truth |
| `MidRebaseLockOnly` | `AutoAct(AbortRebase)` — the dominant 2026-05-28 wedge class |
| `Ahead` | `Escalate("Local commits not on origin — operator must push or drop")` |
| `DivergedHistory` | `Escalate("History diverged — operator must rebase/merge/reset")` |
| `UncommittedSubstantive` | `Escalate("WIP — operator must commit, stash, or discard")` |
| `MidRebaseSubstantive` | `Escalate("Source-file conflicts — operator resolves")` |
| `MidMerge`/`MidCherryPick` | `Escalate("Operator-initiated; preserve")` |
| `StashPresent` | `NoOp` — never touch stashes |
| `UpstreamGone` | `Quarantine("Upstream 404")` |
| `StubDirectory` | `Escalate("Mark with tend placeholder or rm -rf")` |
| `GitFsCorruption` | `Escalate("git fsck required")` |

## 4. Safety invariants (load-bearing)

These are the rules every auto-action MUST satisfy. Violating any one
makes the action `Escalate` instead.

1. **Never lose unpushed commits.** Auto-actions never `reset --hard`
   away commits that aren't on `origin/<remote_head>`.
2. **Never discard uncommitted changes that aren't lock-class.** The
   `DiscardLockOnly` action's classifier MUST refuse any file outside
   the typed `LOCKFILE_ALLOWLIST`
   (`flake.lock`, `Cargo.nix`, `package-lock.json`, `yarn.lock`,
   `Gemfile.lock`, `Pipfile.lock`, `poetry.lock`, `pnpm-lock.yaml`,
   `bun.lockb`, `composer.lock`, `go.sum`).
3. **Never touch stashes.** A stash is operator state; tend treats
   it as opaque.
4. **Reflog-recoverable.** Every `AutoAct` records the pre-action
   ObjectId in the audit log. `tend report` includes a one-liner
   recovery command per action.
5. **Pre-condition re-check at apply time.** Detection and action
   may be milliseconds apart, but git state can change. The action
   re-checks its pre-condition immediately before mutating;
   mismatch → abort the action, re-emit drift event next tick.
6. **Idempotent.** Applying the same auto-action twice MUST produce
   the same state (or noop on the second pass). The detect→react
   loop runs continuously; non-idempotent actions cause oscillation.
7. **Per-repo policy override.** Workspace config can set
   `auto_recover_policy: { repo_name: strict | default | aggressive | off }`.
   `strict` = `Escalate` even on lock-only classes. `off` = full
   skip (no detection either).
8. **New drift classes default to `Escalate`.** Adding a `DriftEvent`
   variant without a matching policy entry compiles BUT yields
   `Escalate("unclassified drift")` at runtime — never silent auto-action.

## 5. Implementation milestones (Viggy Method)

| M | Scope | Notes |
|---|---|---|
| M1 | `tend-types/src/drift.rs` — typed enums + `DeriveTataraDomain` | Lisp authoring of policies via `(defreaction …)` |
| M2 | `tend-detector` crate — `DriftDetector` trait + impls (HeadCheck, MergeStateCheck, TrackingCheck, RebaseStateCheck, LockfileClassifier) | Pure functions; each impl independently testable |
| M3 | `tend-reactor` crate — `ReactionPolicy` trait + default policy matrix + shikumi-tiered config override | New tlisp surface: `(defreaction-policy …)` per workspace |
| M4 | Wire into `tend reconcile` — typed `ConvergenceReceipt` replaces the opaque "dirty skipped" bucket | One Job per repo; output sink captures `(DriftEvent, Reaction, Outcome)` triples |
| M5 | `tend daemon` — auto-recover on detection; `Escalate` reactions emit ntfy alerts via the existing observability path | Continuous convergence (Viggy Method peer of pangea-operator) |
| M6 | `tend report --convergence` — operator-facing rollup grouped by reaction class | Lets operator see "X repos auto-recovered, Y waiting on decision, Z quarantined" |

## 6. Composition with existing primitives

- **shigoto** — each `(DriftEvent, Reaction)` pair becomes a typed
  `DriftReactionJob` in the scheduler. Audit log already exists.
- **shikumi** — `auto_recover_policy` per workspace lives in
  `~/.config/tend/config.yaml` under `[workspaces.<name>.recovery]`.
- **promessa / Viggy** — the lattice expression *"every clean repo is
  fast-forwarded to its origin HEAD"* becomes a typed promessa
  reconciled tick-by-tick. OutcomeChain attests each convergence.
- **pleme-io-github-posture** — `UpstreamGone` events feed back into
  the org's repo catalog as "repo deleted upstream — remove from
  workspace?" prompts.

## 7. The 2026-05-28 incident, replayed under this design

| Stage | Pre-design behavior | With Safe Convergence |
|---|---|---|
| Operator runs `tend pull` | "245 dirty skipped" — opaque | "233 `MidRebaseLockOnly` events → 233 `AutoAct(AbortRebase)` applied" |
| Resolution time | Manual investigation + mass-abort script | 0 — tend daemon auto-applies within one tick |
| Audit trail | shell history + reflog | typed `ConvergenceReceipt` per repo + OutcomeChain attestation |
| Operator awareness | "lots of dirty repos" | "fleet auto-recovered from substrate-bump wedge" notification |
| Recovery if wrong | Per-repo `git reflog` archaeology | One typed command: `tend undo --action <action-id>` |

## 8. Anti-patterns this design forbids

- **Opaque "dirty" buckets.** Every dirty-class repo has a typed
  drift event; no bucket exists for "unclassified dirty."
- **`git rebase --abort` in shell glue.** Mass-actions go through the
  typed `RemediationAction` enum, never ad-hoc shell.
- **Silent skip.** `Escalate` always produces an operator-visible
  signal (status job phase + audit-log entry + optional ntfy).
- **Auto-actions without reflog recording.** Every `AutoAct` writes
  pre/post ObjectIds; "what did tend just do?" is one query away.

## 9. Open questions

1. **`UncommittedLockfileOnly` policy default** — auto-discard or
   escalate? Discard is the right answer when the lockfile is
   derived (Nix/Cargo), but some operators commit lock content
   intentionally (npm, Python). Per-workspace override resolves;
   default could be `Escalate` until operator opts in.

2. **Cross-repo concurrent operations** — if a fleet operation
   (substrate bump) starts rebases on 100+ repos and tend daemon
   reacts mid-operation, do we race the orchestrator? Resolution:
   workspace-level `lockfile` (in `~/.config/tend/state/`) that
   declared fleet operations grab, signaling tend to pause
   detection until the operation completes (or times out).

3. **Quarantine TTL** — `UpstreamGone` quarantine should auto-clear
   if upstream comes back. Re-detection on each tick handles this;
   no special TTL needed.

## 10. References

- 2026-05-28 incident memory: `incident_pleme_io_mass_rebase_wedge_2026_05_28.md`
- Existing M5/M6 work: `src/jobs/reactions.rs` (already ships typed
  reactions for `PullFailed+no-such-ref → FetchRepoJob`)
- Viggy Method: `pleme-io/theory/CONTINUOUS-SOLUTION-MACHINE.md`
- shigoto: `pleme-io/theory/SHIGOTO.md`
