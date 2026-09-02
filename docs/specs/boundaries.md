# Agent Boundaries

Rules and constraints for AI agents working in this codebase. This describes
the current Rust CLI. The original TypeScript implementation is preserved on
the `legacy-ts` branch (tag `v0-final-ts`); its boundaries are archived in
`docs/adrs/archive/`. The authoritative design records are ADR-001 (Rust CLI
rewrite), ADR-002 (CLI lifecycle), and ADR-003 (native CDP observation and
ref-based interaction).

## Code Style

- Rust 2021, builds with stable `cargo` (MSRV 1.80).
- Source lives under `src/`, organised by subsystem (`cli`, `mcp`, `session`,
  `registry`, `cdp`, `bidi`, `a11y`, `detect`, `launch`, `sidecar`, …).
- Run `cargo fmt` and keep `cargo clippy --all-targets -- -D warnings` clean.
- Errors use `anyhow::Result` at the edges; cross-layer failures the CLI/MCP/
  tests must match on are typed in `src/errors.rs` (`SessionError`,
  `BidiLockBusy`, …) — pattern-match these, don't parse strings.

## Architecture Constraints

- **Daemonless (ADR-002).** There is no long-lived process. Every CLI
  invocation opens a fresh upstream CDP/BiDi connection, does its work, and
  exits. The MCP server is the only long-lived process and only while a host
  has it open.
- **No idle work.** Between invocations nothing runs — no held upstream
  WebSocket, no sweep timers, no background process that could keep the
  browser from tab-discard / sleep. Do not add heartbeats, polling loops, or
  timers that fire while the user is idle. Push-only listeners are permitted
  (ADR-003): the MCP capture hub (`src/session/capture.rs`) keeps CDP
  `Runtime`/`Log`/`Network`/`Page` enabled on tabs the server has touched and
  buffers browser-pushed events in memory; it never polls, never wakes on a
  timer, and is torn down with the backend.
- **Always proceed.** Agent-facing tab/session ops must not surface
  `TabHung` / `TabCrashed` / `Closed` to the caller. Detect, recreate, retry
  once, and only then escalate with a typed error. The recover-once wrappers
  live in `src/session/{scratch,tabs}.rs`.
- **Bounded ops.** Every upstream call has a ceiling: 5 s connect-side
  timeouts and per-op `evaluate_with_timeout` defaults (`eval` 10 s, `fetch`
  60 s, `storage` 10 s). No codepath may block indefinitely on a wedged
  renderer or dead endpoint.
- **SQLite registry is the only shared state (ADR-001).** Browsers, named
  tabs, scratch rows, and the BiDi lock live in a SQLite DB under the OS
  app-data dir. Inter-process coordination is SQLite's WAL + `busy_timeout`,
  not an in-memory owner. Keep registry SQL access inside `src/registry/`.
- **Engine-agnostic by dispatch.** CDP (Chromium) and BiDi (Firefox) share
  one CLI surface, one `tabs` schema, and one agent contract. Engine-specific
  protocol lives behind `TabBackend` (`src/session/backend.rs`); the stored
  `target_id` column is engine-opaque (CDP `targetId` or BiDi `context`).
- **Firefox single-session lock.** Firefox allows one WebDriver-BiDi session
  per browser. Any command that opens a BiDi session must acquire the SQLite
  `bidi_lock` row first (`acquire_bidi_lock_if_needed`); the MCP server holds
  it for its lifetime. HTTP-only probes (`wait`) skip the lock.

## Browser Support

- **Supported (CDP):** Chrome, Edge, Chromium, Brave.
- **Supported (BiDi):** Firefox.
- **Not supported:** Safari/WebKit.

## MCP Integration

- `browser-control mcp` is a stdio MCP server that targets a browser resolved
  via `--browser` / `$BROWSER_CONTROL` / persisted default.
- Engine-agnostic tools (`browser_navigate`, `browser_get_html`,
  `browser_get_page_text`, `browser_eval`, `browser_fetch`,
  `browser_take_screenshot`, `browser_storage_*`, `browser_cookies`, …) work
  on every supported browser including Firefox.
- Native-CDP tools (`browser_snapshot`, `browser_find`, the `ref` path of
  `browser_click` / `browser_type` / `browser_hover` / `browser_drag` /
  `browser_take_screenshot`, `browser_console_messages`,
  `browser_network_requests`, `browser_network_body`) are Chromium-only for
  now and return `EngineUnsupported` on Firefox with a hint naming the
  engine-agnostic alternative. Their BiDi arms live behind the `TabBackend`
  seam (ADR-003). Element refs are `backendDOMNodeId`s bound to a document
  token; a navigation makes them `StaleRef`, never a click on a recycled id.
- CSS-selector interaction, `browser_press_key`, `browser_wait_for`, and
  `browser_pdf_save` route through a lazily-spawned Node sidecar wrapping
  `playwright-core`; on Firefox they return `EngineUnsupported`.
- Stateful tools route through `ServerState::ensure_active_tab` (a
  server-owned named tab), never a blind "first page" attach.

## What Not to Do

- Do not reintroduce a daemon or any process/timer that runs while idle.
- Do not surface raw `TabHung` / `TabCrashed` / `Closed` to agents — recover
  once first (see `src/session`).
- Do not add an unbounded upstream call — every op needs a connect timeout
  and a per-op ceiling.
- Do not scatter SQLite access outside `src/registry/`, and do not change a
  registry `pub` signature without checking its CLI/MCP/session/test callers.
- Do not let a BiDi command open a session without acquiring the BiDi lock.
- Do not duplicate the `<browser>/<tab>` vs `--target` routing — the CLI
  resolver lives in `src/cli/route.rs`; reuse it.
- The CLI surface and the `BROWSER_CONTROL` env var are the intended stable
  contracts; treat changes to them as breaking.
