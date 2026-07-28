# tend — the reconciler vocabulary (DESIGN)

> **Tier-honest.** Everything below is **DESIGN**. No code in this document is
> written. Where a technique is named, its honest tier is stated: *truly-
> unrepresentable* (compile error / absent path), *parse-time-rejected*
> (`Err` at a boundary + sealed construction), or *only-mitigated* (a runtime
> check). A mitigation labelled a proof is a leak.

## Gate 0 — the illegal states, measured

Every row was observed live on cid, 2026-07-28. Nothing here is hypothesised.

| # | Illegal state | Evidence |
|---|---|---|
| G1 | A workspace ROOT is itself a repo | `~/code/github/akeylesslabs/.git` exists beside 125 repo dirs |
| G2 | Persistent non-zero residue is the steady state | ~125 failures/tick, forever, nothing escalates |
| G3 | An auth method whose FAILURE destroys the credential | 193,402 `could not read Username` |
| G4 | Unbounded concurrency against a capped shared transport | 17,768 `Session open refused by peer` |
| G5 | A memo whose TTL is shorter than its consumer's poll | `head_cache` TTL 60s vs `--interval 300` |
| G6 | An inert knob that reads live | `flake_refresh.interval` under `enable: false` |
| G7 | Fixed cadence against a shared external budget | ~13,900 `git fetch`/hr, not budget-aware |
| G8 | The diagnostic exists and nobody reads it | 123 MB `tend-daemon.err`, rotation configured, unread |

### The insight: G3 and G4 are ONE shape

They look like a credentials bug and a networking bug. They are the same bug.

```
  G3   N concurrent fetches ─┬─► ~/.config/git/credentials   (one file;
                             │                                any failure ERASES it)
  G4   N concurrent fetches ─┴─► /tmp/ssh-control-%r@%h:%p   (one connection;
                                                              capped sessions)
```

**An unbounded number of concurrent consumers against a single shared,
destructible cell.** That is `eliminate-the-shared-cell` from the
UNREPRESENTABILITY catalog — the same root ECLUSA names for IaC state, one layer
down at the transport. It is why the failures are *self-sustaining* rather than
transient: each failure damages the cell the next consumer needs.

99% of tend's failures are this one shape. Fix the shape, not the two symptoms.

## The vocabulary

Three closed axes and one total product. Reuse-first: every engine below already
exists in the fleet and is CONSUMED, never re-rolled.

### Axis 1 — `Transport` (replaces the bare `CloneMethod`)

`CloneMethod` (config.rs:516) already names ssh/https. It does not carry the
consequence, which is the whole point:

```
Transport =
  | Ssh   { mux: MuxDiscipline }
  | Https { credential: cofre::SecretRef }
```

- `MuxDiscipline = Shared { max_sessions } | PerWorker | Disabled` — makes G4's
  cap a declared quantity instead of an ambient `~/.ssh/config` default that no
  consumer can see.
- `Https` carries a **`cofre::SecretRef`**, not a path — the fleet's existing
  secret vocabulary. A `SecretRef` is resolved per use; there is no file for a
  failure to erase. **G3 becomes truly-unrepresentable at the type level: the
  destructible cell is absent, not guarded.**
  *Honest caveat:* this holds only if the resolved credential never lands in a
  `credential.helper=store` file. If tend keeps shelling to `git` with an
  ambient helper, G3 is merely *only-mitigated*. The sealed form requires
  `GIT_ASKPASS`/`-c credential.helper=` injection per invocation.

### Axis 2 — `FailureCause` (replaces `Failed { stderr: String }`)

`FetchOutcome::Failed { stderr: String }` and `SyncOutcome::Failed { stderr }`
are **stringly-typed discriminants** — the exact anti-pattern dojo's `Evidence`
sum was built to refuse. The classification exists today only in my `grep`.

```
FailureCause =
  | TransportSaturated { transport: Transport }   // G4
  | CredentialAbsent   { r: cofre::SecretRef }    // G3
  | CredentialRejected { r: cofre::SecretRef }    // SSO / scope — DISTINCT from absent
  | UpstreamMissing                               // 404 / renamed / deleted
  | BudgetExhausted                               // G7, from samba
  | Network { kind: NetworkKind }                 // tls / dns / proxy
```

The split that matters: **`CredentialAbsent` vs `CredentialRejected`.** Absent is
self-inflicted (G3's cascade) and auto-recoverable; Rejected is an SSO/scope fact
only a human can grant. Today both are the same `String` and are therefore
indistinguishable — which is precisely why "why do 88 of 125 fail?" was
unanswerable without reading a 123 MB file.

Exhaustive `match` on this enum is what makes G8 structural: a cause with no
route is a **compile error**, not a line in an unread log.

### Axis 3 — `Residue` (the operator's predicate, as a type)

> "those tend numbers should be 100% if tend is working correctly BY DEFINITION"

```
Residue = Clean | Dirty { causes: NonEmpty<(RepoId, FailureCause)> }
```

`Clean` is the ONLY resting state. `Dirty` carries a **`NonEmpty`** — a residue
that cannot name a cause is unconstructible, which kills the "125 failed" line
that explains nothing. This is AUTOREVIVY §XI's zero-residue rule given a type.

### The product

```
(defworkspace
  :name          akeylesslabs
  :root          (WorkspaceRoot "~/code/github/akeylesslabs")   ; G1
  :transport     (Https :credential (secret-ref "github/classic"))
  :discovery     (Discover :org "akeylesslabs")
  :budget        (LeakyBucket :quota-pct 20)                    ; G7, samba
  :cadence       (Adaptive :floor 300 :ceiling 3600)
  :memo          (Ttl :secs 3600))                              ; G5
```

`WorkspaceRoot` is a **parse-don't-validate newtype**: its only constructor
rejects a path containing `.git`. G1 becomes *parse-time-rejected* — a workspace
root that is a repo cannot be constructed. (Not truly-unrepresentable: the
filesystem can grow a `.git` after construction, so a reconciler tick must
re-check. Say so; do not round up.)

`:memo (Ttl …)` is validated against `:cadence` at construction — **G5's
TTL-shorter-than-poll becomes parse-time-rejected**, closing the vacuous-cache
class rather than re-tuning one number.

`:budget` is a **samba `LeakyBucket`** — the fleet's shipped rate-limited-consumer
pattern, which tend is a textbook consumer of and does not use. G7.

## Tier ledger

| # | Technique | Honest tier |
|---|---|---|
| G1 | `WorkspaceRoot` newtype, `.git`-rejecting ctor | parse-time-rejected |
| G2 | `Residue::Dirty{NonEmpty}` + exhaustive route | parse-time-rejected |
| G3 | `SecretRef` — remove the file (no cell to erase) | truly-unrepresentable **iff** no ambient helper; else only-mitigated |
| G4 | `MuxDiscipline` declared + bounded worker pool | only-mitigated (a runtime cap; ssh's limit is external) |
| G5 | TTL-vs-cadence checked in the ctor | parse-time-rejected |
| G6 | shikumi `TieredConfig` + typed `Provenance` | eval/parse-caught (an inert knob reports its tier) |
| G7 | samba `LeakyBucket` | only-mitigated (C5 ceiling: external I/O) |
| G8 | Exhaustive `match` on `FailureCause` | truly-unrepresentable (unrouted cause = compile error) |

Three of eight reach parse-time-rejected, one truly-unrepresentable, one
conditionally so, three only-mitigated with named ceilings. **That is the honest
score. Do not round it up.**

## Phased plan

- **M0 — stop the bleeding, no new types.** `clone_method: https → ssh` for the
  two akeyless workspaces (removes 91% of failures by removing the cell from the
  path); bound fetch concurrency below the ssh `MaxSessions` cap; raise
  `head_cache` TTL above the daemon interval. Config only.
- **M1 — `FailureCause` + `Residue`.** The two `Failed{stderr}` variants become
  typed sums; the aggregate log line carries causes. Mock-green against the
  existing `Environment` seam. **This is the milestone that answers "why".**
- **M2 — `Transport` + `SecretRef`.** Credential per-invocation; the shared file
  leaves the path. Requires the `GIT_ASKPASS` injection noted above or the tier
  claim is false.
- **M3 — `WorkspaceRoot`, budget, cadence.** samba `LeakyBucket`; adaptive
  cadence; G1's newtype.
- **M4 — `(defworkspace …)` + `#[derive(DeriveTataraDomain)]`.** The Lisp surface.
  Until it lands, this document is the *destination* form, not the wired one.

## What this consumes, and never re-rolls

shigoto (`Dag`/`Job`/`RecordingJob`/gates — tend is already native; the new
causes become typed job outcomes) · samba `LeakyBucket` · shikumi `TieredConfig`
+ `Provenance` · cofre `SecretRef` · dojo's `Evidence` discipline (no stringly-
typed discriminants) · AUTOREVIVY §XI (zero-residue-or-be-loud) ·
UNREPRESENTABILITY's eliminate-the-shared-cell.

No new job system. No second cache primitive. No new secret vocabulary.
