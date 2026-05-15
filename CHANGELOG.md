# Changelog

## 0.3.2 — 2026-05-15

### Fixed

- `browser-control wait --ready` and `list-running --json` enrichment
  used the raw `ws://host:port/devtools/browser/<id>` endpoint as the
  HTTP probe base, so the `/json/version` request never succeeded.
  `wait --ready` always timed out and `cdp_ws_url` / `bidi_ws_url` were
  silently `null`. Endpoints are now normalised to `http(s)://host:port`
  before probing.

## 0.3.1 — 2026-05-15

### Fixed

- Removed the `--profile` flag from `browser-control start`. Agents tended
  to pass a fresh profile path on every invocation, defeating the persisted
  default profile and forcing repeated re-authentication. The persisted
  per-kind profile under the OS app-data dir is now the only option,
  matching the documented intent of 0.3.0.

## 0.3.0 — 2026-05-15

### Added

- New session subcommands that attach to a running browser and operate on it
  over CDP (Chromium-family) or BiDi (Firefox) with a single CLI surface:
  - `browser-control targets` — list/filter open page targets
    (`--url REGEX`, `--json`).
  - `browser-control cookies` — export cookies with `--domain`/`--name`
    regex filters; formats `json` (default), `netscape`, and `header`.
    `-o FILE` writes the file `chmod 0600`; `--reveal` opts in to printing
    full values to stdout. The `netscape` output is byte-compatible with
    the Mozilla `cookies.txt` format used by `curl` and `yt-dlp`.
  - `browser-control fetch` — run an HTTP request from inside the page
    context (cookies, CORS, TLS apply). `-X`, `-H`, `-d`, `--target REGEX`,
    `-i`, `-o FILE`.
  - `browser-control storage get|set|list` — read/write `localStorage` or
    `sessionStorage` (`--namespace session`).
  - `browser-control eval` — run a JS expression in the active page,
    optionally with a `--json` envelope and `--target REGEX`.
  - `browser-control wait --ready` — block until the browser's CDP / BiDi
    endpoint is reachable.
  - `browser-control wait-for-cookie` — block until a cookie matching
    `--domain REGEX --name REGEX` exists; optional `--validate-url URL`
    requires a 2xx response before exiting.
- New MCP tools mirroring the above: `list_targets`, `cookies`,
  `storage_get`, `storage_set`, `wait_for_cookie`.
- `browser-control list-running --json` is enriched with `cdp_port`,
  `cdp_ws_url` (CDP), and `bidi_ws_url` (Firefox) for tooling integration.
  Stale rows are re-probed before WS URLs are emitted; the fields are
  omitted when the probe fails.
- New documentation under [docs/session-ops.md](docs/session-ops.md)
  covering the engine-agnostic `PageSession` model, `TargetInfo` shape, the
  exact `cookies --format netscape` byte format, and profile semantics.

### Changed

- `browser-control start` without `--profile` now uses a stable persisted
  profile per browser kind under the OS app-data dir
  (`profiles/<kind>/default/`) instead of a fresh ad-hoc temp directory on
  every launch. Browser state (cookies, logins, extensions) is reused
  across invocations and across MCP agents. `--profile <absolute-path>`
  continues to override per-invocation. Named profiles are deferred.

## 0.2.2 - 2026-05-14

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
