# wyd

> A fast TUI for seeing what your development tools and AI agents left running.

> **Status:** prototype. Classifies agents, MCP, browsers, servers, DBs; shows listen ports and git project. Kill and Docker are not built yet.

**wyd** is a fast terminal UI for understanding, inspecting, and cleaning up your local development runtime.

Modern development environments accumulate a surprising amount of background state:

- AI coding agents
- MCP servers
- headless browsers
- Playwright / Chromium processes
- Node.js / Python / PHP dev servers
- language servers
- databases
- Docker containers
- unused Docker images
- orphaned volumes
- build cache
- listening ports
- stale processes from old projects

Traditional tools such as `ps`, `top`, `btop`, `lsof`, `docker ps`, and Docker Desktop show the raw system state, but they do not answer the question developers increasingly have:

> **What the hell is still running, who started it, what project does it belong to, and can I safely kill it?**

wyd provides a semantic, project-aware view of the local development runtime.

---

## The idea

Running:

```bash
wyd
```

opens a TUI inspired by tools such as `lazygit`.

Instead of showing every macOS/Linux process, wyd only shows development-related activity and groups it into meaningful categories.

Example:

```text
┌ wyd ───────────────────────────────────────── RAM 7.2/32G │ CPU 12% ┐
│                                                                    │
│  Overview             Runtime                                     │
│                                                                    │
│  Agents        3      ● omp                    312 MB   00:42      │
│  MCP           7        ├ chrome-devtools-mcp   48 MB              │
│  Browsers     14        │ └ Chromium ×6        780 MB              │
│  Dev servers   4        └ queryknight mcp       37 MB              │
│  Databases     3                                                   │
│  Docker        8      ● opencode                420 MB   01:17      │
│  Ports        11        ├ playwright-mcp                            │
│  Leftovers     5        └ Chromium ×8          1.1 GB              │
│                                                                    │
│  ⚠ RAM waste          ⚠ vite                   181 MB   17h        │
│    ~1.6 GB              :3001 ~/Work/old-project                  │
│                         likely leftover                            │
│                                                                    │
├────────────────────────────────────────────────────────────────────┤
│ ↑↓ select  enter details  k kill  x clean  r refresh  / filter    │
└────────────────────────────────────────────────────────────────────┘
```

The default screen is intentionally small and useful.

No system daemons.  
No Spotlight noise.  
No giant process list.

---

## What wyd understands

wyd classifies and groups development activity into high-level categories.

### Agents

Examples:

- OpenCode
- OMP
- Claude Code
- Codex
- Aider
- Cursor-related agents
- configurable custom agents

Instead of showing a tree of anonymous `node` processes, wyd tries to show:

```text
OpenCode / Kimi
└─ chrome-devtools-mcp
   └─ Chromium ×8
```

---

### MCP servers

MCP is a first-class concept.

Example:

```text
MCP SERVERS

● chrome-devtools-mcp ×2
● queryknight mcp ×2
● filesystem-mcp
● playwright-mcp
● context7
```

Selecting one shows ownership and descendants:

```text
chrome-devtools-mcp

PID        94148
PPID       94108
RAM        48 MB
CPU        0.1%
Uptime     42m
Project    ~/Work/queryknight
Owner      omp PID 94097

Children
└─ Chromium ×6                       780 MB
```

---

### Headless browsers

wyd should distinguish normal desktop browsers from browser processes spawned by development tooling.

A normal Chrome session should generally not appear.

This:

```text
Google Chrome
```

is probably irrelevant.

This:

```text
omp
└─ chrome-devtools-mcp
   └─ Chromium Helper ×12
```

is relevant and should be grouped as:

```text
Chromium / chrome-devtools-mcp ×12     1.3 GB
```

---

### Dev servers

Examples:

- Vite
- Next.js
- Nuxt
- Astro
- webpack dev server
- Node.js APIs
- Bun / Deno servers
- Uvicorn
- Gunicorn
- Django dev server
- Flask
- PHP built-in server
- Laravel-related processes
- custom servers detected by command + port + cwd

Example:

```text
DEV SERVERS

● Vite       :5173     ~/Work/databoundary
● Node API   :3000     ~/Work/flexus
⚠ Vite       :3001     ~/Work/old-project    17h
```

---

### Databases

Examples:

- PostgreSQL
- MySQL
- MariaDB
- Redis
- MongoDB

wyd should distinguish between:

```text
PostgreSQL :5432
Homebrew service
```

and:

```text
PostgreSQL :5441
~/Work/temporary-test
started by agent
```

Persistent services should not automatically be treated as suspicious.

---

### Docker

Docker is a full first-class section, not just `docker ps`.

wyd should show:

- running containers
- stopped containers
- images
- dangling images
- unused images
- volumes
- unused volumes
- networks
- unused networks
- build cache
- disk usage
- reclaimable disk
- Docker Compose project ownership where available

Example:

```text
DOCKER                                      Disk: 27.4 GB

Containers
● flexus-app              running           420 MB
● flexus-mysql            running           1.2 GB
○ old-api                 stopped           310 MB

Images
⚠ dangling                12               1.7 GB
⚠ unused                  7                4.8 GB

Volumes
● attached                18               12.2 GB
⚠ unused                  4                 8.8 GB

Build cache
⚠ reclaimable                              8.4 GB

Potentially reclaimable                   18.9 GB
```

wyd must **never assume an unused volume is garbage**.

Volumes may contain valuable persistent databases.

---

## Projects

wyd attempts to associate processes and Docker resources with projects.

Example:

```text
PROJECTS

queryknight                         1.42 GB RAM
├─ omp ×2
├─ MCP ×4
└─ Chromium ×12

databoundary                         384 MB RAM
├─ Vite :5173
└─ Node

flexus                     2.1 GB RAM / 8.4 GB disk
├─ Node :3000
├─ MySQL [Docker]
├─ Redis [Docker]
└─ flexus_mysql_data
```

Project detection can use:

1. process cwd
2. command arguments
3. parent cwd
4. Git root
5. Docker Compose project labels
6. container labels
7. open files where useful

---

## Leftovers

One of wyd's core features is detecting likely development leftovers.

Example:

```text
LEFTOVERS                              ~1.6 GB RAM / 11.4 GB disk

Processes
⚠ Chromium ×8        1.1 GB    owning agent exited
⚠ Vite :3001         181 MB    terminal gone / running 17h

MCP
⚠ filesystem-mcp ×2  120 MB    owning agent gone

Docker
⚠ stopped containers           620 MB
⚠ dangling images              1.7 GB
⚠ build cache                  8.4 GB
```

A leftover is a **heuristic**, not a guarantee.

wyd should explain *why* something looks suspicious instead of simply labeling it garbage.

Possible signals:

- parent process no longer exists
- process has been re-parented
- no active terminal ancestor
- owning agent appears to have exited
- very old dev server
- project no longer has an active session
- MCP server has no known owning agent
- headless browser remains after MCP/agent exit
- stopped Docker resources have not been used for a long time
- dangling Docker images
- unused Docker networks
- reclaimable build cache

---

## Safe kill

wyd should make cleanup easy, but conservative.

Navigate to a process and press:

```text
k
```

Example:

```text
Kill this tree?

chrome-devtools-mcp
└─ Chromium ×8

9 processes
~934 MB RAM

[y] terminate
[n] cancel
```

Normal kill should attempt graceful termination first.

Force kill should be a separate explicit action.

```text
K = force kill
```

wyd should revalidate PID + process start time before sending signals to reduce the risk of PID reuse.

---

## Safe Docker cleanup

Docker cleanup uses a different action:

```text
x
```

Example:

```text
Remove image?

<none>:<none>
1.4 GB
unused

[y/N]
```

Volumes receive stronger warnings:

```text
⚠ PERSISTENT DATA

oldproject_postgres
6.8 GB

Previously used by:
oldproject-db

This volume may contain database data.

[D] Delete permanently
[Esc] Cancel
```

wyd should not blindly wrap:

```bash
docker system prune -a --volumes
```

Interactive, resource-by-resource cleanup is the intended behavior.

---

## Keyboard model

The UI should feel familiar to `lazygit` users.

```text
↑ / ↓ / j / k     navigate
← / → / h / l     change panel
Enter             inspect / open
Space             select
k                 terminate process/tree
K                 force kill
x                 remove/clean Docker resource
r                 refresh
/                 search/filter
p                 project view
?                 help
Esc               back
q                 quit
```

---

## Non-interactive mode

The TUI is the primary experience, but wyd should also work in scripts and with AI agents.

Examples:

```bash
wyd --plain
wyd --json
wyd --json leftovers
wyd --json mcp
```

This enables workflows such as:

```text
agent finishes task
→ runs `wyd --json leftovers`
→ checks whether it left browsers/servers/MCP behind
→ optionally cleans up its own runtime
```

---

## Architecture

The intended implementation is a native Rust binary.

Suggested core stack:

- `ratatui` — TUI
- `crossterm` — terminal input/events
- `sysinfo` — process/system information
- `netstat2` or native socket inspection — process/port mapping
- `bollard` — Docker Engine API
- `tokio` — concurrent scanners
- `clap` — CLI options
- `serde` — configuration/state

wyd should avoid spawning `ps`, `lsof`, or `docker ps` on every refresh.

The UI renders the latest snapshot while scanners refresh independently.

```text
Process scanner ─┐
Port scanner ────┼──→ Snapshot ───→ TUI
Docker scanner ──┘
```

The goal is instant startup and low idle overhead.

---

## Why Rust

wyd is a good fit for Rust because it can provide:

- a single executable
- fast startup
- low memory usage
- native macOS/Linux access
- no runtime dependency
- simple Homebrew / binary distribution
- safe concurrent scanning
- good terminal UI ecosystem

---

## Installation

The final installation experience should be simple.

### Homebrew

```bash
brew install <tap>/wyd
```

or, depending on final package naming:

```bash
brew install <tap>/wyd
```

### Cargo

```bash
cargo install wyd
```

### Prebuilt binary

Release binaries should be provided for at least:

```text
aarch64-apple-darwin
x86_64-apple-darwin
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
```

### From source

```bash
git clone <repo>
cd wyd
cargo install --path .
wyd
```

---

## Platform scope

Initial support:

- macOS
- Linux

Potential later support:

- Windows

macOS and Linux differ significantly in process and socket APIs, so platform-specific scanner backends are expected.

---

## Philosophy

wyd should stay focused.

It is **not** intended to become:

- a replacement for Activity Monitor
- a replacement for `top`
- a full Docker Desktop clone
- an observability platform
- an enterprise agent manager
- a daemon-heavy monitoring system
- an AI-powered process classifier

The core idea is simple:

> **Show developers the runtime their tools and agents created, explain it, and let them safely clean it up.**

---

## Possible tagline

> **See what your AI agents left running.**

Alternative:

> **A TUI for understanding and cleaning up your local development runtime.**

Or, more casually:

> **What the hell is still running?**

---

## Status

Early prototype: live process snapshot and ancestry tree.

The mockup in this README is the target UI, not what `cargo run` renders today.

## License

Copyright 2026 Maksym Nevinchanyy.

Licensed under the [Apache License, Version 2.0](LICENSE).
