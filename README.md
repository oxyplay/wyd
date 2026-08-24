# wyd

See what your dev tools and AI agents left running.

A local TUI (macOS / Linux). No accounts, no network, no telemetry.
Hides OS noise. Groups agents, MCP, browsers, servers, DBs, Docker.
Tells you the project and whether it looks leftover.

```bash
cargo install wyd
wyd
```

macOS / Linux binaries: [GitHub Releases](https://github.com/oxyplay/wyd/releases).

From a clone: `cargo install --path .`


## Keys

| Key | Action |
|---|---|
| `←` `→` | overview / list |
| `↑` `↓` | move |
| `enter` | details; on a project, pin that project |
| `space` | mark several |
| `k` / `K` | terminate / force kill (`y` confirms) |
| `x` | Docker clean (`y`; volumes need `D`) |
| `p` | projects |
| `/` | filter |
| `r` | refresh |
| `?` | help |
| `esc` | clear filter, then project, then quit |
| `q` | quit |

Kill only signals the item’s own PIDs (re-checked by PID + start time).
An unused Docker volume is never treated as garbage.

## Scripts

Same snapshot as the TUI:

```bash
wyd --json leftovers
wyd --plain mcp
wyd --json project myapp
```

Filters: `leftovers`, `mcp`, `agents`, `docker`, `project`.

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
kill = "k"
force_kill = "K"
clean = "x"
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

Longer product notes: [SPEC.md](SPEC.md).
