# wyd

**wyd?** — *what you doing?*

See what your coding agents left running.

Coding agents spawn MCP servers, headless browsers, dev servers, workers, and databases.

`ps` tells you what is running. `wyd` tells you **why** — which session started it, in which project, and whether it's leftover.

```text
OpenCode
└─ chrome-devtools-mcp
   └─ Chromium ×8       1.2 GB

Agent exited 47m ago
⚠ leftover
```

Local, macOS and Linux. No account, no network, no telemetry.

**https://wyd.sh**

```bash
curl -fsSL https://wyd.sh/install.sh | sh
# or
brew install oxyplay/tap/wyd
# or
cargo install wyd
wyd
wyd upgrade   # brew or cargo, matching the install
wyd --demo    # deterministic synthetic dataset — screenshots/demos, no host scan
```

Each tagged release publishes the crate to crates.io and bumps
`Formula/wyd.rb` in [oxyplay/homebrew-tap](https://github.com/oxyplay/homebrew-tap)
automatically, so `wyd upgrade` stays current with no manual step. `wyd upgrade`
still prints a hint if a formula bump is ever lagging.

Binaries: [GitHub Releases](https://github.com/oxyplay/wyd/releases). From a clone: `cargo install --path .`


![wyd TUI (wyd --demo) — 5 agent sessions, MCP servers, dev servers, databases and Docker](docs/screenshot.webp)

OS daemons stay hidden. Desktop Chrome stays hidden. Agent-spawned Chromium does not.

## Why wyd

| | `ps` / Activity Monitor | Docker Desktop | wyd |
|---|---|---|---|
| Every process | yes | containers only | only dev runtime |
| Who started it | no | compose labels | agent → MCP → browser |
| Which project | no | sometimes | cwd / git root |
| Leftover? | no | you guess | scored, with a reason |
| Safe kill | you hope | stop/rm | PID + start time, `y` to confirm |
| Volumes | — | easy to nuke | unused ≠ garbage; `D` required |

Built for people who run coding agents all day and then ask *what did that session leave behind?*

## Runtime ownership

Beyond the live view, wyd tracks **coding-agent runtime sessions** — which
resources a session spawned, keyed by `boot_id + pid + start_time`, so the
provenance survives process re-parenting and Wyd restarts. A deterministic
resolver attributes resources to a session with an explainable score when
exact ancestry is gone.

- **Sessions** view in the TUI (top-level): agent · project · state · age · id, with a details panel.
- **`wyd why <pid>`** — which session owns a process, and the evidence.
- **`wyd --json sessions`** — recorded sessions as JSON.
- **`wyd serve`** — a local daemon over a Unix socket (`wyd.sock`, mode 0600, single-instance). Keeps provenance fresh and answers read-only queries; vendors can register sessions with `session_start` / `session_end` (their id maps to a Wyd session as an alias).
- **`wyd mcp`** — a Model Context Protocol server over stdio, so a coding agent can ask wyd for its sessions, who owns a PID, and its managed runs (`--allow-run` adds host execution).

## MCP

`wyd` also speaks the Model Context Protocol over stdio, so a coding agent
can ask the machine what it — or other agents — left running.

```bash
wyd mcp                 # read-only
wyd mcp --allow-run     # also allows starting and cancelling runs
```

Starts the local MCP server. Read tools are always available:
`list_sessions` (recorded agent sessions), `explain` (which session owns a
process, by pid), `list_runs`, `get_run` and `read_run_output`. `--allow-run`
adds `start_run` and `cancel_run` for that connection only — without it those
calls are rejected with a message to restart with `--allow-run`. No network,
no account — the answers come from the local provenance store.

Registered in the MCP Registry:

- MCP Registry name: `mcp-name: io.github.oxyplay/wyd`

## Managed runs

`wyd run` starts a command under a local supervisor, so an agent gets a
deadline, a process group wyd owns, and bounded logs instead of a hung shell.

```bash
wyd run --timeout 120s -- npm test
```

The command is `argv`, not a shell — `&&`, pipes and globs need an explicit
`sh -c`. `--timeout` defaults to `10m`, `--grace` (soft-to-forced stop) to
`3s`, and `--cwd` must be absolute. `--json` prints the structured result
instead of the captured output; repeating a `--request-id` within 24 hours
reuses that run instead of starting a second one, and the same id with a
different command is rejected.

Inspect or stop it afterwards:

```bash
wyd runs                  # recent runs: state, outcome, cleanup, duration
wyd runs --json           # full RunView records
wyd logs <id> --follow    # retained stdout (--stream stderr for the other)
wyd cancel <id>           # idempotent
```

The TUI shows the same runs read-only: select **Runs** in the Overview pane,
`enter` opens a run's details. The web dashboard has the same view.

The supervisor owns the run, not the client that started it: if the CLI or MCP
connection drops, the run continues to its deadline and its result and logs are
still there afterwards. Reads are durable and daemon-free — `wyd runs`,
`wyd logs` and the MCP `list_runs`/`get_run`/`read_run_output` read the store
and the retained log files, so they work with no supervisor alive. Starting or
cancelling a run needs the supervisor, which starts on demand and exits after
five idle minutes; the next `wyd run` or `wyd mcp --allow-run` brings it back.
`SIGTERM`/`SIGINT` to the supervisor stops its active runs first (up to 10s)
instead of abandoning them.

Exit codes: the command's own code when it exits, `128+signal` when it is
signaled, `124` on timeout, `130` on cancel, `125` when the command could not
be spawned. The precise reason is always in the JSON (`outcome`, `exit_code`,
`signal`, `cleanup`, `detail`).

Output is bounded per stream at 10 MiB and at 250 MiB total across retained
runs — the head is kept and truncation is reported
(`stdout_truncated`/`stderr_truncated` in JSON, a marker on stderr from
`wyd logs`). The pipe keeps draining after the limit so a chatty test cannot
wedge the supervisor. Finished runs and their logs are kept 7 days.

### What this is not

Stage 1 manages local processes on macOS and Linux and nothing more:

- Cleanup covers the run's own process group. A command that leaves it —
  `setsid`, double fork, an external daemon — is not silently claimed as
  covered: `cleanup` reports `incomplete` or `unknown` instead of `complete`.
- `TMPDIR`, `TMP` and `TEMP` point at a run-owned directory. That is
  convenience, **not a sandbox**: the command can still write anywhere you can.
- No filesystem, network or secret isolation, and no control over package
  installs. Run untrusted code in a container or VM, not here.

MCP setup: `wyd mcp` is read-only. `wyd mcp --allow-run` enables `start_run`
and `cancel_run` for that connection; `start_run` is described as host
execution and is never sandboxed by wyd.

## WebMCP

Try the hosted demo: **https://demo.wyd.sh**

wyd exposes ten WebMCP tools so a browser agent can investigate leftovers in
the same UI the human sees: `list_sessions`, `get_session`, `list_leftovers`,
`explain_process`, `focus_resource`, `propose_cleanup`, plus managed-run reads
`list_runs`, `get_run`, `read_run_output` and the proposal-only
`propose_cancel_run`. Nothing is killed from a tool call — the human confirms
cleanup, and run cancel goes through the same confirmed proposal.

```bash
wyd web         # local runtime (loopback)
wyd web --demo  # synthetic dataset, no host data
```

### Quick test

Paste this into a browser agent on the [demo](https://demo.wyd.sh) (or local `wyd web`):

```text
Find an ended OpenCode session, show me what it left running, explain Chromium PID 4102, and prepare a cleanup proposal. Do not execute cleanup.
```

WebMCP support was added during the OpenAI WebMCP Challenge on top of the
existing wyd CLI/TUI runtime inspector.

See [docs/webmcp.md](docs/webmcp.md).


## Keys

| Key | Action |
|---|---|
| `←` `→` / `h` `l` | overview / list |
| `↑` `↓` / `j` `k` | move (scroll the details popup) |
| `Tab` | focus overview / list |
| `backspace` | go back (clear filter / project / section, never quits) |
| `enter` | details popup; on a project, pin that project |
| `space` | mark several |
| `x` / `K` | terminate / force kill (`y` confirms) |
| `s` | stop running Docker container (running ones sort first) |
| `c` | Docker clean (`y`; volumes need `D`) |
| `P` | prune unused anonymous volumes (confirm; named data kept) |
| `o` | open the selected listener as HTTP (`o try HTTP` — an explicit assumption; listeners are shown as sockets, not URLs) |
| `p` | projects |
| `/` | filter |
| `r` | refresh |
| `?` | help |
| `esc` | close popup, then clear filter / project, then quit |
| `q` | quit |

Kill only signals the item’s own PIDs (re-checked by PID + start time). A named volume is never treated as garbage — only anonymous, unattached ones (`P`) are offered for pruning, with confirmation. The bottom hint line is context-aware: it shows the actions that apply to the currently selected row.

## Scripts

Same snapshot as the TUI — useful after an agent finishes a task:

```bash
wyd --json leftovers
wyd --plain mcp
wyd --json project myapp
wyd --json sessions      # recorded agent sessions
wyd why <pid>            # which session owns a process, and the evidence
wyd serve                # local daemon: Unix-socket API + keeps provenance fresh
wyd mcp                  # MCP server over stdio (for coding agents)
wyd prune --dry-run      # list anonymous volumes that would be deleted
wyd prune                # confirm, then delete them
```

Filters: `leftovers`, `mcp`, `agents`, `docker`, `project`, `sessions` (JSON only).

```json
{
  "runtime": [{ "type": "mcp", "name": "chrome-devtools-mcp", "pid": 94148, "status": "leftover", "reasons": ["owning agent missing"] }],
  "docker": [{ "type": "dangling-image", "name": "abcdef012345", "status": "dangling", "size_bytes": 1400000000 }]
}
```

Field names stay stable until a major version bump. Empty `ports` / `reasons` / `children` / `project` are omitted.

## Config

`~/.config/wyd/config.toml` — missing file is fine.

```toml
[leftovers]
server_age_hours = 8

[persistent]
commands = ["postgres", "redis-server"]

[projects]
roots = ["~/Work"]

[keys]
quit = "q"
kill = "x"
force_kill = "K"
clean = "c"
stop = "s"
prune = "P"
help = "?"
refresh = "r"

[[signature]]
category = "agent"
names = ["myagent"]
contains = ["my-company-agent"]
display = "myagent"
```

## License

Apache-2.0. Copyright 2026 Maksym Nevinchanyy.
