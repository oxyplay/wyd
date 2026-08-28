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
```

Each tagged release publishes the crate to crates.io and bumps
`Formula/wyd.rb` in [oxyplay/homebrew-tap](https://github.com/oxyplay/homebrew-tap)
automatically, so `wyd upgrade` stays current with no manual step. `wyd upgrade`
still prints a hint if a formula bump is ever lagging.

Binaries: [GitHub Releases](https://github.com/oxyplay/wyd/releases). From a clone: `cargo install --path .`


![wyd TUI — agents, MCP, Vite, and project folders](docs/screenshot.webp)

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
- **`wyd mcp`** — a Model Context Protocol server over stdio, so a coding agent can ask wyd for its sessions and who owns a PID.

## MCP

`wyd` also speaks the Model Context Protocol over stdio, so a coding agent
can ask the machine what it — or other agents — left running.

```bash
wyd mcp
```

Starts the local MCP server (read-only). It exposes two tools:
`list_sessions` (recorded agent sessions) and `explain` (which session owns a
process, by pid). No network, no account — the answers come from the local
provenance store.

Registered in the MCP Registry:

- MCP Registry name: `mcp-name: io.github.oxyplay/wyd`

## WebMCP (browser)

`wyd web` is a loopback HTTP dashboard plus a WebMCP tool surface, so a
**browser** agent can investigate runtime provenance in the same UI the
human sees — not just a coding agent over stdio.

```bash
wyd web              # real local runtime (loopback only)
wyd web --demo       # deterministic synthetic dataset, no host data
```

- **Overview** sidebar: categories with counts; RAM/CPU metrics shown as small
  icons (with alt text), reclaimable Docker disk shown against Docker, never
  the Leftovers total.
- **Runtime** tree: agent → MCP → browser/dev-server hierarchy with RAM/CPU/status/age.
- **Details** drawer: verdict, score, why-it's-flagged reasons with a shared
  plain-language explanation, listening sockets (address/port/protocol/pid —
  not assumed URLs), provenance evidence, `Terminate` (PID + start-time
  revalidated, human-confirmed) and *Copy investigation prompt*.
- **Light + dark** themes, responsive columns.
- Loopback-only by default; `--allow-lan` is explicit and discouraged.

`wyd mcp` (stdio) and `wyd web` (browser WebMCP) read the same store and
expose the same session/ownership primitives — they're entry points for
different clients. See [docs/webmcp.md](docs/webmcp.md).

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
