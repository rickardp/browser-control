# Changelog

## Unreleased

### Added

- `browser-control set default <value>` / `get default` / `unset default`:
  persist a default browser used by `browser-control mcp` when no positional
  argument and no `BROWSER_CONTROL` env var is present. Values accept the full
  `BROWSER_CONTROL` grammar (URL, kind, friendly name, absolute path) and are
  validated at set-time. Stored as TOML in the OS config dir
  (`BROWSER_CONTROL_CONFIG_DIR` overrides the location).

### Changed

- `browser-control mcp`: the positional `BROWSER` argument now overrides the
  `BROWSER_CONTROL` environment variable (previously env took precedence).
  Clap merges them, and `--help` advertises the `BROWSER_CONTROL` env binding.

## 0.2.1 - 2026-05-13

### Fixed

- `browser-control start` now fully detaches the spawned browser from the
  parent shell. Previously the child inherited piped stdout/stderr; once
  the CLI exited, the read-ends closed and the next stderr write from the
  browser produced `SIGPIPE` and killed it. Symptom: `start` printed the
  PID, then `list-running` immediately showed nothing.
  - Stdout/stderr are now redirected to `<profile>/browser.log` instead
    of being piped to the parent.
  - On Unix the child calls `setsid(2)` in a `pre_exec` hook so it
    becomes its own session leader (PPID=1, own PGID). Terminal signals
    sent to the parent's process group no longer reach the browser.

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
