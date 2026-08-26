# Changelog

All notable changes to wyd.

## [Unreleased]

### Fixed
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