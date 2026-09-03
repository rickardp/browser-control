# Changelog

## 1.2.0 — 2026-09-03

### Added

- Console and network capture (ADR-003). The MCP server keeps one CDP
  session per touched tab with `Runtime`, `Log`, `Network`, and `Page`
  enabled and buffers browser-pushed events (last 1000 console entries and
  500 requests per tab, across navigations). New tools
  `browser_console_messages` (`pattern`, `only_errors`, `limit`, `clear`,
  `format`), `browser_network_requests` (`url_pattern`, `method`, `status`,
  `resource_type`, `limit`, `clear`, `format`), and `browser_network_body`
  (on-demand `Network.getResponseBody`, capped at 256 KiB by default, 8 MiB
  max). Chromium-only; `BROWSER_CONTROL_CAPTURE=0` disables attachment.
- `browser_snapshot` is now native CDP (no Playwright sidecar) and returns
  stable element refs (`[ref=eN]`), with `interactive_only`, `ref`, `depth`,
  and `max_chars` options and a page header line.
- `browser_find` returns refs for a short description of an element
  ("search box", "Sign in button") by matching accessible name, value,
  description, and role.
- `browser_click`, `browser_type`, `browser_hover`, and `browser_drag` accept
  a `ref` (native CDP input via `DOM.getContentQuads`,
  `Input.dispatchMouseEvent`, `Input.insertText`) as an alternative to the
  CSS selector. `browser_type` gains `submit` (press Enter afterwards) on
  both paths. Refs are bound to a document token; after a navigation they
  fail with a typed `StaleRef` instead of acting on a recycled node id.
- `browser_get_page_text`: readable, article-first page text through a
  single evaluate; works on every engine including Firefox.
- `browser_take_screenshot` gains `format` (`png` | `jpeg`), `quality`,
  `max_width` (downscale via `clip.scale`), `save_to` (write a 0600 file and
  return only path and dimensions), and `ref` clipping. The default output
  is unchanged.

- Firefox (WebDriver BiDi) parity for the native tools: `browser_snapshot`,
  `browser_find`, and ref-based `browser_click` / `browser_type` /
  `browser_hover` / `browser_drag` / `browser_take_screenshot` work on Firefox
  through an injected accessibility walker, a page-side ref registry, and
  `input.performActions`; `browser_take_screenshot { full_page }` works on
  Firefox via a document-origin clip. Console and network listing work on
  Firefox through one BiDi `session.subscribe` (network needs Firefox 124+).
  Still Chromium-only: `browser_network_body` and screenshot `max_width`.
  Every remaining Chromium/Firefox difference is listed in
  `docs/engine-parity.md`.

- Foreground emulation (ADR-004): `browser_tab_foreground` (MCP) and
  `browser-control tab foreground <browser>/<tab> on|off` (CLI) make a
  Chromium tab behave as the focused, visible foreground tab while the window
  is minimized or the display is locked, so `requestAnimationFrame` and timers
  run at full rate, `document.visibilityState` / `document.hasFocus()` report
  foreground, and screenshots show live content. Both surfaces spawn the same
  detached holder process (`tab foreground-hold`, hidden) whose PID and expiry
  live in the registry, so `browser_tab_list` / `tab list` show the flag and
  either surface can turn it off. Default timeout 1 hour (`timeout` /
  `--timeout`); `enabled: false, all: true` or `tab foreground <browser> off`
  stops every holder on a browser.

### Changed

- The transport's event broadcast capacity is 2048 (was 256) so a page load's
  burst of `Network.*` events does not lag the capture hub.
- `browser-control --agent-instructions` documents the snapshot/ref workflow,
  screenshot hygiene, and console/network capture.
- BiDi script results are flattened to plain JSON (objects arrive as
  `[[key, value]]` pairs), so `browser_eval` and the page-freshness probe work
  on Firefox; script exceptions surface as errors.

### Fixed

- Firefox does not end a WebDriver BiDi session when its WebSocket closes,
  so the MCP server, `browser_select`, and the `tab` / `eval` / `curl` /
  named-tab CLI paths left the browser refusing every later `session.new`
  ("Maximum number of active sessions"). They now send `session.end` before
  exiting.

## 1.1.1

### Fixed

- Prefer an already-running browser over the hardcoded detection order.

## 1.1.0 — 2026-07-14

### Added

- `browser-control curl` and MCP `browser_curl` invoke the real system curl
  with a temporary cookie jar and User-Agent copied from the selected browser,
  plus Origin and Referer derived from the selected source tab.
  Curl arguments are forwarded unchanged. CLI output streams normally; MCP
  returns text or binary responses up to 8 MiB and directs larger downloads to
  use curl `-o`/`--output` for unrestricted file streaming.

## 0.3.5 — 2026-05-15

### Fixed

- `browser-control fetch URL` now runs in the context of a tab on the
  target URL's origin instead of whichever tab happens to be active. If
  no existing tab matches the origin, a new tab is opened and navigated
  to the origin root before the fetch is issued. This ensures cookies
  and CORS behaviour match what the URL expects, regardless of what the
  user is doing in the browser. The new tab is left open so subsequent
  fetches against the same origin reuse it. Passing `--target REGEX`
  still works as an explicit override and skips the origin auto-attach.

## 0.3.4 — 2026-05-15

### Changed

- Browser resolution no longer falls back to "most recently alive in the
  registry" when no argument, `BROWSER_CONTROL` env, or persisted default
  is supplied. That fallback depended on global state another process
  could mutate, producing surprising behaviour on shared hosts. Callers
  must now be explicit — pass an argument, set `BROWSER_CONTROL`, or run
  `browser-control set default <value>`.

## 0.3.3 — 2026-05-15

### Changed

- `browser-control start` now waits for the browser's debugging endpoint
  to be reachable before returning (up to `--wait-timeout` seconds,
  default 30). This eliminates the most common command chain of
  `start && wait --ready && ...` and prevents races where agents tried
  to attach before the endpoint was serving requests. Pass `--no-wait`
  to opt out and return as soon as the process is spawned.

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
