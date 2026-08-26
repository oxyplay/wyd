# Changelog

All notable changes to wyd.

## [0.6.0] - 2026-08-26

### Added
- **Explicit status classes in the TUI**: the status column now separates `leftover` (warn + `⚠`), `persistent` (`◆`), and `owned` (green, for resources under a live agent), instead of only highlighting leftovers.
- **`wyd mcp` conformance test**: drives the real binary through the full stdio initialize-based handshake as a client, guarding the wire protocol against drift.
- **Fixture-based tests** for classify / ownership / resolver, leftover reasons (agent alive, agent died, reparent-to-init), and a macOS regression that the boot id prefers the stable `kern.bootsessionuuid` over clock-derived `kern.boottime` (an NTP adjustment never reads as a new boot).
- The `?` help screen now lists the kill / force-kill / stop / clean keys (previously omitted).

### Changed
- **Canonical homepage `https://wyd.sh`** in Cargo metadata and the Homebrew formula; `repository` stays GitHub. The README links it right after the description.
- **Automated release pipeline**: a `v*` tag now builds the four binaries, publishes the crate to crates.io, and bumps `Formula/wyd.rb` in `oxyplay/homebrew-tap` (all platform URLs + sha256s) with no manual step — so `wyd upgrade` stays current.

## [0.5.0] - 2026-08-26

### Added
- **Runtime ownership & durable provenance**: coding-agent runtime sessions keyed by `boot_id + pid + start_time`, exact observed ownership over the process tree, and SQLite provenance that survives process re-parenting and Wyd restarts.
- **Sessions** view in the TUI (top-level): agent · project · state · age · id, with a details panel.
- **`wyd why <pid>`**: which session owns a process, with the evidence behind the attribution.
- **`wyd --json sessions`**: recorded sessions as JSON.
- **Session-aware leftovers**: a resource whose origin session ended is flagged as a leftover (TUI and CLI).
- **`wyd serve`**: a local daemon over a Unix socket — read queries plus vendor `session_start` / `session_end` registration (a vendor session id maps to a Wyd session as an alias).
- **`wyd mcp`**: a Model Context Protocol server over stdio, so a coding agent can ask wyd for its sessions and who owns a PID.
- **Workers** category for background watchers/queues (Celery, Sidekiq, Laravel horizon, nodemon, cargo-watch, watchexec, air, `tsc`/`tailwindcss --watch`), scored as leftovers by age.
- Expanded agent detection (Amp, Crush, Goose, Qwen Code, Factory Droid, Kiro, Antigravity, Pi), database detection (Elasticsearch, ClickHouse, CockroachDB, Cassandra, Qdrant, …), language servers (Vue, Svelte, Tailwind, jdtls, nil, Biome, …), and semantic dev-server labels (Astro, SvelteKit, Remix, Nest, Rails, Phoenix, …).

### Changed
- Safer Docker cleanup: named volumes are never treated as garbage; only anonymous, unattached ones are offered for pruning.
- Runtime store uses WAL + `synchronous=NORMAL` + a busy timeout, so concurrent readers (TUI/CLI/MCP) don't block the `wyd serve` writer.
- `wyd serve` is single-instance and restricts its socket to the owner (`0600`).
- Databases are persistent only when run as a real service, not unconditionally.

### Fixed
- Stable macOS boot identity (`kern.bootsessionuuid`): an NTP clock adjustment no longer reads as a new boot and wipes session provenance.
- Detached headless Chromium surfaces as a leftover instead of silently disappearing.
- Docker snapshot is Arc-cloned (O(1)) instead of fully cloned every scan; leftover scoring and project counting are O(n).
- Named volumes are no longer flagged as leftover in `--json leftovers`; ports are deduplicated; `prune --dry-run` reports when the daemon is down.

## [0.4.4] - 2026-08-26

### Added
- Vim navigation: `j`/`k`/`h`/`l` + `Tab` focus + `backspace` back. Kill moved to `x`, docker clean to `c`.
- `wyd prune` CLI: `--dry-run` / `--yes`.
- `--json` docker entries now include `anonymous` and `created`.
- Anonymous unused volumes collapse into one summary row.
- CHANGELOG.md.

### Changed
- Confirmations are dedicated popups (not embedded tables).
- Footer hint is context-aware and brighter.
- Details mode keys use config now (not hardcoded).

## [0.4.3] - 2026-08-25

### Added
- `P` prunes unused anonymous volumes with a confirmation dialog; named volume data is always kept.
- Confirmation dialogs are now dedicated popups with the action keys visible inside.
- Footer hint line is context-aware and brighter.
- Server URLs are clickable/openable (`o` or click) from details.

### Fixed
- Ports now come only from real listening sockets — an `--port` in argv no longer claims a dead port.

## [0.4.2] - 2026-08-25

### Added
- `wyd upgrade` alias (brew or cargo, from the binary path).
- Terminal tab title shows running counts.
- Clickable `http://` dev-server URL in details.
- Language servers show their role, leftover state, and project; orphaned ones are flagged.

### Changed
- Docker: running containers sort first; `s` stops a running container; age/status split into separate columns.

## [0.4.1] - 2026-08-25

### Added
- Homebrew tap + `cargo install` docs.

## [0.4.0] - 2026-08-25

Initial public release: two-panel TUI, leftovers/ports/projects/docker, kill/clean, `--json`/`--plain`.