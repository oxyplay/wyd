# wyd + WebMCP

`wyd web` is a loopback HTTP dashboard plus a WebMCP tool surface: a
**browser** agent can investigate coding-agent runtime leftovers in the
same UI the human sees. It extends the existing `wyd` runtime store rather
than building a separate backend.

## Architecture

```
   w serve / w web  (single writer: server::collect_loop)
        │
        ├── RuntimeProvider trait
        │       ├── LocalProvider  ──> RuntimeStore / OwnershipTracker
        │       └── DemoProvider   ──> deterministic synthetic snapshot
        │
        ├── HTTP API (loopback TcpListener)
        │       /api/health · /api/snapshot
        │       /api/sessions · /api/items · /api/leftovers
        │       /api/explain/<pid> · /api/proposal · /api/confirm
        │       /api/kill (force) · /api/docker/stop · /api/docker/remove
        │       /api/docker/prune   (PID + start-time revalidated; all
        │                            mutating routes CSRF-guarded)
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

`wyd web` binds to `127.0.0.1:8732` by default and refuses any other
address unless `--allow-lan` is passed. It also refuses to start if
`wyd serve` already owns the local collector (single writer per machine).

## Demo dataset

`--demo` serves a deterministic story — no host I/O, no scanners:

- **5 sessions**: `opencode`/`claude`/`cursor` ended and left leftovers;
  `codex`/`gemini-cli` still active.
- **Popular MCP servers**: `chrome-devtools-mcp` (→ headless Chromium ×8),
  `playwright-mcp` (→ Chromium ×3), `github-mcp`, `context7-mcp`,
  `filesystem-mcp`, `sequential-thinking`, `fetch-mcp`.
- **Dev servers**: `vite :5173`, `next :3000`.
- **Persistent** (excluded from cleanup): `postgres`, `redis`, `mysql`.

The page banner reads `Demo data — synthetic, not your machine.`

## WebMCP tools

Registered through `document.modelContext.registerTool` (or
`navigator.modelContext`). Each tool reads/writes the shared `appState` so
the visible UI reflects every agent action.

| Tool | Purpose | UI side effect |
|------|---------|----------------|
| `list_sessions` | Filter sessions by `state`/`agent`/`project` | none (read) |
| `get_session` | One session by id + its resources | focuses that session |
| `list_leftovers` | Leftovers with reasons | switches Overview to Leftovers |
| `explain_process` | `wyd why` over the web | opens details drawer |
| `focus_resource` | Select a session/resource | highlights it in the tree |
| `propose_cleanup` | Build a proposal; **never kills** | fills the Cleanup proposal block |

There is **no** destructive WebMCP tool. The human confirms every action:
`Terminate` in the drawer re-validates PID + start time and asks inline;
cleanup is confirmed with a button in the proposal. Persistent services are
always excluded.

## Human ↔ agent in the same UI

- Selecting a resource opens a **details drawer**: verdict, score, why-it's-flagged
  reasons with a plain-language explanation, listening sockets (address / port /
  protocol / pid — not assumed URLs), provenance evidence, and a *Copy
  investigation prompt* button — the user pastes `Explain why <name> PID <pid>
  is <status> in wyd.` into their agent chat themselves.
- The browser agent and the human clicks both go through the same
  `dispatch()` reducer, so `focus_resource`/`propose_cleanup` visibly update
  the page the user is looking at.

## Security model

- Binds to `127.0.0.1` by default. `--allow-lan` required for any other
  address and logged at startup.
- Static assets are embedded via `include_bytes!` — no filesystem path the
  network can read.
- No `Command` endpoint. No shell execution surface.
- `/api/confirm` requires a matching `snapshot_version`; stale proposals are
  rejected with HTTP 409.
- `/api/kill` re-validates PID + start time (rejects PID reuse) and never
  uses `killpg` — only the item's own PIDs. `--demo` returns
  `simulated: true` and does not signal host processes.
- POST `/api/kill`, `/api/confirm`, `/api/proposal` require a CSRF token
  issued with `index.html`. Responses have no CORS headers.
- The local server does not contact `wyd.sh`, has no telemetry, no accounts.

## Difference from `wyd mcp`

- `wyd mcp` is a stdio MCP server — coding agents connect directly over
  stdin/stdout (JSON-RPC framing). Read-only session/ownership queries.
- `wyd web` is an HTTP dashboard for browsers, with a WebMCP tool surface
  running in the browser context.

Both read the same `RuntimeStore`; they exist for different clients.

## Supported environments

WebMCP requires the host browser to expose `modelContext`:

- ChatGPT desktop app's built-in browser (Work / Codex).
- Chrome ≥149 with the WebMCP origin trial flag
  (`--enable-features=WebMCP`).

When the API is unavailable the dashboard still works as a regular local
web app; registration is skipped and the human can drive everything
manually.

## Quick tour of the JSON surface

```bash
curl -s http://127.0.0.1:8732/api/health
# {"ok":true,"mode":"local","banner":""}

curl -s http://127.0.0.1:8732/api/snapshot | jq .data.overview
# {"total_items":18,"suspicious":2,"categories":[...]}

curl -s -X POST -H 'Content-Type: application/json' \
  -d '{"scope":"leftovers"}' http://127.0.0.1:8732/api/proposal | jq .data.proposal
# {"selected":[...],"excluded":[...],"reclaim_bytes":...,"snapshot_version":...}
```

## Repo layout

- `src/web/mod.rs` — `RuntimeProvider` trait, loopback HTTP routes, WebState.
- `src/web/proposal.rs` — pure proposal builder; no side effects.
- `src/web/demo.rs` — deterministic synthetic dataset (5 agents + MCP servers).
- `src/web/assets.rs` — embedded `web/*` files.
- `web/index.html` — dashboard shell (Overview | Runtime tree | details drawer).
- `web/app.js` — `appState` reducer, WebMCP tool registration, theme toggle.
- `web/styles.css` — light + dark themes, responsive columns.
