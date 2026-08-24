# Repository Guidelines

> **Status: pre-implementation.** This repo currently contains only `README.md` and `SPEC.md` — no source code, build files, or tests exist yet. `SPEC.md` (2069 lines) is the authoritative source of truth; treat its MUST/SHOULD statements as acceptance criteria. Sections below describe the *planned* implementation per the spec.

## Project Overview

**wyd** is a fast terminal UI (TUI, inspired by `lazygit`) for inspecting and safely cleaning up local development runtime state: AI coding agents, MCP servers, headless browsers, dev servers, databases, Docker resources, ports, and stale "leftover" processes. It answers: *what is still running, who started it, which project it belongs to, and can it be killed safely?*

Core principles (SPEC §4):
- **Semantic over exhaustive** — group process trees into logical items, never dump raw `ps` output.
- **Explain before acting** — every ⚠ suspicious item must show *why* (age, dead parent, etc.).
- **Conservative cleanup** — never silently destroy state; Docker volumes get extra-strong warnings (possible DB data); no `docker system prune -a --volumes` wrappers.
- **Origin matters** — track ancestry (`Ghostty → zsh → omp → chrome-devtools-mcp → Chromium ×8`), including re-parented/lost ancestry.

## Architecture & Data Flow

Planned native **Rust** binary. Scanners refresh independently; the TUI renders the latest snapshot and never scans directly (SPEC §19):

```
Processes scanner ─┐
Ports scanner      ┼──→ RuntimeSnapshot ──→ TUI
Docker scanner   ──┘        (Arc<RwLock<_>> or channel-updated)
```

Key flows:
1. **Scan** → `Vec<ProcessInfo>` (pid, ppid, name, command, cwd, RAM/CPU, start_time, tty) — MUST NOT spawn external commands (`ps`/`lsof`/`docker ps`) per refresh; use `sysinfo`/native APIs (SPEC §10.2).
2. **Classify** (core product logic, SPEC §12) → group processes into `RuntimeItem`s with `Category::{Agent, Mcp, Browser, DevServer, LanguageServer, Database, DevService, Container, UnknownDev}`.
3. **Context propagation** (SPEC §13) → project/origin inherited down the process tree; project detection via cwd, args, git root, Docker Compose labels (cached).
4. **Leftover scoring** (SPEC §16) → heuristic + explanation; persistent services (Homebrew DBs, etc.) are exempt.
5. **Actions** → kill (`k` graceful / `K` force / tree termination) and Docker cleanup (`x`) with confirmation; revalidate PID + start_time before signaling (PID-reuse guard, SPEC §17).

Refresh cadence differs per subsystem: processes/CPU frequent, ports/containers moderate, images/volumes slow, project discovery cached (SPEC §20).

## Key Directories

Planned layout (SPEC §21):

```
src/
├── main.rs, app.rs, config.rs
├── scanner/    # processes.rs, ports.rs, services.rs, docker.rs
├── classify/   # rules.rs, tree.rs, project.rs, origin.rs, leftovers.rs
├── model/      # process.rs, runtime.rs, project.rs, docker.rs, snapshot.rs
├── actions/    # process.rs, docker.rs (kill / cleanup)
├── platform/   # macos.rs, linux.rs — platform-specific scanner backends
└── ui/         # overview.rs, list.rs, details.rs, confirm.rs, help.rs
```

## Development Commands

No toolchain is set up yet. Once scaffolded (SPEC §38 Step 1: `cargo init` + static ratatui layout), standard Rust commands apply:

```bash
cargo build            # build
cargo run              # launch TUI (binary name: wyd)
cargo test             # all tests
cargo clippy           # lint
cargo fmt              # format
```

CLI surface (SPEC §27): `wyd` (TUI), `wyd --plain`, `wyd --json [leftovers|mcp|…]` for scripts/agents.

## Code Conventions & Common Patterns

- **Scanner traits** (SPEC §23) — keep platform details out of classification; deps replaceable behind traits:
  `ProcessScanner::scan() -> Result<Vec<ProcessInfo>>`, `PortScanner`, `async DockerScanner`.
- **Error handling** (SPEC §32) — scanner failures NEVER crash the TUI; subsystems fail independently and render degraded states (`Docker ○ not running`, `Ports ⚠ partial`). Use `Result`; no panics in scanner paths.
- **Async** — `tokio` (rt-multi-thread) for concurrent scanners; TUI stays responsive regardless of scanner state.
- **Config** (SPEC §24) — TOML at `~/.config/wyd/config.toml` via `serde`; users extend classifier rules (custom agent/MCP signatures, persistent-service exemptions, project roots, suspicion thresholds).
- **Safety invariants** — PID+start_time revalidation before any signal; graceful SIGTERM before SIGKILL; no blind process-group kills; unused Docker volumes are never assumed garbage.
- **Local-first** — no LLM, telemetry, daemon, network calls, or accounts (SPEC §3.2 non-goals). Keep it that way.

## Important Files

- `SPEC.md` — full product & technical spec; sections numbered (e.g. §12 classification, §16 leftovers, §37 testing). Cite these when discussing behavior.
- `README.md` — user-facing overview, TUI mockups, install story.
- (planned) `Cargo.toml`, `src/main.rs` — entry points once scaffolded.

## Runtime/Tooling Preferences

- **Rust** (stable), single static binary; macOS + Linux first, Windows later (SPEC §34).
- Suggested deps (SPEC §22, indicative not binding): `ratatui`, `crossterm`, `sysinfo`, `tokio`, `clap` (derive), `serde`/`serde_json`/`toml`, `bollard` (Docker Engine API via local socket), `netstat2` or native socket APIs.
- Distribution targets: `aarch64/x86_64-apple-darwin`, `x86_64/aarch64-unknown-linux-gnu`; Homebrew + `cargo install`.

## Testing & QA

Strategy per SPEC §37 — three tiers:
- **Unit tests**: classifier rules, project detection, ancestry propagation, leftover scoring, Docker safety logic, process-identity checks.
- **Fixture tests**: synthetic process trees (agent→MCP→Chromium, detached MCP, old Vite server, Docker Compose project, …) verifying logical grouping.
- **Integration tests**: real spawned process trees (spawn → scan → kill parent → verify leftover classification); Docker tests only where Docker is available.

Performance targets (SPEC §33): sub-second first snapshot, immediate TUI render, low idle CPU, no per-refresh subprocess spawning.

Implementation order (SPEC §38): static ratatui layout → process snapshot → ancestry tree → classifier → grouping → ports → projects → safe kill → Docker → leftovers → JSON/plain output. The tool is considered useful after Step 8.
