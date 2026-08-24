# wyd — Product & Technical Specification

## 1. Summary

wyd is a fast, interactive terminal UI for inspecting and cleaning up local development runtime state.

The tool focuses on development-related processes and resources instead of exposing the full operating-system process table.

Its purpose is to answer:

1. What development tools are currently running?
2. Which agent/session/project started them?
3. What ports and resources are they consuming?
4. Which MCP servers and headless browsers are alive?
5. Which Docker resources are active, stale, or reclaimable?
6. Which processes look abandoned?
7. Can the user safely terminate or remove them?

Primary interface:

```bash
wyd
```

This opens an interactive TUI inspired by `lazygit`.

---

# 2. Problem

AI-assisted development increasingly creates temporary local infrastructure:

- coding agents
- MCP servers
- Chrome DevTools MCP
- Playwright MCP
- headless Chromium processes
- temporary Node/Python servers
- local databases
- Docker containers
- Docker volumes
- build caches
- language servers
- proxy processes
- temporary listeners

A session can end while some of these remain alive.

The developer is then left with raw system output such as:

```text
node
node
node
Chromium Helper
Chromium Helper
python
npm
postgres
```

Existing tools expose individual layers:

| Tool | Good at | Missing |
| --- | --- | --- |
| `ps` / `procs` | process list/tree | semantic dev context |
| `top` / `btop` | CPU/RAM | project/origin |
| `lsof` | ports/files | ownership model |
| Docker CLI | containers/images | host process context |
| Docker Desktop | Docker resources | agents/MCP/process ancestry |
| Activity Monitor | generic processes | development semantics |

wyd combines these signals into a developer-oriented runtime view.

---

# 3. Goals

## 3.1 Primary goals

wyd MUST:

- start quickly;
- use little CPU/RAM itself;
- show only development-relevant runtime by default;
- group related process trees;
- recognize AI coding agents;
- recognize MCP servers;
- recognize headless browser trees;
- recognize common dev servers;
- recognize language servers;
- recognize databases;
- show listening ports;
- associate runtime with projects;
- inspect Docker state;
- identify likely leftovers;
- allow safe process termination;
- allow safe Docker cleanup;
- work on macOS and Linux;
- provide non-interactive/plain/JSON output.

---

## 3.2 Non-goals for V0/V1

wyd MUST NOT require:

- an LLM;
- embeddings;
- cloud services;
- a remote backend;
- authentication;
- an account;
- telemetry;
- Kubernetes;
- an always-running daemon;
- eBPF;
- a database;
- enterprise policy systems;
- automatic destructive cleanup.

These may only be reconsidered if a concrete use case later requires them.

---

# 4. Product principles

## 4.1 Semantic over exhaustive

The default screen should not show every process.

Instead of:

```text
node ×38
Chrome Helper ×42
python ×9
```

show:

```text
OpenCode / Kimi
├─ MCP ×3
├─ Chromium ×8
└─ Vite :5173
```

---

## 4.2 Explain before acting

Every suspicious item should have a reason.

Bad:

```text
⚠ node
```

Good:

```text
⚠ Vite :3001
  ~/Work/old-project
  running 17h
  terminal ancestor no longer exists
```

---

## 4.3 Conservative cleanup

wyd may suggest cleanup.

wyd must not silently destroy runtime state or persistent data.

Especially:

- Docker volumes;
- database containers;
- unknown processes;
- persistent services.

---

## 4.4 Origin matters

The most useful view is not merely category-based.

wyd should attempt to answer:

```text
Who started this?
```

Examples:

```text
Ghostty
└─ zsh
   └─ omp
      └─ chrome-devtools-mcp
         └─ Chromium ×8
```

or:

```text
VS Code
└─ copilot-language-server ×2
```

---

# 5. User experience

## 5.1 Default invocation

```bash
wyd
```

opens the dashboard.

Proposed layout:

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
│  Projects      6        └ Chromium ×8          1.1 GB              │
│  Leftovers     5                                                   │
│                                                                    │
│  ⚠ RAM waste          ⚠ vite                   181 MB   17h        │
│    ~1.6 GB              :3001 ~/Work/old-project                  │
│                         likely leftover                            │
│                                                                    │
├────────────────────────────────────────────────────────────────────┤
│ ↑↓ select  enter details  k kill  x clean  r refresh  / filter    │
└────────────────────────────────────────────────────────────────────┘
```

The first screen should be understandable without opening details.

---

# 6. Navigation

Default keys:

```text
↑ / ↓ / j / k     navigate
← / → / h / l     switch panels
Enter             inspect
Space             select / multi-select
k                 terminate
K                 force terminate
x                 cleanup/remove
r                 refresh
/                 search/filter
p                 project view
?                 help
Esc               back
q                 quit
```

Key bindings should eventually be configurable.

---

# 7. Core views

## 7.1 Overview

Contains compact totals:

```text
Agents          3        1.1 GB
MCP             7        380 MB
Browsers        2 groups 1.8 GB
Dev servers     4        620 MB
Databases       3        1.4 GB
Docker          8        27.4 GB disk
Ports          11
Projects         6
Leftovers        5        ~1.6 GB RAM / 11 GB disk
```

---

## 7.2 Agents

Displays recognized agent processes and their relevant descendants.

Example:

```text
AGENTS

● OMP                           312 MB
  ~/Work/queryknight
  ├─ chrome-devtools-mcp
  │  └─ Chromium ×6             780 MB
  └─ queryknight mcp             37 MB

● OpenCode / Kimi              420 MB
  ~/Work/databoundary
  ├─ playwright-mcp
  ├─ Chromium ×8               1.1 GB
  └─ Vite :5173
```

Known agent names are classification data, not hard-coded assumptions about every child.

---

## 7.3 MCP

MCP is a dedicated category.

Example:

```text
MCP SERVERS                          7 running / 420 MB

● chrome-devtools-mcp ×2
● queryknight mcp ×2
● filesystem-mcp
● playwright-mcp
● context7
```

Detail:

```text
chrome-devtools-mcp

PID        94148
PPID       94108
RAM        48 MB
CPU        0.1%
Uptime     42m
Project    ~/Work/queryknight
Owner      omp PID 94097
Command    npm exec chrome-devtools-mcp@latest

Children
└─ node
   └─ Chromium ×6                       780 MB

Ports
9222 localhost
```

---

## 7.4 Browsers

Show only browser trees determined to be development-related.

Examples of useful origins:

- Playwright
- Puppeteer
- Chrome DevTools MCP
- browser-use
- Selenium
- agent-owned Chrome profile
- known remote debugging invocation

Group helpers/renderers into one logical browser group where possible.

Example:

```text
BROWSERS

● Chromium / chrome-devtools-mcp
  owner: omp
  project: queryknight
  processes: 8
  RAM: 1.1 GB

⚠ Chromium / unknown
  processes: 6
  RAM: 820 MB
  no live owning agent
```

---

## 7.5 Dev servers

Show:

- logical server type;
- port;
- PID;
- project;
- memory;
- uptime;
- origin.

Example:

```text
DEV SERVERS

● Vite       :5173     180 MB    ~/Work/databoundary
● Node API   :3000     240 MB    ~/Work/flexus
⚠ Vite       :3001     181 MB    ~/Work/old-project    17h
```

---

## 7.6 Databases

Example:

```text
DATABASES

○ PostgreSQL :5432     420 MB    Homebrew service
○ Redis      :6379      31 MB    Homebrew service

● PostgreSQL :5442     390 MB    ~/Work/foo
  owner: OpenCode session
```

Persistent services use neutral status.

---

## 7.7 Ports

Example:

```text
PORTS

3000   node       ~/Work/flexus
5173   vite       ~/Work/databoundary
5432   postgres   Homebrew
6379   redis      Homebrew
9222   Chromium   chrome-devtools-mcp → omp
```

Ports should link back to the owning logical runtime item.

---

## 7.8 Projects

Example:

```text
PROJECTS

queryknight                      RAM 1.42 GB
├─ OMP ×2
├─ MCP ×4
└─ Chromium ×12

databoundary                     RAM 384 MB
├─ Vite :5173
└─ Node

flexus                           RAM 2.1 GB / disk 8.4 GB
├─ Node :3000
├─ MySQL [Docker]
├─ Redis [Docker]
├─ image: flexus-app
└─ volume: flexus_mysql_data
```

Selecting a project filters all other views to that project.

---

# 8. Docker

## 8.1 Docker overview

wyd should query Docker through the Engine API, preferably via local socket.

Default view:

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

---

## 8.2 Docker entities

The internal model should cover:

```text
Container
Image
Volume
Network
BuildCache
ComposeProject
```

Useful metadata:

### Container

- ID
- name
- image
- state
- created time
- start time
- labels
- compose project
- compose service
- ports
- mounts
- writable layer size if available

### Image

- ID
- tags
- created time
- size
- dangling
- currently referenced by container
- last-known use if derivable

### Volume

- name
- driver
- labels
- mount point
- size if available
- currently attached
- previous container references where discoverable
- compose project label

### Network

- name
- driver
- attached containers
- compose project

### Build cache

- size
- reclaimable
- age where supported

---

## 8.3 Docker cleanup rules

Actions:

```text
x
```

may clean selected Docker resources.

The UI MUST distinguish:

```text
safe-ish
caution
persistent-data-risk
```

Examples:

### Dangling image

```text
Remove dangling image?

sha256:...
1.4 GB

[y] remove
```

### Stopped container

```text
Remove stopped container?

old-api
310 MB
Exited 21 days ago

[y] remove
```

### Volume

```text
⚠ PERSISTENT DATA

oldproject_postgres
6.8 GB

Last associated container:
oldproject-db

Compose project:
oldproject

This volume may contain database data.

[D] Delete permanently
[Esc] Cancel
```

Volumes MUST never be automatically removed in V0/V1.

---

# 9. Runtime data model

Suggested internal models:

```rust
struct RuntimeSnapshot {
    system: SystemSummary,
    processes: Vec<ProcessInfo>,
    logical_items: Vec<RuntimeItem>,
    projects: Vec<Project>,
    ports: Vec<ListeningPort>,
    docker: Option<DockerSnapshot>,
    leftovers: Vec<Leftover>,
}
```

Process model:

```rust
struct ProcessInfo {
    pid: u32,
    parent_pid: Option<u32>,

    name: String,
    command: Vec<String>,
    executable: Option<PathBuf>,
    cwd: Option<PathBuf>,

    cpu_percent: f32,
    memory_bytes: u64,

    start_time: u64,
    tty: Option<String>,

    listening_ports: Vec<ListeningPort>,
}
```

Logical item:

```rust
struct RuntimeItem {
    id: RuntimeId,

    category: Category,
    display_name: String,

    root_pid: Option<u32>,
    process_ids: Vec<u32>,

    project: Option<ProjectId>,
    origin: Option<Origin>,

    memory_bytes: u64,
    cpu_percent: f32,

    ports: Vec<ListeningPort>,

    state: RuntimeState,
    suspicion: Option<Suspicion>,
}
```

Categories:

```rust
enum Category {
    Agent,
    Mcp,
    Browser,
    DevServer,
    LanguageServer,
    Database,
    DevService,
    Container,
    UnknownDev,
}
```

State:

```rust
enum RuntimeState {
    Active,
    Persistent,
    Suspicious,
}
```

---

# 10. Process scanning

## 10.1 Required information

Scanner should collect when available:

- PID
- PPID
- process name
- complete command arguments
- executable path
- cwd
- CPU
- RSS
- start time
- TTY/session information
- process group/session where useful

macOS/Linux capabilities differ.

Platform backends may expose different optional fields.

The internal model must tolerate missing values.

---

## 10.2 Performance

The process scanner MUST NOT create a new external command for each process.

Prefer direct APIs/libraries.

Scanner should keep reusable process/system state where necessary for CPU deltas.

---

# 11. Port scanning

wyd should associate listening sockets with PIDs.

Required fields:

```text
protocol
address
port
pid
```

Example:

```text
TCP 127.0.0.1:5173 PID 71231
```

This maps back into logical items:

```text
Vite :5173 → PID 71231 → project databoundary
```

Port scanning should avoid repeatedly shelling out to `lsof` during normal refresh.

Fallbacks may be acceptable if native access is unavailable.

---

# 12. Classification

Classification is the core product logic.

wyd should classify using multiple signals:

1. executable/name;
2. command arguments;
3. parent/ancestor tree;
4. child tree;
5. cwd;
6. known ports;
7. environment where safely available;
8. Docker labels;
9. project metadata.

No AI model is required.

---

## 12.1 Example rules

### MCP

```text
command contains "chrome-devtools-mcp"
→ Mcp
```

```text
command contains "playwright-mcp"
→ Mcp
```

```text
command/path contains "-mcp" or "mcp-server"
→ candidate MCP
```

Broad matches must be lower-confidence than exact known signatures.

---

### Dev server

```text
command contains node_modules/.bin/vite
→ Vite dev server
```

```text
command contains "next dev"
→ Next.js dev server
```

```text
uvicorn + listening port + project cwd
→ Python dev server
```

---

### Browser

```text
browser descendant of known MCP/Playwright/Puppeteer
→ dev Browser
```

Normal desktop browser roots should be ignored by default.

---

### Language server

Recognize commands such as:

```text
copilot-language-server
typescript-language-server
pyright-langserver
gopls
rust-analyzer
```

Whether all language servers are visible by default may later become configurable.

---

# 13. Context propagation

A key feature is propagating context down the process tree.

Example:

```text
omp
└─ npm exec chrome-devtools-mcp
   └─ node
      └─ Chromium
         ├─ renderer
         ├─ gpu
         └─ utility
```

Derived information:

```text
origin  = omp
project = project associated with omp / MCP cwd
browser = chrome-devtools-mcp browser group
```

The UI should collapse internal helper processes unless the user opens full details.

---

# 14. Project detection

Project detection should be cached.

Potential signals:

1. process cwd;
2. ancestor cwd;
3. command path;
4. Git root;
5. Docker Compose labels;
6. package/project files;
7. user overrides.

Primary algorithm:

```text
cwd
→ walk upward
→ find .git
→ project root
```

Fallback project markers may include:

```text
package.json
pyproject.toml
Cargo.toml
go.mod
composer.json
docker-compose.yml
compose.yml
```

Git root takes precedence where available.

Repeated paths must use cache:

```rust
HashMap<PathBuf, Option<ProjectId>>
```

No repeated `git rev-parse` per process on every refresh.

---

# 15. Origin and ownership

## 15.1 Live ancestry

If the tree is alive:

```text
Ghostty → zsh → omp → MCP → Chromium
```

ownership is straightforward.

---

## 15.2 Lost ancestry

After the original process exits, the OS may re-parent descendants.

Example:

Before:

```text
Kimi/OpenCode
└─ MCP
   └─ Chromium
```

Later:

```text
launchd/systemd
└─ Chromium
```

wyd cannot always reconstruct historical origin from the current process table.

Therefore V0/V1 should display confidence honestly:

```text
Origin: unknown / detached
```

or:

```text
Likely origin: chrome-devtools-mcp
```

Do not fabricate certainty.

---

# 16. Leftover detection

A leftover is a heuristic result.

Suggested structure:

```rust
struct Suspicion {
    score: u8,
    reasons: Vec<SuspicionReason>,
}
```

Possible reasons:

```text
ParentExited
NoTerminalAncestor
OwningAgentMissing
McpOwnerMissing
HeadlessBrowserDetached
LongRunningDevServer
ProjectInactive
StoppedContainerOld
DanglingImage
UnusedNetwork
BuildCacheReclaimable
```

---

## 16.1 Example scoring

Illustrative only:

```text
+50 owning agent missing
+40 parent gone / re-parented
+30 headless browser
+25 MCP server without owner
+20 dev server older than threshold
+15 no terminal ancestor
```

Statuses could be:

```text
0–29   active/normal
30–59  unusual
60+    likely leftover
```

Exact scoring should be tuned from real-world data.

---

## 16.2 Persistent exceptions

Do NOT classify these as leftovers merely because they are long-running:

- Homebrew services
- systemd services
- Laravel Valet
- user-configured persistent services
- Docker Desktop
- explicitly pinned processes

Persistent infrastructure should appear as:

```text
○ persistent
```

rather than:

```text
⚠ suspicious
```

---

# 17. Kill semantics

## 17.1 Process kill

Normal action:

```text
k
```

Flow:

1. freeze selected logical item;
2. resolve current PIDs;
3. verify process start times;
4. show descendants;
5. ask for confirmation;
6. send graceful termination;
7. refresh;
8. if survivors remain, offer force termination.

---

## 17.2 Force kill

Force action:

```text
K
```

must be explicitly separate.

Example:

```text
2 processes ignored SIGTERM.

[K] Force kill
[Esc] Cancel
```

---

## 17.3 Tree termination

Do not blindly kill a Unix process group unless wyd is certain the process group belongs entirely to the selected runtime item.

Safer default:

```text
deepest descendants first
→ children
→ root
```

Before each signal, confirm identity using PID plus start time where available.

---

# 18. Docker actions

Actions should call the Docker API directly.

Potential actions:

```text
stop container
remove stopped container
remove image
remove network
remove build cache entry/group
remove volume
```

Default destructive behavior must require user interaction.

Bulk selection may be supported via Space.

Example:

```text
[x] dangling images                 1.7 GB
[x] build cache older than 7 days   6.1 GB
[x] stopped containers              1.2 GB
[ ] unused images                   4.8 GB
[ ] unused volumes                  5.1 GB

Potentially freed: 9.0 GB
```

Volumes remain high-risk even in multi-select.

---

# 19. Scanner architecture

The UI should never perform expensive scans directly.

Suggested model:

```text
                   ┌──────────┐
                   │   TUI    │
                   └────┬─────┘
                        │ read
                  latest snapshot
                        │
       ┌────────────────┼────────────────┐
       │                │                │
 Processes          Ports           Docker
 scanner            scanner          scanner
```

Use shared immutable snapshots or lock-minimized state.

Potential Rust shape:

```rust
Arc<RwLock<RuntimeSnapshot>>
```

or a channel-based state updater.

---

# 20. Refresh cadence

Not every subsystem needs the same frequency.

Illustrative defaults:

```text
process list       frequent
CPU/RAM            frequent
ports              moderate
containers         moderate
images/volumes     slow
Docker disk usage  slow/manual
project discovery  cached
```

The exact intervals should be benchmarked.

The dashboard should render immediately using available process data while slower Docker metadata loads asynchronously.

---

# 21. TUI implementation

Suggested stack:

```text
ratatui
crossterm
```

Suggested modules:

```text
src/
├── main.rs
├── app.rs
├── config.rs
│
├── scanner/
│   ├── processes.rs
│   ├── ports.rs
│   ├── services.rs
│   ├── docker.rs
│   └── mod.rs
│
├── classify/
│   ├── rules.rs
│   ├── tree.rs
│   ├── project.rs
│   ├── origin.rs
│   ├── leftovers.rs
│   └── mod.rs
│
├── model/
│   ├── process.rs
│   ├── runtime.rs
│   ├── project.rs
│   ├── docker.rs
│   └── snapshot.rs
│
├── actions/
│   ├── process.rs
│   ├── docker.rs
│   └── mod.rs
│
├── platform/
│   ├── macos.rs
│   ├── linux.rs
│   └── mod.rs
│
└── ui/
    ├── overview.rs
    ├── list.rs
    ├── details.rs
    ├── confirm.rs
    ├── help.rs
    └── mod.rs
```

---

# 22. Suggested Rust dependencies

Indicative only:

```toml
[dependencies]
ratatui = "..."
crossterm = "..."
sysinfo = "..."
tokio = { version = "...", features = ["rt-multi-thread", "macros", "time"] }
clap = { version = "...", features = ["derive"] }
serde = { version = "...", features = ["derive"] }
serde_json = "..."
toml = "..."
bollard = "..."
```

Socket/process implementation may use:

```text
netstat2
```

or platform-specific native APIs if more reliable.

Dependency choices should remain replaceable behind scanner traits.

---

# 23. Scanner traits

A useful abstraction:

```rust
trait ProcessScanner {
    fn scan(&mut self) -> Result<Vec<ProcessInfo>>;
}

trait PortScanner {
    fn scan(&mut self) -> Result<Vec<ListeningPort>>;
}

trait DockerScanner {
    async fn scan(&self) -> Result<DockerSnapshot>;
}
```

Platform details should remain outside classification logic.

---

# 24. Rules and configuration

Built-in rules should cover popular tooling.

User config:

```text
~/.config/wyd/config.toml
```

Example:

```toml
[agents.custom-agent]
names = ["myagent"]
command_contains = ["my-company-agent"]

[mcp.internal]
command_contains = ["internal-mcp"]

[persistent]
commands = [
  "my-local-daemon"
]

[projects]
roots = [
  "~/Work",
  "~/Projects"
]
```

A user must be able to:

- add agent signatures;
- add MCP signatures;
- add persistent-service exemptions;
- hide categories;
- adjust suspicious-age thresholds.

---

# 25. Built-in classifier coverage

Initial useful signatures should include common examples from these groups.

## Agents

- OpenCode
- OMP
- Claude Code
- Codex
- Aider
- common agent runners

## MCP

- chrome-devtools-mcp
- playwright-mcp
- filesystem MCP
- context7
- package commands containing known MCP server names
- custom MCP executables via config

## JS runtimes/dev

- node
- npm
- npx
- pnpm
- yarn
- bun
- deno
- vite
- next
- nuxt
- astro

## Python

- python
- uvicorn
- gunicorn
- flask
- Django runserver

## PHP

- php
- php-fpm
- Laravel-related servers
- Valet

## Databases

- postgres
- mysqld
- mariadbd
- redis-server
- mongod

## Language servers

- copilot-language-server
- typescript-language-server
- pyright
- rust-analyzer
- gopls

## Browser tooling

- Chromium
- Chrome headless
- Playwright
- Puppeteer
- Selenium

Classification should rely on full context rather than executable name alone.

---

# 26. Services

macOS:

- Homebrew services
- launchd context where safely detectable
- Laravel Valet identification

Linux:

- systemd user/system services where useful

wyd should not turn into a generic service manager.

Service data exists mainly to answer:

```text
Is this expected to be persistent?
```

Example:

```text
PostgreSQL :5432
○ persistent — Homebrew service
```

---

# 27. Non-interactive CLI

Required eventually:

```bash
wyd --plain
wyd --json
```

Potential filters:

```bash
wyd --json mcp
wyd --json leftovers
wyd --json docker
wyd --json project queryknight
```

JSON must be stable enough for external tools/agents.

Illustrative:

```json
{
  "runtime": [
    {
      "type": "mcp",
      "name": "chrome-devtools-mcp",
      "pid": 94148,
      "project": "queryknight",
      "memory_bytes": 50331648,
      "status": "active"
    }
  ]
}
```

---

# 28. Agent integration

A future agent workflow:

```text
1. Agent starts task.
2. Agent runs servers/MCP/browser.
3. Agent completes task.
4. Agent runs:
   wyd --json leftovers
5. Agent detects resources it owns.
6. Agent offers or performs scoped cleanup.
```

wyd itself should remain independent of any specific model/provider.

---

# 29. History / provenance — future V2

Current process ancestry disappears when parent processes exit.

For exact historical provenance, a future optional watcher may be introduced.

Possible command:

```bash
wyd watch
```

or optional user daemon:

```text
wydd
```

The watcher should record only lightweight metadata:

```text
PID
process start time
PPID
command hash / compact command
cwd
first seen
last seen
derived project
derived origin
```

No process output capture.

No keystroke capture.

No file-content monitoring.

No network-content inspection.

Example capability:

```text
⚠ Chromium ×8

Current parent:
launchd

Originally observed:
Kimi/OpenCode
└─ chrome-devtools-mcp
   └─ Chromium

Agent exited:
1h 42m ago
```

This is a strong future feature but NOT required for V0.

---

# 30. Privacy

wyd is local-first.

Default behavior:

- no telemetry;
- no cloud upload;
- no command history upload;
- no account;
- no AI API;
- no remote storage.

Commands can contain sensitive arguments.

The TUI may display them locally, but any future diagnostics/export feature must consider redaction.

---

# 31. Security

wyd performs destructive actions, so safeguards matter.

Required:

- explicit confirmation for destructive operations;
- SIGTERM before SIGKILL;
- PID identity re-check;
- clear process-tree preview;
- stronger confirmation for persistent Docker volumes;
- no default `sudo`;
- no silent privilege escalation;
- graceful handling of inaccessible processes.

The tool should work usefully without root privileges.

If elevated privileges would expose more information, explain that rather than requiring them.

---

# 32. Error handling

Scanner failures should not crash the TUI.

Examples:

```text
Docker
○ not running
```

```text
Ports
⚠ partial — some process ownership unavailable
```

```text
Process details
permission denied
```

Subsystems should fail independently.

---

# 33. Performance targets

Initial aspirational targets on a normal developer laptop:

- TUI visible almost immediately;
- first process snapshot well under a second;
- no visible blocking while Docker metadata loads;
- low idle CPU;
- low tens of MB RAM where practical;
- no hundreds of subprocess spawns;
- responsive navigation regardless of scanner refresh.

Exact targets should be established with benchmarks after the first prototype.

---

# 34. Distribution

Primary release format:

- single native binaries.

Targets:

```text
aarch64-apple-darwin
x86_64-apple-darwin
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
```

Potential distribution:

### Homebrew tap

```bash
brew install <owner>/tap/wyd
```

### Cargo

```bash
cargo install wyd
```

### Shell installer

```bash
curl -LsSf <installer> | sh
```

### GitHub Releases

Prebuilt archives/binaries with checksums.

---

# 35. Naming

Name: **`wyd`** — "what you doing?", the question the tool asks every process.

Verified free (2026-08): crates.io, Homebrew, default PATH. npm `wyd` is squatted (0.0.1 placeholder) but irrelevant — distribution is brew/cargo/binaries per §34.

Rejected candidates:
- `wtf` — brew formula taken by bsdwtf (acronym translator), crates.io taken by a profiling crate
- `wat` — crates.io taken by the WebAssembly Text parser (wasmtime); strong .wat association
- `vibes`, `uhoh`, `smh` — free, but weaker fit

---

# 36. MVP / roadmap

## V0.1 — useful prototype

Required:

- Rust binary;
- Ratatui dashboard;
- macOS support;
- process scanner;
- process ancestry;
- CPU/RAM;
- cwd;
- command;
- basic project detection;
- listening ports;
- basic classifier;
- categories:
  - Agents
  - MCP
  - Browsers
  - Dev servers
  - Databases
  - Language servers
- detail view;
- graceful process kill;
- manual refresh.

Success criterion:

> wyd reduces a noisy macOS development process table to a small, understandable list of development runtime items.

---

## V0.2 — Docker

Add:

- Docker socket detection;
- running/stopped containers;
- images;
- dangling images;
- volumes;
- networks;
- build cache;
- disk/reclaimable totals;
- Docker detail view;
- safe cleanup actions;
- Compose project mapping.

Success criterion:

> User can understand both live dev processes and Docker leftovers from the same TUI.

---

## V0.3 — projects + leftovers

Add:

- stronger project grouping;
- Git-root cache;
- context propagation;
- persistent-service classification;
- leftover scoring/reasons;
- `Leftovers` view;
- RAM/disk waste estimate.

Success criterion:

> wyd can answer “what did my last development/agent sessions leave behind?”

---

## V0.4 — CLI automation

Add:

```text
--plain
--json
filters
```

Success criterion:

> Agents and scripts can consume wyd output.

---

## V1.0

Polish:

- macOS + Linux;
- robust classifier;
- configurable rules;
- stable JSON;
- Homebrew release;
- prebuilt binaries;
- tests;
- safe multi-select cleanup;
- good documentation;
- useful help UI.

---

## V2

Potential:

- optional history watcher;
- provenance history;
- better agent-session grouping;
- shell integration;
- explicit session registration API;
- plugin/rule registry;
- Windows support.

Do not commit to these before V1 proves useful.

---

# 37. Testing strategy

## Unit tests

- classifier rules;
- project detection;
- ancestry propagation;
- leftover scoring;
- Docker safety logic;
- process identity checks.

## Fixture tests

Capture synthetic process trees such as:

```text
OMP
└─ npm
   └─ chrome-devtools-mcp
      └─ Chromium ×8
```

and verify logical grouping.

Fixtures should cover:

- active agent tree;
- detached MCP;
- detached Chromium;
- persistent database;
- old Vite server;
- multiple agents using same project;
- multiple language servers;
- Docker Compose project;
- dangling Docker resources.

## Integration tests

Platform-specific process spawning.

Example test program:

```text
spawn parent
→ spawn fake MCP
→ spawn child server
→ verify scanner tree
→ terminate parent
→ verify leftover classification
```

Docker integration tests should run only where Docker is available.

---

# 38. Initial implementation sequence

Recommended order:

### Step 1

Create Rust project and render static Ratatui layout.

### Step 2

Implement process snapshot:

```text
PID
PPID
name
command
cwd
RAM
CPU
start time
```

### Step 3

Build ancestry tree.

### Step 4

Add classifier and hide irrelevant OS processes.

### Step 5

Add logical grouping:

```text
MCP → Chromium ×N
```

### Step 6

Add port/PID mapping.

### Step 7

Add project detection.

### Step 8

Add process details and safe kill.

At this point the tool is already useful.

### Step 9

Add Docker scanner.

### Step 10

Add Docker cleanup.

### Step 11

Add leftover heuristics.

### Step 12

Add JSON/plain output and distribution.

---

# 39. Definition of done for the first usable release

On a real developer Mac/Linux machine with:

- multiple terminal sessions;
- AI coding agents;
- MCP servers;
- Chromium;
- Vite/Node;
- local database;
- Docker containers;
- stale Docker resources;

running:

```bash
wyd
```

should produce a useful overview without the user needing:

```bash
ps
top
lsof
docker ps
docker images
docker volume ls
docker system df
brew services list
```

The user should be able to:

1. understand what is active;
2. identify ownership/project;
3. identify suspicious leftovers;
4. inspect details;
5. terminate a process tree safely;
6. remove selected Docker garbage safely.

That is the core product.
