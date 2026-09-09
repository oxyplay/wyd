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
        │       /api/runs · /api/runs/<id> · /api/runs/<id>/output
        │       /api/runs/<id>/cancel/propose · /api/runs/<id>/cancel
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
- **Managed runs**: synthetic `RunView`s (running / exited / timed_out /
  cancelled / spawn_failed). Demo never starts a process, and demo cancel is
  simulated — no host action.
- **Persistent** (excluded from cleanup): `postgres`, `redis`, `mysql`.

The page banner reads `Demo data — synthetic, not your machine.`

## WebMCP tools

Registered through `document.modelContext.registerTool` (Chrome ≥149;
`navigator.modelContext` is accepted as a fallback). Each tool reads/writes the shared `appState` so
the visible UI reflects every agent action.

| Tool | Purpose | UI side effect |
|------|---------|----------------|
| `list_sessions` | Filter sessions by `state`/`agent`/`project` | none (read) |
| `get_session` | One session by id + its resources | focuses that session |
| `list_leftovers` | Leftovers with reasons | switches Overview to Leftovers |
| `explain_process` | `wyd why` over the web | opens details drawer |
| `focus_resource` | Select a session/resource | highlights it in the tree |
| `propose_cleanup` | Build a proposal; **never kills** | fills the Cleanup proposal block |
| `list_runs` | Managed runs with state/outcome/cleanup | none (read; populates Runs view) |
| `get_run` | One run by id: result, cleanup, capabilities | none (read; opens run detail) |
| `read_run_output` | Bounded stdout/stderr chunk from a cursor | none (read; fills output pane) |
| `propose_cancel_run` | Build a cancel proposal; **never cancels** | creates a pending cancel proposal in the Runs view |

There is **no** destructive WebMCP tool. The human confirms every action:
`Terminate` in the drawer re-validates PID + start time and asks inline;
cleanup is confirmed with a button in the proposal; a run cancel is confirmed
in the Runs view. Persistent services are always excluded.

## Managed runs

The dashboard lists runs started through `wyd run` or `wyd mcp --allow-run`
and reads their retained output. It cannot start one.

| Route | Method | Purpose |
|-------|--------|---------|
| `/api/runs` | GET | List runs (`state`, `project`, `session`, `limit`) |
| `/api/runs/<id>` | GET | One run: state, result, cleanup, capabilities |
| `/api/runs/<id>/output` | GET | Bounded stdout/stderr chunk from a byte cursor |
| `/api/runs/<id>/cancel/propose` | POST | Build a cancel proposal; never cancels |
| `/api/runs/<id>/cancel` | POST | Apply a confirmed proposal |

Every read is store-backed: list and detail come from
`RuntimeStore::run_list`/`run_get` mapped through `RunView::from_record`, and
output is read from the retained log file with `runner::logs::read_chunk`. No
read path connects to the supervisor, executes a command, or signals anything,
so the Runs view works with no daemon alive.

For managed runs, cancel is the only host action, and it stays
human-confirmed: `propose_cancel_run` and `/api/runs/<id>/cancel/propose` only
build a proposal; `/api/runs/<id>/cancel` applies it after the human confirms
in the Runs view. Both POSTs require the CSRF token, and a bare GET never
cancels.

There is **no** route or tool that starts a command, so `--allow-lan` does not
expose host execution. Host execution exists only on the local `wyd mcp
--allow-run` connection and `wyd run`.

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
- No `Command` endpoint and no host-execution route: the dashboard observes
  managed runs and can propose a cancel, but never starts a command, and
  `--allow-lan` does not expose host execution.
- `/api/runs/<id>/cancel/propose` only builds a proposal; POST
  `/api/runs/<id>/cancel` applies it after the human confirms in the Runs
  view. A bare GET never cancels.
- `/api/confirm` requires a matching `snapshot_version`; stale proposals are
  rejected with HTTP 409.
- `/api/kill` re-validates PID + start time (rejects PID reuse) and never
  uses `killpg` — only the item's own PIDs. `--demo` returns
  `simulated: true` and does not signal host processes.
- POST `/api/kill`, `/api/confirm`, `/api/proposal` and
  `/api/runs/<id>/cancel*` require a CSRF token issued with `index.html`.
  Responses have no CORS headers.
- The local server does not contact `wyd.sh`, has no telemetry, no accounts.

## Difference from `wyd mcp`

- `wyd mcp` is a stdio MCP server — coding agents connect directly over
  stdin/stdout (JSON-RPC framing). Session/ownership queries plus managed-run
  reads; `wyd mcp --allow-run` adds host execution (`start_run`, `cancel_run`).
- `wyd web` is an HTTP dashboard for browsers, with a WebMCP tool surface
  running in the browser context. It reads the same runs but never starts one:
  cancel is a human-confirmed proposal.

Both read the same `RuntimeStore`; they exist for different clients.

## Supported environments

WebMCP requires the host browser to expose `document.modelContext`:

- ChatGPT desktop app's built-in browser (Work / Codex).
- Chrome ≥149 with the WebMCP origin trial or
  `chrome://flags/#enable-webmcp-testing`.

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

- `src/web/mod.rs` — `RuntimeProvider` trait, `LocalProvider`/`DemoProvider`,
  loopback HTTP routes (including `/api/runs*`), WebState.
- `src/web/proposal.rs` — pure proposal builder; no side effects.
- `src/web/assets.rs` — embedded `web/*` files.
- `web/index.html` — dashboard shell (Overview | Runs | Runtime tree | details drawer).
- `web/app.js` — `appState` reducer, WebMCP tool registration (sessions + runs), theme toggle.
- `web/styles.css` — light + dark themes, responsive columns.
