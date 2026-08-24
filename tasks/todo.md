# wyd — Work Plan

Source of truth: `SPEC.md` (§-references below). Milestones follow SPEC §36/§38.

Name locked: `wyd` (crate + binary). Verified free on crates.io, Homebrew, local PATH; npm taken (irrelevant — distribution is brew/cargo/binaries per SPEC §34). "wyd?" = asking every process what it's doing — matches the origin/ancestry feature.
- macOS first, Linux backend at V1.0
- Deps per SPEC §22: ratatui, crossterm, sysinfo, tokio, clap, serde/toml, bollard

---

## M0 — Scaffold (SPEC §38 step 1)

- [x] `cargo init` binary crate `wyd`; deps: ratatui 0.30, crossterm 0.29, clap 4.6 (rest of §22 added when needed)
- [x] Static ratatui layout matching SPEC §5.1 mockup (two panels + footer key hints), hardcoded data
- [x] `clap` CLI skeleton (`wyd`, no flags yet)
- [x] CI: fmt + clippy + test on macOS/Linux
- [x] PTY smoke test: render verified, `q` exits 0; TestBackend layout test in main.rs

**Done when:** `cargo run` renders the mockup layout; q quits.

## M1 — Process snapshot + tree (V0.1 core, §38 steps 2–3)

- [ ] `model/process.rs` — `ProcessInfo` per SPEC §9
- [ ] `scanner/processes.rs` — sysinfo-based scan (pid, ppid, name, cmd, cwd, RAM/CPU, start_time, tty); no subprocess spawning (§10.2)
- [ ] Ancestry tree builder (`classify/tree.rs`)
- [ ] `Arc<RwLock<RuntimeSnapshot>>` + background scanner thread; TUI renders latest snapshot (§19)
- [ ] Unit tests: tree building from fixture process lists

**Done when:** TUI lists real live processes with CPU/RAM, refresh on `r`, tree indentation correct.

## M2 — Classifier + grouping (§38 steps 4–5)

- [ ] `classify/rules.rs` — signature table for SPEC §25 groups (agents, MCP, JS/Python/PHP dev, DBs, language servers, browser tooling)
- [ ] `Category` enum + `RuntimeItem` grouping (MCP → Chromium ×N rollups)
- [ ] Hide non-dev OS processes from default view
- [ ] Fixture tests: synthetic trees per SPEC §37 (agent→MCP→Chromium, detached MCP, old Vite, multi-agent)

**Done when:** noisy process table reduces to the small categorized list (V0.1 success criterion, §36).

## M3 — Ports + projects (§38 steps 6–7)

- [ ] `scanner/ports.rs` — listening socket → PID mapping (netstat2 or native), moderate cadence (§20)
- [ ] `classify/project.rs` — project detection: cwd → args → git root; cached (§14)
- [ ] Ports view; project shown in details
- [ ] Unit tests: port/PID join, project detection rules

**Done when:** dev servers show `:port` + project path (e.g. `Vite :5173 ~/Work/x`).

## M4 — Details + safe kill (§38 step 8) — **V0.1 complete**

- [ ] Details view (§7): PID/PPID, RAM/CPU, uptime, project, owner, children
- [ ] `k` graceful SIGTERM → confirm dialog (§17.1); `K` force SIGKILL (§17.2)
- [ ] PID + start_time revalidation before signaling (§17 PID-reuse guard)
- [ ] Tree termination with process-group safety (§17.3)
- [ ] Integration test: spawn tree → scan → kill → verify gone (§37)

**Done when:** V0.1 success criterion met on a real macOS session.

## M5 — Docker (V0.2, §8)

- [ ] `scanner/docker.rs` — bollard via local socket; socket-absent → `Docker ○ not running` degraded state (§32)
- [ ] Containers/images/dangling/volumes/networks/build-cache + disk totals (§8.2)
- [ ] Docker detail view; Compose project mapping via labels
- [ ] `x` cleanup actions, resource-by-resource, confirmations; volumes get PERSISTENT DATA warning (§8.3) — never assume unused volume is garbage
- [ ] Integration tests gated on Docker availability (§37)

**Done when:** processes + Docker leftovers visible in one TUI (V0.2 criterion).

## M6 — Leftovers (V0.3, §16)

- [ ] `classify/leftovers.rs` — scoring signals (dead parent, re-parented, no tty ancestor, old server) with **reason strings** (§4.2)
- [ ] Persistent-service exemptions (Homebrew DBs etc., §16.2) + `[persistent]` config
- [ ] Leftovers view + RAM/disk waste estimate
- [ ] `~/.config/wyd/config.toml` loading (§24): custom signatures, project roots, thresholds

**Done when:** "what did my last agent session leave behind?" is answerable (V0.3 criterion).

## M7 — CLI automation (V0.4, §27)

- [ ] `--plain` and `--json` output; subcommands/filters (`wyd --json leftovers`, `mcp`, …)
- [ ] Snapshot reuse: same scanners, no TUI
- [ ] JSON schema stability notes in README

**Done when:** agents/scripts can consume wyd output (V0.4 criterion).

## M8 — V1.0 polish

- [ ] Linux `platform/linux.rs` backend (socket/process APIs differ, §platform scope)
- [ ] Configurable keybindings; help UI (`?`)
- [ ] Multi-select cleanup
- [ ] Release: prebuilt binaries (4 targets, §34), Homebrew tap, `cargo install`
- [ ] Full test pass: unit + fixture + integration (§37)

---

## Working agreements

- Follow §38 order; don't pull Docker (M5) earlier — tool is useful at M4.
- Every scanner failure degrades UI, never crashes it (§32).
- No LLM/telemetry/daemon/network calls — ever (§3.2).
- Tests required where SPEC §37 names them; fixtures preferred over live-system tests.
- Log decisions + corrections in `tasks/lessons.md` as they happen.

## Review

(to fill after each milestone: what shipped, deviations from plan, lessons)
