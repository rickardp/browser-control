# Changelog

## 0.2.0 - 2026-05-13

First Rust release. Establishes the crate on crates.io and seeds the
in-repo Homebrew tap. CI handles all subsequent releases end-to-end.

### Breaking

- Project rewritten in Rust as a CLI. The previous TypeScript MCP server is
  archived on the `legacy-ts` branch (tagged `v0-final-ts`). The npm package
  `@anthropic-community/browser-coordinator-mcp` is deprecated.

### Added

- `browser-control list-installed`, `list-running`, `start`, `mcp` subcommands.
- Persistent SQLite registry of running browsers.
- Friendly per-instance names (e.g. `firefox-pikachu`).
- `BROWSER_CONTROL` environment variable for session-level browser selection.
- `mcp --playwright` stdio passthrough to `@playwright/mcp`.

### Removed

- `coordinator_launch_browser`, `coordinator_stop_browser`,
  `coordinator_restart_browser` (browser lifecycle is now CLI-only).
- `coordinator_get_markdown` (no Turndown port in v1).
- CDP reverse proxy (replaced by direct connection or `mcp --playwright`).
- VS Code companion extension's Unix-socket IPC (replaced by env-var
  injection).
