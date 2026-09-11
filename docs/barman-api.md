# `wyd barman` API (contract v1)

Machine-readable JSON API for the `wyd-barman` macOS menu-bar client.
Owned by `wyd`: all intelligence (discovery, ownership, safety) stays here;
the client renders `actions` arrays verbatim and never infers control
semantics.

Requires `schema_version == 1`. The client shows "wyd needs to be updated"
on any other version instead of failing to decode.

## Commands

All commands print pretty JSON to **stdout**, even on failure.

```
wyd barman snapshot --json [--demo]
wyd barman action --target <id> --action <start|stop|restart|kill|open-url> --json [--demo]
wyd barman cleanup-plan --json [--demo]
wyd barman execute --plan <plan-id> [--only id,...] --json [--demo]
wyd barman version --json
```

`--demo` resolves against the deterministic synthetic dataset and never
signals real processes (safe for client development and tests).

Exit codes: `0` success; `2` stale/unknown target (`{"ok":false,"error":"stale
target: re-refresh",...}` — the client should re-refresh); `1` other errors.

## `snapshot`

Single coherent point-in-time document. Arrays are sorted by `id`. Nulls are
omitted. `name` is always the grouped display name — raw command lines are
never emitted.

```json
{
  "schema_version": 1,
  "wyd_version": "0.9.0",
  "generated_at": "2026-09-11T12:00:00Z",
  "projects": [{"id": "project_wyd", "name": "wyd", "agent": "opencode",
    "resource_count": 3, "memory_bytes": 1234, "resource_ids": []}],
  "sessions": [{"id": "session_0123abcd4567ef89", "agent": "opencode",
    "project_id": "project_wyd", "status": "working|ended",
    "age_seconds": 60, "resource_ids": []}],
  "resources": [{"id": "resource_abc123def456", "kind": "agent|mcp|browser|dev_server|dev_service|database|worker|container|service|unknown",
    "name": "vite :5173", "status": "running|stopped", "project_id": null,
    "session_id": null, "port": 5173, "ports": [], "pid": 4812,
    "memory_bytes": 1, "cpu_percent": 0.0, "url": "http://localhost:5173",
    "classification": "active|persistent|leftover",
    "confidence": "high|medium|low", "reasons": [],
    "estimated_reclaim_bytes": 0, "actions": ["open|start|stop|restart|kill"]}],
  "containers": [{"id": "container_a1b2c3", "name": "postgres-dev",
    "compose_project": null, "ports": [], "status": "running|stopped",
    "actions": ["stop|restart"], "estimated_reclaim_bytes": 0}],
  "leftovers": {"count": 0, "estimated_reclaim_bytes": 0, "resource_ids": []}
}
```

Scan reuse: one invocation of the same pipeline the TUI loop uses
(process scan + port scan + group + attach + mark + ownership
record/layer + blocking Docker scan). CPU percent is a first-sample read
and may be 0 on a cold one-shot call.

### Stable ids

- `resource_<blake3-12>` over `(kind, root start_time, project, display
  name)`. PIDs are diagnostic metadata only, never identity (PID reuse).
- `container_<docker-id-short>` reuses the engine id verbatim.
- `project_<slug>` from the classifier's project name.
- `session_<16 hex>` from the stable session key.

### Actions

Computed here; the UI renders them without inference:

- item with a known URL (dev server with a port) → `open` + `url`
  (the client opens it via `NSWorkspace`; no process is touched).
- running process → `stop`, `restart`, `kill`.
- running container → `stop`, `restart`; stopped container → `start`.
- Docker volumes/images → `kill` (remove).

`restart` on a plain process without a known start mechanism returns
`ok:false, error:"restart not supported for this resource"`. v1 restart
scope is Docker containers plus Homebrew-service-backed items where `wyd`
already knows the mechanism. No Swift-side `sh -c` launching, ever.

## `action`

Re-resolves the id against live state at execution time:

- process targets reuse the PID-reuse-guarded `send` path (SIGTERM for
  `stop`, SIGKILL for `kill`, deepest-first, start_time revalidation,
  never `killpg`).
- Docker targets reuse `stop_blocking` / `remove_blocking` (+
  `start`/`restart` via bollard).
- `open-url` returns `{"ok":true,"url":"…"}`; the client opens it.
- project/session ids fan out to member process items through the same
  guarded path.

Success: `{"ok":true,"target":"…","action":"…","detail":"…","stale":false}`.
Unknown/stale id: `{"ok":false,…,"error":"stale target:
re-refresh","stale":true}`, exit 2.

## `cleanup-plan` / `execute`
`cleanup-plan` derives strictly from `classification == "leftover"` rows
(abandoned running processes) and Docker dangling/anonymous-volume
candidates. Stopped containers are not leftovers — they consume nothing;
manage them via the `containers` table (start/stop):

```json
{"plan_id": "cleanup_abc123", "items": [{"resource_id": "…",
  "selected": true, "safe": true, "reason": "…"}],
 "protected": [{"resource_id": "…",
  "reason": "Persistent database service"}],
 "estimated_reclaim_bytes": 0}
```
`execute` revalidates every id through the action path (the plan is
advisory; the engine re-checks identity and the protected list).
Deselected ids are skipped via `--only`. Plans are single-use files under
`<state-dir>/barman-plans/` (next to `state.db`, 5-minute TTL, no DB
migration) — durable across CLI invocations, since the client spawns one
`wyd` process per call:

```json
{"ok": true, "plan_id": "…", "stopped": [], "killed": [],
 "failed": [{"resource_id": "…", "error": "…"}], "reclaimed_bytes": 0}
```

## `version`

```json
{"schema_version": 1, "wyd_version": "0.9.0"}
```
