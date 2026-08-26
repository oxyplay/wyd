# Changelog

All notable changes to wyd.

## [Unreleased]

### Added
- Vim navigation: `j`/`h`/`l` move (up is `k`, which is kill by default — remap `kill` in config to free it).
- `Tab` toggles focus between overview and list.
- `backspace` goes back (clears filter → project → section) without quitting.
- Anonymous unused volumes collapse into one summary row in the Docker section.
- `wyd prune` CLI (mirrors the `P` key): `--dry-run` lists what would be deleted, `--yes` skips the prompt.
- `--json` docker entries now include `anonymous` and `created`.

## [0.4.3] - 2026-08-25

### Added
- `P` prunes unused anonymous volumes with a confirmation dialog; named volume data is always kept.
- Confirmation dialogs are now dedicated popups with the action keys visible inside.
- Footer hint line is context-aware and brighter.
- `wyd prune` groundwork (engine-side `all=false` filter).
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
