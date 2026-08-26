# Changelog

All notable changes to wyd.

### Added
- Runtime-ownership foundation: agent runtime sessions keyed by `boot_id + pid + start_time`, exact observed ownership over the process tree, durable SQLite provenance that survives process ancestry loss and Wyd restarts, and a deterministic ownership resolver (`wyd why` explains a process's origin session and attribution).
- `wyd why <pid>`: reconstruct a process's origin session and attribution from recorded provenance.
- `wyd --json sessions`: list recorded agent sessions.
- Session-aware leftovers: a resource whose origin session ended (and that is not persistent) is flagged as a leftover in both the TUI and CLI (`--json`/`--plain`).
- The attribution resolver now runs live in the tracker: it persists a per-resource decision (re-affirming the recorded owner) so `wyd why` shows the attribution, including the raw evidence (cwd match, start-time correlation, tool relationship).
- A new top-level **Sessions** view in the TUI lists recorded agent sessions (agent · project · state · age · id) with a details panel; sessions now record the agent's project so the resolver's project evidence works.
- `wyd serve`: a local read-only Unix-socket API (daemon) exposing `list_sessions`, `get_session`, `explain`, plus vendor `session_start`/`session_end` registration (vendor ids map to Wyd sessions as aliases).
- `wyd mcp`: a minimal MCP server over stdio exposing Wyd ownership queries (`list_sessions`, `explain`) to coding agents.
- New **Workers** category for background watchers/queues: Celery, Sidekiq, Laravel `horizon`/`queue:work`, nodemon, cargo-watch, watchexec, air, and `tsc`/`tailwindcss --watch`. They score as leftovers by age, like dev servers.
- Detection for agents: Amp, Crush, Goose, Qwen Code, Factory Droid, Kiro, Antigravity (`agy`), Pi.
- Detection for databases: Elasticsearch, OpenSearch, ClickHouse, CockroachDB, Cassandra, Memcached, Neo4j, Qdrant, Weaviate, Milvus, Meilisearch, Typesense, InfluxDB, SQL Server.
- Detection for language servers: Vue, Svelte, Tailwind, ESLint, YAML, Bash, Docker, jdtls, Ruby (`ruby-lsp`/`solargraph`), nixd, nil, Biome (`lsp-proxy` only).
- Semantic dev-server labels: Astro, SvelteKit, Remix/React Router, Parcel, Rsbuild/Rspack, Nest, Puma, Rails, Phoenix.

### Changed
- Runtime store uses WAL + `synchronous=NORMAL` + a 2 s busy timeout, so concurrent readers (TUI/CLI/MCP) don't block the `wyd serve` writer.
- `wyd serve` is single-instance (refuses to start if another is running), writes a PID file, and restricts the socket to the owner (`0600`).
- Vendor `session_end` is metadata on the session alias, not a runtime-session end; a runtime session ends only when its process exits.
- `wyd mcp` pins the MCP protocol version (2025-11-25) and returns a proper JSON-RPC error for unknown methods.
- Databases are no longer unconditionally `persistent`. A database is persistent only when run as a service (Homebrew/Docker/Valet path, `persistent.commands`, or parented to launchd/systemd/init); an agent- or shell-spawned DB (`postgres -D /tmp/test-db`) is session-scoped and can become a leftover. State is now derived in `mark`, not at group time.

### Fixed
- macOS boot identity now uses `kern.bootsessionuuid` (stable per boot) instead of the clock-derived `kern.boottime`, so an NTP clock adjustment no longer reads as a new boot and wipes session provenance.
- Detached headless Chromium now surfaces as a leftover item instead of silently disappearing.
- Docker snapshot is Arc-cloned (O(1)) instead of full clone every 2 s.
- Leftover scoring uses HashMap lookup instead of linear scan for ancestor processes.
- `KeysConfig::hit` is exact match (was `starts_with`).
- Project filter is case-insensitive in TUI (already was in CLI).
- Named volumes are no longer flagged as leftover in `--json leftovers`.
- Ports are deduplicated in the Ports view.
- `prune --dry-run` reports `docker not running` when the daemon is down.
- `XDG_CONFIG_HOME` is respected on Linux.
- `count_projects` is O(n) instead of O(n²).
- `with_disk_usage` flag dropped (unused sysinfo data).

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