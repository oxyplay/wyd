# wyd

**wyd?** — *what you doing?* The question it asks every leftover process.

A fast TUI for seeing what your development tools and AI agents left running.

Coding agents leave MCP servers, headless Chromium, Vite, and Docker junk on the machine. `ps` shows every PID. wyd shows the session: who started it, which project, whether it’s leftover, and whether you can kill it.

Local, macOS and Linux. No account, no network, no telemetry.


```bash
brew install oxyplay/tap/wyd
# or
cargo install wyd
wyd
wyd upgrade   # brew or cargo, matching the install
```

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

Provenance lives in SQLite (`~/Library/Application Support/wyd/state.db` on
macOS, `$XDG_DATA_HOME/wyd/state.db` on Linux), kept fresh by the TUI,
`wyd serve`, or `wyd mcp` (whichever is running; never more than one writer).

## Keys

| Key | Action |
|---|---|
| `←` `→` / `h` `l` | overview / list |
| `↑` `↓` / `j` `k` | move |
| `Tab` | focus overview / list |
| `backspace` | go back (clear filter / project / section, never quits) |
| `enter` | details popup; on a project, pin that project |
| `space` | mark several |
| `x` / `K` | terminate / force kill (`y` confirms) |
| `s` | stop running Docker container (running ones sort first) |
| `c` | Docker clean (`y`; volumes need `D`) |
| `P` | prune unused anonymous volumes (confirm; named data kept) |
| `o` | open server URL from details (`http://…` shown only for a live socket) |
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
