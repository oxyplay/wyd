# wyd + WebMCP

`wyd web` is a loopback HTTP dashboard and a WebMCP tool surface that lets a
browser agent investigate coding-agent runtime leftovers **in the same UI
the human sees**. It extends the existing `wyd` runtime store rather than
building a separate backend.

## Architecture

```
   w serve / w web  (single writer: server::collect_loop)
        │
        ├── RuntimeProvider trait
        │       ├── LocalProvider  ──> RuntimeStore / OwnershipTracker
        │       └── DemoProvider   ──> deterministic synthetic snapshot
        │
        ├── HTTP API (loopback TcpListener)
        │       /api/health · /api/snapshot · /api/sessions
        │       /api/items · /api/leftovers · /api/explain/<pid>
        │       /api/proposal · /api/confirm
        │
        └── embedded static assets (web/index.html, web/app.js, web/styles.css)
                │
                └── shared appState reducer + WebMCP tool registration
```

Ownership reasoning is **never duplicated in JavaScript**. The browser agent
and the human UI both drive the same Rust endpoints, so the visible UI
always reflects what the resolver actually decided.

## Running locally

```bash
# Real local mode (uses your host's process tree and provenance store)
cargo run -- web

# Hosted demo mode (deterministic synthetic data; safe to expose publicly)
cargo run -- web --demo --port 8732
```

`wyd web` defaults to `127.0.0.1:8732`. It refuses to bind any other
address unless `--allow-lan` is passed. It also refuses to start if
`wyd serve` already owns the local collector, so a single writer per
machine is preserved.

## WebMCP tools

Registered through `document.modelContext.registerTool` (or
`navigator.modelContext`, whichever the host environment exposes).
Each tool reads and writes the shared `appState` so the visible UI
reflects every agent action.

| Tool | Purpose | UI side effect |
|------|---------|----------------|
| `list_sessions` | Filter sessions by `state` / `agent` / `project` | none (read) |
| `get_session` | One session by id | focuses that session |
| `list_leftovers` | Leftovers with reasons | switches view to leftovers |
| `explain_process` | `wyd why` over the web | opens details panel |
| `focus_resource` | Highlight a session/resource | outlines in the UI |
| `propose_cleanup` | Build a proposal; excludes obvious persistent resources; **never kills** | populates proposal panel |

`propose_cleanup` returns `{id, selected, excluded, reclaim_bytes,
snapshot_version}`. The visible confirm button is wired through the
human-driven `/api/confirm` endpoint, which enforces `snapshot_version`
and refuses to run a stale proposal (the analogue of `wyd`'s PID +
start-time safety check). There is no WebMCP tool that performs a
destructive action.

## Local vs demo

- `wyd web` (local) reads `RuntimeStore::default_path()` and runs
  `server::collect_loop`. The browser agent gets the same data the TUI
  gets.
- `wyd web --demo` swaps `LocalProvider` for `DemoProvider`. The synthetic
  dataset is reproducible (FNV-1a over fixed seeds), includes two
  sessions (ended opencode on `~/Work/wyd`, active codex on `~/Work/api`)
  and two persistent services (postgres, redis). The page banner reads
  `Demo data — synthetic; not your machine.`

## Security model

- Binds to `127.0.0.1` by default. `--allow-lan` is required for any
  non-loopback address and is logged at startup.
- Static assets are embedded in the binary via `include_bytes!`. There is
  no filesystem path the network can read — `wyd web` never serves a
  path the user supplies.
- No `Command::new` endpoint. No shell execution surface. No remote
  fetch from the dashboard JS.
- `/api/confirm` requires a matching `snapshot_version`; stale proposals
  are rejected with HTTP 409.
- The local server does not contact `wyd.sh`, has no telemetry, and has
  no account requirement.

## How we used WebMCP

The dashboard registers a small set of high-signal tools (six in
total — see table above). Every tool call is funneled through the
same `dispatch()` reducer the human's clicks use, so the agent's
reasoning and the user's exploration share a single UI state. The
agent can:

1. Enumerate sessions and leftovers.
2. Pick the relevant session with `focus_resource`.
3. Explain a specific process with `explain_process`.
4. Propose a cleanup; the human reviews and confirms in the UI.

We deliberately stop short of exposing a `kill_process` /
`prune_volume` WebMCP tool. Even where the host environment supports
in-band confirmation primitives, `wyd` keeps destructive actions
human-only — matching its existing safety philosophy.

## Difference from `wyd mcp`

- `wyd mcp` is a stdio MCP server. Coding agents connect to it
  directly over the local process' stdin/stdout. It speaks the MCP
  JSON-RPC framing and exposes the same `wyd why` / session
  primitives.
- `wyd web` (this feature) is an HTTP dashboard for browsers, with a
  WebMCP tool surface that runs in the browser context. It uses the
  same `RuntimeStore` underneath.

Both are entry points to the same data; they exist for different
clients.

## Supported environments

WebMCP requires the host browser to expose `modelContext`. We have
targeted:

- ChatGPT desktop app's built-in browser (Work / Codex).
- Chrome ≥149 with the WebMCP origin trial flag
  (`--enable-features=WebMCP`).

When the API is unavailable the dashboard still works as a regular
local web app; the registration step is skipped and the human can
drive everything manually.

## Quick tour of the JSON surface

```bash
curl -s http://127.0.0.1:8732/api/health
# {"ok":true,"mode":"local","banner":""}

curl -s http://127.0.0.1:8732/api/snapshot | jq .data.sessions
# [...agent, project, active, age_seconds...]

curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"scope":"leftovers"}' http://127.0.0.1:8732/api/proposal | jq .data.proposal.selected
# [...resource names with reasons...]
```

## Repo layout

- `src/web/mod.rs` — `RuntimeProvider` trait, loopback HTTP routes, WebState.
- `src/web/proposal.rs` — pure proposal builder; no side effects.
- `src/web/demo.rs` — deterministic synthetic dataset.
- `src/web/assets.rs` — embedded `web/*` files.
- `web/index.html` — dashboard shell.
- `web/app.js` — `appState` reducer + WebMCP tool registration.
- `web/styles.css` — dark, technical theme matching `wyd.sh`.
