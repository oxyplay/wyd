# Wyd Runtime Ownership — Core Technical Plan

## Goal

Wyd remains a local TUI/CLI for development runtime, but its core abstraction changes from:

```text
process → category → leftover
```

to:

```text
runtime session
    ↓
owned resources
    ↓
lifecycle
```

Primary invariant:

> Wyd must deterministically identify which runtime resources originated from which coding-agent runtime session and retain that provenance after the owning process exits.

No daemon, MCP, web, Fleet or vendor protocol is part of this core plan.

## Core thesis

> Wyd records exact runtime provenance while it is observable, persists it across process and Wyd lifetimes, and uses a deterministic resolver only when exact ownership is unavailable.

The design principle, strongest to weakest:

```text
exact first → durable → heuristic only for gaps
```

Wyd first learns to reliably remember what it exactly observed. Only then does it infer what it can no longer directly observe. Heuristic attribution never replaces recorded provenance; it only fills gaps.

## Phase boundaries

```text
PHASE 1  ✅ implemented (steps 1–8)
identity
session
exact ownership
resource continuity
durability
restore

↓ only when this works

PHASE 2 CORE
resolver
candidate sets
scores
ambiguity
propagation

↓ much later

PHASE 3
daemon
MCP
ACP
web
vendor protocol
Fleet
```

This document is the contract for PHASE 1. It is frozen until the first eight implementation steps complete.

---

# 1. Preserve Current Leftover Logic First

Do not replace the existing `classify::leftovers` logic initially.

Current:

```text
processes
    ↓
group()
    ↓
attach()
    ↓
mark()
    ↓
Active / Persistent / Suspicious
```

remains operational.

Add session ownership alongside it.

Initial ownership is derived only from facts already considered reliable:

```text
agent process
    ↓ direct/observed process ancestry
child runtime
```

Example:

```text
Claude
└── node
    └── Vite
```

becomes:

```text
RuntimeSession(Claude)
└── owns Vite
```

No heuristic resolver is required for this first increment.

Existing leftover tests remain the regression suite.

---

# 2. Process Identity

A process identity is valid only when `start_time != 0`.

```rust
pub struct ProcessIdentity {
    pub boot_id: BootId,
    pub pid: u32,
    pub start_time: u64,
}
```

`pid` alone is never an identity.

`boot_id` distinguishes identical PID/start-time combinations across machine boots.

The only constructor from a live process:

```rust
impl ProcessIdentity {
    pub fn from_process(
        boot_id: &BootId,
        process: &ProcessInfo,
    ) -> Option<ProcessIdentity> {
        (process.start_time != 0).then(|| Self {
            boot_id: boot_id.clone(),
            pid: process.pid,
            start_time: process.start_time,
        })
    }
}
```

`start_time == 0` yields `None`.

## Degradation rule

When `start_time == 0`, a process:

```text
participates in current group()/mark() as today
may be displayed in the TUI
may participate in the current process tree
cannot be a persistent session root
receives no durable ownership identity
does not match SQLite after a Wyd restart
```

That is, it is never treated as:

```text
PID 1234 → probably the same process
```

after a Wyd restart. Instead:

```text
identity unavailable
→ live-only attribution
```

## Start-time change between scans

If the same PID shows:

```text
start_time 1000 → 2000
```

then:

```text
old process disappeared
new process appeared
```

If it shows:

```text
1000 → 0
```

or:

```text
0 → 1000
```

identity is not bridged between them automatically.

A safe false-negative is better than a wrong persisted owner.

---

# 3. BootId as Platform Primitive

```rust
pub trait BootIdentityProvider {
    fn current_boot_epoch(&self) -> Result<BootEpoch>;
}
```

The provider returns a platform boot fingerprint. It does not persist; mapping the fingerprint to a stable `BootId` is the resolver's job (so the persistence backend plugs in at step 7 without touching platform code).

## Linux

`/proc/sys/kernel/random/boot_id` is used directly. It is a per-boot UUID, stable for the boot, and collision-safe — it can become the `BootId` unchanged.

## macOS

`sysctl kern.boottime` is used as a boot epoch fingerprint, **not** as a `BootId`.

Algorithm:

```text
read kern.boottime
        │
        ▼
compare with last_boot_epoch in SQLite
        │
   same │ different
        │
 reuse stored       generate random UUID
 BootId             and persist mapping
```

SQLite stores:

```text
platform_boot_epoch = sec/usec
boot_id = random UUID
```

After a reboot, `kern.boottime` changed → a new `BootId`.

Thus a collision of the timestamp itself never becomes a `ProcessIdentity` collision.

---

# 4. Runtime Session Identity

A Wyd session is explicitly defined as:

> **one observed invocation of an agent runtime process**

It is not a chat, conversation, task or vendor session.

Restarting an agent process creates a new Wyd runtime session.

## Runtime session

```rust
pub struct RuntimeSession {
    pub id: RuntimeSessionId,
    pub agent: ToolId,
    pub root: ProcessIdentity,
    pub project: Option<Project>,
    pub started_at: u64,
    pub last_seen_at: u64,
    pub ended_at: Option<u64>,
}
```

Session ID:

```text
hash(
    boot_id,
    root.pid,
    root.start_time,
    agent_tool
)
```

Project is metadata and is not part of identity.

An agent process changing cwd does not create a new session.

## Creation

A session is created only from an agent `RuntimeItem` with a valid root identity.

An agent with `start_time == 0` continues to display as before, but no session exists for it.

## Start

When an agent detector sees a process identity not present in the session store:

```text
SessionStarted
```

with:

```text
started_at = process.start_time
```

## End

When the exact root `ProcessIdentity` disappears:

```text
SessionEnded
```

`ended_at` is the first observation time where the process is absent.

It is therefore an observed end time, not necessarily the exact OS exit timestamp.

## Restart

```text
Claude PID 100 / start 1000
```

and later:

```text
Claude PID 100 / start 5000
```

are two different runtime sessions.

If vendors later expose their own stable task/session IDs, those may be stored as aliases or grouping metadata. They do not redefine Wyd's local runtime identity.

---

# 5. Resource Identity Is a RuntimeItem

`RuntimeItem` is the runtime resource. Do not build a second logical-resource tree.

Pipeline:

```text
ProcessInfo[]
    ↓
group()
    ↓
RuntimeItem tree
    ↓
session ownership
    ↓
leftover classification
```

`group()` remains the source of logical resources. No second grouping engine.

Example:

```text
RuntimeItem
category = MCP
display_name = playwright
process_ids = [100, 101, 102]
root_pid = 100
children = [
    RuntimeItem Chromium ×3
]
```

A persisted resource corresponds to a logical `RuntimeItem`, not to every line of `ps`.

## Resource identity is fixed at first observation

For a process-backed resource, durable identity is based on the identity of its root process:

```rust
enum ResourceIdentity {
    ProcessGroup {
        root: ProcessIdentity,
    },
}
```

Member processes are stored as observations:

```text
resource
├ root ProcessIdentity
└ member ProcessIdentity[]
```

**Resource identity is fixed when first observed.** A `RuntimeItem` becomes a historical resource with an immutable `resource_id` and the original root `ProcessIdentity`. If the root dies, the resource is **not** recreated. Changing the number of worker processes inside one grouped runtime does not create a new logical resource while the root identity stays the same.

This matters especially for:

```text
MCP
Chromium trees
language servers
worker groups
```

Docker stays a backend-specific resource identity (`container ID`), and does not pretend to be a Unix process identity.

## Matching a live item to a persisted resource

Order of matching a freshly grouped `RuntimeItem` against the store:

```text
1. root ProcessIdentity exact match
        ↓ yes
   existing resource

2. any current member ProcessIdentity matches
   a persisted member of a surviving resource
   (only where the resource's root has disappeared)
        ↓ yes
   existing resource

3. otherwise
   new resource
```

Step 2 applies only to an already-known resource whose root is gone. Member identity is never used to turn an initially non-durable resource into a durable one.

If the root had `start_time == 0`, the entire resource stays live-only.

## Exact ownership side-table

Attach exact ownership without polluting the existing model before it stabilizes:

```rust
struct ExactOwnership {
    resource: ResourceId,
    session: RuntimeSessionId,
    evidence: Vec<ExactEvidence>,
}
```

An existing `Agent RuntimeItem` tree:

```text
Agent RuntimeItem
└ MCP
  └ Chromium
```

becomes exact ownership (no resolver involved — ancestry was really observed):

```rust
RuntimeSession
├ owns MCP
└ owns Chromium
```

---

# 6. Ownership Is Not Probability

Do not call the resolver output a probability.

Use:

```text
AttributionScore
```

not:

```text
97% confidence
```

The score is a deterministic ranking produced from a fixed rule table.

Its purposes are:

```text
candidate selection
ambiguity detection
explanation
regression testing
```

It does not mean:

```text
93 = 93% statistically likely
```

---

# 7. Candidate Ownership Requires an Anchor

Project and timing information may support attribution but must never create ownership by themselves.

A session becomes an ownership candidate only through an anchor.

Initial anchors:

```text
Explicit registration             100
Direct child of session root       85
Observed descendant of root        75
Persisted previous ownership       previous score
Owned-parent propagation           derived from parent
```

`Explicit registration` is reserved for future integrations.

The initial implementation uses the other anchors.

No:

```text
same repo
+
started recently
=
owner
```

logic is allowed.

---

# 8. Supporting Evidence

After an anchor exists, supporting evidence modifies its score.

Evidence families are capped independently so correlated facts are not counted repeatedly.

## Project family

```text
exact cwd match                +10
same git root                   +8

family cap                     +10
```

Exact cwd and git root do not produce `+18`.

## Temporal family

```text
started <= 10s after anchor     +5
started <= 60s                  +3
started <= 5m                   +1

family cap                      +5
```

## Tool relationship family

```text
known agent → MCP relation      +5
known MCP → browser relation    +5
known launcher/wrapper chain    +5

family cap                      +5
```

Score:

```text
score =
    min(
        100,
        anchor
        + project_support
        + temporal_support
        + relationship_support
    )
```

---

# 9. Hard Contradictions

Some facts do not subtract points. They reject a candidate.

Examples:

```text
resource started before session
```

cannot mean:

```text
session spawned resource
```

Therefore the ownership candidate is rejected.

Likewise:

```text
observed ancestry reaches another agent session
```

rejects the candidate.

Known persistent OS services are not session-owned merely because they share a project or port.

Supporting evidence cannot override a contradiction.

---

# 10. Resolver Threshold

After scoring all valid candidates:

```text
top score >= 75
AND
top score - second score >= 15
```

assigns ownership.

Otherwise:

```text
owner = ambiguous
```

or:

```text
owner = unknown
```

Example:

```text
Claude    91
Codex     no anchor
```

→ Claude owns resource.

Example:

```text
Claude    88
Codex     82
```

→ ambiguous.

Never select a weak winner simply because it ranked first.

---

# 11. Ownership Propagation

Ownership can propagate through already attributed runtime nodes.

Example:

```text
Claude
└── Playwright MCP
    └── Chromium
```

If:

```text
Claude → MCP = 95
```

and Chromium is a direct child of that MCP:

```text
MCP → Chromium = direct observed relationship
```

Chromium inherits the session candidate through the owned parent.

The propagated score cannot exceed the parent ownership score.

```text
Claude → MCP        95
MCP → Chromium      strong
```

results in:

```text
Claude → Chromium <= 95
```

This prevents downstream nodes becoming "more certain" than the chain that established their ownership.

---

# 12. Versioned Resolver Rules

All rule numbers live in one module, not scattered through code:

```rust
// src/classify/ownership/rules.rs
pub const RESOLVER_VERSION: u32 = 1;

pub struct ResolverRules {
    pub descendant_anchor: u8,
    pub direct_parent_anchor: u8,

    pub exact_cwd: u8,
    pub same_git_root: u8,
    pub project_cap: u8,

    pub temporal_10s: u8,
    pub temporal_60s: u8,
    pub temporal_5m: u8,
    pub temporal_cap: u8,

    pub relationship_support: u8,
    pub relationship_cap: u8,

    pub ownership_threshold: u8,
    pub ambiguity_margin: u8,
}
```

With a comment directly in the code:

> These are deterministic v1 heuristic weights, not statistically calibrated probabilities.

Fixtures freeze v1 behavior.

Changing `75 → 80` is a deliberate change to the resolver contract.

Every persisted decision stores `resolver_version`, so an old `score = 95` remains reproducible after the rule table changes.

---

# 13. Durability

Runtime ownership must survive Wyd restarts.

Therefore lifecycle state is persisted from the start of session/lifecycle work.

Use local SQLite. No daemon is required.

Suggested platform state location:

```text
Linux:
$XDG_DATA_HOME/wyd/state.db

macOS:
~/Library/Application Support/wyd/state.db
```

A process-backed resource is identified by:

```text
boot_id + pid + start_time
```

not PID.

## Persistence schema (PHASE 2 full)

```sql
boots
  boot_id
  platform_epoch
  first_seen
  last_seen

sessions
  session_id
  boot_id
  agent
  root_pid
  root_start_time
  project
  started_at
  last_seen_at
  ended_at

resources
  resource_id
  kind
  root_boot_id
  root_pid
  root_start_time
  first_seen_at
  last_seen_at
  stopped_at

resource_members
  resource_id
  boot_id
  pid
  start_time

attribution_decisions
  decision_id
  resource_id
  observed_at
  resolver_version
  verdict
  winner_session_id

attribution_candidates
  decision_id
  session_id
  anchor_kind
  anchor_score
  project_score
  temporal_score
  relationship_score
  total_score
  rejected_reason

evidence
  candidate_id
  kind
  value

events
  ...
```

## Durability levels

Split cleanly by lifetime:

```text
DURABLE (long, while a live resource references them):
  session
  resource origin
  final attribution decision

BEST-EFFORT (may expire by retention):
  raw evidence
  detailed lifecycle events
```

A previously observed strong ownership relation is not recomputed from a degraded future process tree.

At T1:

```text
Claude → MCP → Chromium
score 95
```

At T2:

```text
Claude gone
MCP gone
Chromium re-parented
```

The historical edge remains:

```text
Chromium originated from S1
```

Current state changes lifecycle classification, not origin.

Distinguish:

```text
origin_session
```

from:

```text
current_parent
```

They are different concepts.

## Store matching is always strict

The first condition of any persisted lookup:

```text
boot_id matches
AND pid matches
AND start_time matches
```

A `start_time` mismatch means a different entity — no fallback by PID.

## GC / retention

Store has automatic GC, not a user-facing `wyd prune` (which is already Docker volumes).

Config:

```toml
[history]
retention_days = 30
```

Deletion rule is not simply `ended_at < now - 30 days` — that would delete provenance for a still-live orphan.

Rule:

```text
ended session older than retention
AND
no currently live resource references it
→ eligible for GC
```

Raw evidence and detailed lifecycle events may be deleted more aggressively, after the final state of session/resource is saved.

But:

```text
session
resource origin
final attribution decision
```

must be kept while a live descendant/resource references them.

On startup or periodically:

```rust
store.gc(now, retention_policy)
```

Use incremental vacuum, not a full `VACUUM` every run.

Because raw evidence may expire, `wyd why` shows the historical decision and honestly reports:

```text
raw evidence expired
```

rather than trying to reconstruct missing data.

---

# 14. Store Stores Evidence and Decision Separately

Store both:

```text
facts / evidence
```

and:

```text
computed components + resolver_version
```

They are two different tasks:

```text
Evidence  → what Wyd observed?
Decision  → what resolver v1 concluded from it?
```

Example:

```text
Evidence:
  Ancestor(agent = S1)
  CwdMatch
  DeltaStart = 7s

Decision v1:
  75 + 10 + 5 = 90
```

After resolver v2 ships, historical `wyd why` still shows:

```text
Decision made by resolver v1
```

A separate `re-evaluate with current resolver` may exist, but a historical decision is never silently rewritten.

---

# 15. Why Persistence Matters (Restore Scenario)

Initial state:

```text
Claude S1
└── Playwright
    └── Chromium PID 9000
```

Wyd persists:

```text
Chromium ProcessIdentity
→ owned by S1
→ score 95
→ evidence [...]
```

Claude exits. Chromium survives and is re-parented. Wyd exits too.

Later Wyd starts again.

Current OS tree only contains:

```text
launchd
└── Chromium PID 9000
```

Current ancestry cannot recover Claude.

But process identity is still:

```text
boot-A
PID 9000
start 12345
```

The store contains that exact resource identity.

Wyd restores:

```text
Chromium
→ previous owner S1
→ S1 ended
```

Therefore:

```text
provenance survives process ancestry
AND
provenance survives Wyd restart
```

This is required behavior.

---

# 16. Persist Candidate Set, Not Only the Winner

For each attribution decision persist the full candidate set:

```rust
AttributionDecision {
    resource,
    observed_at,
    resolver_version,
    verdict: Owned | Ambiguous | Unknown,
    winner_session: Option<SessionId>,
}
```

plus the complete candidate list:

```rust
AttributionCandidate {
    session,
    anchor,
    anchor_score,
    project_support,
    temporal_support,
    relationship_support,
    total,
    rejection: Option<Rejection>,
}
```

plus evidence.

Example:

```text
resource: vite-4120

resolver: v1
verdict: ambiguous

candidates:

Claude S1
  descendant       75
  cwd              +10
  temporal          +3
  total             88

Codex S2
  descendant       75
  git root          +8
  total             83

margin = 5
required = 15

→ ambiguous
```

After a restart, `wyd why 4120` reconstructs the verdict that was actually decided, rather than guessing it again from a degraded live state.

---

# 17. Session-Aware Leftovers

Only after deterministic session ownership exists, layer it into the current leftover classifier.

Current heuristic remains fallback.

New strong rule:

```text
resource has origin_session
AND
origin_session ended
AND
resource still exists
AND
resource is not persistent
```

→ session-ended leftover candidate.

Resource-specific policy remains.

Example:

```text
ended session + detached Chromium
→ strong leftover

ended session + MCP
→ strong leftover

ended session + Vite
→ suspicious, possibly intentionally retained

ended session + system Postgres
→ persistent
```

Existing `Suspicion.score` remains a leftover-risk score.

It must not become or reuse `AttributionScore`.

---

# 18. Lifecycle Events

Snapshots produce state transitions:

```text
SessionStarted
SessionEnded

ResourceStarted
ResourceStopped

OwnershipEstablished
OwnershipBecameAmbiguous

OwnerSessionEnded
ResourceSurvivedSession
```

Events are written only on transition.

Do not persist complete snapshots every scan.

The current snapshot remains the live view. SQLite stores durable identity/provenance/lifecycle transitions.

---

# 19. Explanation Is Resolver Output

Every ownership decision is reproducible:

```text
wyd why <pid>
```

Example:

```text
vite PID 4120

origin session:
  Claude S1

attribution score:
  95

anchor:
  observed descendant       75

support:
  exact cwd                 +10
  started within 10s         +5
  known dev-server chain     +5

total:
  95
```

No explanation may contain a score that cannot be reconstructed from persisted/current evidence.

When raw evidence has expired, the explanation reports the durable decision and notes the expired evidence.

---

# 20. Test Contract

Resolver tests operate on fixtures containing:

```text
process identity
ppid
start time
command
executable
cwd
tty
ports
Docker metadata
previous persisted ownership
```

Do not include evidence Wyd does not actually collect.

Required fixtures:

```text
direct child
multi-hop descendant
MCP → Chromium
Docker spawned by agent
agent-created database

two agents different projects
two agents same project

resource predates session
manual dev server
persistent service

re-parented child
Wyd restart with surviving child
PID reuse
machine reboot / PID reuse

ambiguous candidates
```

Every fixture asserts:

```text
candidate set
anchor
supporting evidence
score
selected owner
ambiguity
lifecycle state
```

not only the final display result.

---

# 21. Phase-1 Implementation Order

1. Add `BootId`, `BootEpoch`, platform `current_boot_epoch` (Linux + macOS) and the epoch→`BootId` resolver seam.
2. Add `ProcessIdentity` with `from_process` and the `start_time == 0` degradation rule.
3. Add `RuntimeSession` identity. Do not touch the TUI. A session is created only from an agent `RuntimeItem` with a valid root identity.
4. Attach exact ownership over the existing `RuntimeItem` tree via an `ExactOwnership` side-table. `group()` remains the source of logical resources.
5. Add `ResourceIdentity` (root fixed at first observation) with the strict matching order and live-only fallback for `start_time == 0` roots.
6. Add regression fixtures around current `classify/` behavior. Prove the live model before SQLite.
7. Add SQLite `RuntimeStore`: `meta`, `boots`, `sessions`, `resources`, `resource_members`, `exact_ownership`. No resolver, no candidate sets, no scores yet — only what Wyd actually observed.
8. Restore exact provenance after Wyd restart (the survive-and-reparent scenario).

On step 8, the first eight steps are complete. Only then create:

```text
src/classify/ownership/
    resolver.rs
    rules.rs
```

and begin heuristic attribution where exact ownership is unavailable.

At the end of PHASE 1, behavior of old Wyd must not have changed at all.

---

# 22. Explicitly Deferred (PHASE 3)

```text
daemon
Unix-socket API
MCP server
ACP integration
vendor session registration
web UI
desktop UI
Fleet/cloud
policy engine
```

The core must provide value with:

```text
wyd
```

running exactly as it does today.

No external process or vendor cooperation is required.

---

# Core Architecture

```text
OS processes / ports / Docker
            │
            ▼
      RuntimeCollector
            │
            ▼
      Tool Detection
            │
            ▼
      Runtime Sessions
            │
            ▼
    Exact Ownership Pass
            │
            ▼
   Attribution Resolver     ← PHASE 2
            │
            ▼
       Runtime Graph
        │        │
        │        └── evidence + score
        │
        ▼
     RuntimeStore
       SQLite
        │
        ▼
   Lifecycle / Leftovers
        │
     ┌──┴───┐
     │      │
    TUI    JSON
```

The hard technical product is:

> **deterministic, explainable and durable runtime ownership attribution.**

Everything else is a frontend or future integration.
