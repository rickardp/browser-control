---
status: Active
date: 2026-05-22
---

# ADR-002: CLI lifecycle with bounded browser operations

## Context

Three failure modes pushed us to ask whether `browser-control` needs a
long-lived daemon between the CLI and the browser:

1. **Wedged renderers.** A live tab that doesn't service `Runtime.evaluate`
   (service-worker-paused page, busy event loop, modal dialog, embedded
   admin UIs like iLO) emits no protocol event. The default CLI selector
   could land on such a tab and the call would block until the upstream
   30 s `REQUEST_TIMEOUT`. Agents going down this rabbit hole was a
   recurring real-world bug.
2. **Firefox BiDi single-session limit.** Firefox allows one WebDriver-BiDi
   session per browser at a time. Two concurrent CLI invocations against
   the same Firefox instance race on `session.new` and one gets "Maximum
   number of active sessions".
3. **Stable tab identity across CLI invocations.** Each CLI process is
   short-lived and stateless; agents that want to drive the same tab across
   calls have nothing to address it by except a regex over the URL, which
   re-resolves to a different `targetId` after a navigation cross-process.

We also accumulated two product constraints during the design discussion:

- **Always proceed.** Agent-facing tab/session ops must not surface
  `TabHung` / `TargetCrashed` / `Closed` to the caller — detect, recreate,
  retry once, only then escalate.
- **No idle work.** The tool must not do work while the user is idle.
  Nothing should hold an upstream CDP WebSocket, run sweep timers, or keep
  the browser from entering tab-discard / sleep states between
  invocations.

## Options Considered

### Option A — Long-lived daemon with named tabs and a scratch pool

Run a per-browser daemon process that owns the upstream CDP/BiDi
WebSocket for the browser's lifetime. The daemon would:

- Subscribe to `Target.*` and `Inspector.targetCrashed` and maintain a
  per-target liveness map, so a crashed tab fast-fails in-flight ops.
- Own a pool of `about:blank` scratch tabs and route lock-free ops
  (`eval`, `fetch`, cookie reads) through them, so user tabs (and
  therefore iLO-style wedges) are off the default code path entirely.
- Provide named tabs (`tab open --name X`) with idle GC and LRU recycling
  on a bounded budget.
- Hold the single Firefox BiDi session for its lifetime, arbitrating
  concurrent CLI calls behind it.
- Expose `LockedSession` for stateful work that needs exclusive access.

CLI ↔ daemon wire over UDS (mac/linux) and Windows Named Pipes, framed
by Cap'n Proto RPC.

We took this option seriously and prototyped it end-to-end on the
`daemon-phase-0` branch to measure the actual implementation cost, not
guess at it. The prototype confirmed the design works and the wins are
real for the workloads it targets. It also surfaced costs that are
permanent, not introductory:

- Cap'n Proto schemas plus ~17 000 LOC of generated bindings under
  `src/generated/`. Schema drift becomes a CI concern; `cargo install`
  needs the schema compiler unless bindings are committed (which means a
  contributor regen workflow).
- A custom cross-platform IPC transport (UDS, Windows Named Pipes) plus
  a bringup state machine (atomic state file, `flock`-based spawn-race
  serialization, `pid_alive` probe, stale-socket cleanup).
- A capnp toolchain dependency installed per-CI-matrix-cell via an
  `xtask`, plumbed through `build.rs`.
- A `current_thread` runtime + `LocalSet` shape forced by `capnp-rpc`'s
  `!Send` types — the prototype already deadlocked one integration test
  under nested `tokio::spawn` + `Promise::from_future`.
- A daemon-upgrade problem the tool doesn't have today: `brew upgrade`
  replaces the binary, but a running daemon keeps the old version until
  something restarts it. Either every CLI invocation negotiates a version
  handshake and respawns on mismatch, or the user is told to `daemon
  stop` after upgrades. Neither is free.
- A new `Daemon` and `Tab` CLI surface (developer + agent-facing
  subcommands) with its own permissions, error taxonomy, and docs.

The product constraint that flipped the decision was **No idle work**. A
daemon by definition holds an upstream CDP WebSocket open, runs an
idle-sweep timer, and may keep the browser from entering tab-discard /
sleep states. We could mitigate (`daemon stop --on-idle 5m`, no idle
sweep, lazy upstream open), but each mitigation walks back one of the
daemon's wins. At that point we'd be carrying the costs of a daemon for
diminishing returns.

Re-examining the three motivating failure modes with that constraint:

- **(1) Wedged renderers.** The actual fix is bounded ops with typed
  errors; a daemon adds *recovery* (retry on a fresh tab), but recovery
  can live in the CLI's session layer just as well.
- **(2) Firefox BiDi.** A daemon-owned long-lived session is the most
  elegant arbitration, but a SQLite advisory lock with `pid_alive` on
  acquire gives the same correctness without a long-lived process.
- **(3) Named tabs.** Useful, but a SQLite-backed `name → target_id`
  mapping plus get-or-create-on-invocation gives the same agent contract
  without a daemon owning in-memory state.

Rejected: the wins are real but recoverable via direct-path mechanisms,
and the costs (IPC, capnp toolchain, bringup state machine, upgrade
dance, lifetime-cross-platform-daemon-care) plus the "no idle work"
product rule rule it out.

### Option B — Short-lived CLI lifecycle

No long-lived process. Every CLI invocation opens a fresh upstream
connection, does its work, and exits. Address the three motivating
failure modes individually on the direct path:

- **(1) Wedged renderers**: 5 s connect-side timeouts on
  `CdpClient::connect` / `connect_http` / `BidiClient::connect` so the
  initial handshake can't hang on a dying browser process or a stale
  `/devtools/browser/<GUID>`. `PageSession::evaluate_with_timeout` with a
  typed `SessionError::TabHung { target_id, url, timeout_ms, hint }` so
  every locked op has a bounded ceiling and the caller sees a structured
  error instead of a multi-second stall. CLI defaults: `eval` 10 s
  (override with `--timeout-ms`), `fetch` 60 s, `storage` 10 s,
  `wait-for-cookie --validate-url` 30 s per iteration.
- **(2) Firefox BiDi**: `session.end` on close plus retry-on-collision in
  `session.new` handles the common case today. The remaining race window
  closes with a SQLite advisory lock (`pid_alive` on acquire), held for
  the CLI process's lifetime — filed as a follow-up.
- **(3) Named tabs**: SQLite `tabs` table with sweep-on-read for stale
  rows, addressable as `<browser>/<name>` — filed as a follow-up.

The recover-once wrapper around scratch-tab ops — the operational
implementation of the "always proceed" rule — also moves to the CLI's
session layer as a follow-up.

Accepted.

## Decision

Adopt **Option B**.

Concretely, this ADR captures three things that ship together:

1. **Connection-side timeouts.** 5 s on `CdpClient::connect`,
   `CdpClient::connect_http`, and `BidiClient::connect`. No CLI codepath
   can block indefinitely on a dying browser or stale endpoint.
2. **Bounded ops via `PageSession::evaluate_with_timeout`.** Returns
   typed `SessionError::TabHung` / `TabCrashed` on expiry. Existing
   `PageSession::evaluate(...)` remains as a `timeout = None` delegate
   for back-compat; new callers should bound explicitly.
3. **Bounded defaults in every direct CLI caller.** `eval` (`--timeout-ms`
   default 10 000), `fetch` (60 s), `storage` (10 s),
   `wait-for-cookie --validate-url` (30 s per iteration).

The remaining design work — the recover-once wrapper, named-tab table,
BiDi SQLite lock, and `Browser.getVersion` cache — is deliberate
additive product work and is filed as follow-ups under
`Related → Follow-ups` below, each easier to review in isolation than
bundled with the architectural decision.

## Consequences

**Wins**

- "No idle work" is a structural property of the tool. Between CLI
  invocations there is nothing running — no upstream WebSocket, no
  sweep timers, no daemon process.
- `cargo install browser-control` and `brew install` work without a
  schema compiler, an IPC transport bringup state machine, or a daemon
  upgrade story.
- The CI matrix stays narrow (no per-cell capnp install, no
  schema-drift job, no daemon-smoke step).
- The 5 s connect timeout and `evaluate_with_timeout` defaults
  immediately bound the original rabbit-hole bug (wedged renderer →
  30 s `REQUEST_TIMEOUT`) on every direct-path caller.

**Losses**

- The "always proceed" rule comes at the cost of one extra round-trip
  per recovery: a wedged or crashed renderer surfaces only after the
  per-op timeout fires, then the wrapper recreates and retries. Agents
  see slightly higher tail latency under failure compared to a daemon
  that could detect a dead tab eagerly. For the wedged case this is
  fundamental (no protocol event); for the crashed case it is bounded
  by the crash-event path (see Follow-ups → renderer-crash detection).
- BiDi has no protocol event analogous to `Target.targetCrashed` /
  `Inspector.targetCrashed`. Firefox context crashes surface as
  `no such frame` / `no such context` on the next request, which the
  `TargetGone` classifier already routes through recover-once — but the
  in-flight call still waits for its timeout rather than short-circuiting
  on a crash signal.

**Non-impact**

- The shipped CLI surface (`list-installed`, `list-running`, `start`,
  `targets`, `cookies`, `fetch`, `storage`, `eval`, `wait`,
  `wait-for-cookie`, `set` / `get` / `unset`, `mcp`, `tab`) is
  unchanged in shape; new flags (`eval --timeout-ms`,
  `fetch --timeout-ms`) and the unified `<browser>/<name>` path
  syntax are additive.
- The SQLite registry under the OS app-data dir is unchanged in shape
  (ADR-001 remains in force). The registry's lock granularity narrowed:
  the exclusive file lock is now held only across schema migration
  during `Registry::open_at`, not for the lifetime of the handle.
  Steady-state CLI invocations rely on SQLite's WAL + `busy_timeout`
  for inter-process coordination and no longer serialise on browser
  I/O.

## Related

- ADR-001 — Rust CLI rewrite (registry shape, lifecycle ownership).

### Follow-ups

- **Scratch-tab recover-once wrapper** around the default agent ops
  (`fetch`, `eval` against a daemon-style scratch tab). Implements the
  "always proceed" rule on the direct path.
  **Status:** landed. `src/session/scratch.rs` +
  `src/registry/scratches.rs`. The default `eval <browser>` (no
  `--target`) routes through the scratch tab with one-shot recovery
  on `TabHung` / `TabCrashed` / "no target" protocol errors. **Named
  tabs** got the same recover-once contract via
  `with_named_tab_recovery` in `src/session/tabs.rs`: a daemon-created tab
  that dies between `resolve_tab` and the op is closed before the registry
  row is re-pointed at a fresh tab under the same name, preventing repeated
  recoveries from orphaning unbounded browser tabs. User-adopted tabs are
  not closed by recovery. The fresh tab is navigated to the dead row's
  `last_url` (best-effort rehydration; falls back to `about:blank` only if
  rehydration navigation itself fails), and the op retries once. Otherwise
  typed `SessionError::TabNotFound` /
  `TabHung` / `TabCrashed` is surfaced. **Bare-browser `fetch`**
  (origin-bound, no explicit tab) gets the same one-shot recovery
  around `attach_for_origin` — auth-inheritance is preserved because
  each attempt independently re-resolves the origin-bound target
  rather than falling back to `about:blank`.
- **`tab` CLI subcommand** backed by a SQLite `tabs` table with
  sweep-on-read for stale rows (`name`, `target_id`, `last_url`,
  `last_used_at`, `daemon_created`). Stable tab identity across CLI
  invocations.
  **Status:** landed, engine-agnostic. `src/cli/tab.rs` exposes
  `tab open` and `tab list`; every other command's `<browser>` positional
  accepts the unified `<browser>/<name>` path syntax (via `parse_target`
  in `env_resolver.rs`). Soft cap deferred; hard cap 50 with LRU close on
  budget pressure. The `tabs` table and the named-tab/scratch
  orchestration store engine-opaque ids: CDP `targetId` and BiDi
  `context` use the same column, dispatched by the [`TabBackend`] enum in
  `src/session/backend.rs` (`Target.*` vs `browsingContext.*`). Chromium
  and Firefox share the same SQL, the same CLI surface, and the same
  agent contract.
- **SQLite `bidi_lock` row**, acquired on first BiDi op, released on
  `Drop` of a lock guard at process exit. `pid_alive` check on acquire
  handles the crashed-CLI case. Closes the Firefox BiDi race window.
  **Status:** landed, fully spread. `src/registry/bidi_lock.rs`. Every
  CLI command that opens a BiDi session acquires via
  `acquire_bidi_lock_if_needed` (30 s default wait, typed
  `BidiLockBusy` on timeout): `eval`, `fetch`, `cookies`, `storage`,
  `wait-for-cookie`, `tab open`, `tab list`. `wait` skips the lock
  (HTTP probe only, no `session.new`). The MCP server acquires lazily
  on the first tool call via `ServerState::ensure_bidi_lock` and holds
  for the server's lifetime, so concurrent MCP servers and CLI
  invocations against the same Firefox can't race on `session.new`.
- **SQLite `browser_cache` row** with TTL for `Browser.getVersion`,
  engine, and WS endpoint — eliminates per-invocation probes.
  **Status:** still deferred. Revisit when profiling shows per-invocation
  probe cost matters.

- **Narrowed registry lock.** **Status:** landed. `Registry::open_at`
  no longer holds a process-wide exclusive file lock for the lifetime
  of the handle. The lock is taken across schema migration only;
  steady-state CLI invocations rely on SQLite's WAL + `busy_timeout`
  (5 s) for inter-process coordination. Unrelated invocations against
  the same registry — and the same browser — no longer serialise on
  each other's browser I/O.

- **CDP renderer-crash detection.** **Status:** landed.
  `src/session/crash.rs` runs every CDP `Runtime.evaluate` in a
  `tokio::select!` against a subscription filter for
  `Target.targetCrashed` (matched on `targetId`) and
  `Inspector.targetCrashed` (matched on `sessionId`). A matching event
  short-circuits the in-flight call with typed `SessionError::TabCrashed`
  instead of waiting for the per-op timeout. `Inspector.enable` is
  emitted on each per-page attach (`PageSession::attach`,
  `attach_for_origin`, and `TabBackend::evaluate`'s transient session)
  so the per-session event is actually delivered. BiDi has no
  equivalent event; context crashes there still surface as
  `TargetGone` on the next request — see Losses.

- **Typed `SessionError::TabNotFound`**. **Status:** landed. Added next
  to `TabHung` / `TabCrashed` so the CLI, MCP tools, and tests can
  pattern-match instead of parsing strings. Carries `browser` + `name`
  for the diagnostic message.

- **`fetch --timeout-ms`** to bound the in-page JS fetch. **Status:**
  landed. Default 60 000 ms (same as the previous hard-coded value).
  `eval`, `fetch`, and `storage` ops now all honour a `timeout_ms`;
  `wait-for-cookie --validate-url` keeps its inner 30 s bound.

- **Engine-agnostic `<browser>/<tab>` syntax** across every command
  that takes a `<browser>` positional. **Status:** wired into `eval`,
  `fetch`, `tab open`, `tab list`. `--name` flag on `tab open` is gone
  — the positional `<browser>/<name>` is the only form. `storage`
  keeps its `--target <regex>` selector by design (path syntax would
  be redundant); `cookies` / `wait-for-cookie` are browser-wide and
  don't take a tab.

- **`tab list --all`** merges the named-tab registry with every live
  top-level tab in the browser (unregistered tabs surface with empty
  `name`, `owner = "unnamed"`). Replaces the design intent of a
  separate `tab adopt` verb: agents discover the id of a
  user-opened tab via `tab list --all`, then drive it by id (future
  follow-up: bind id → name without creating a new tab).

- **Structured routing trace per command**. **Status:** landed.
  `src/cli/trace.rs::CommandTrace` emits one `tracing::info!` line per
  CLI dispatch on `target=browser_control::cli` with fixed fields:
  `command, browser, engine, route, tab_name, target_id, elapsed_ms,
  outcome, err`. Agents and operators tail stderr with
  `BROWSER_CONTROL_LOG=info` (the default). Wired into `eval`,
  `fetch`, `tab open`, `tab list`, `cookies`, `storage` (sub-command
  granularity), `wait`, `wait-for-cookie`.

- **MCP server BiDi lock**. **Status:** landed. The MCP server
  acquires the SQLite BiDi lock lazily on first tool call via
  `ServerState::ensure_bidi_lock`, holds for its lifetime, releases on
  process exit. Closes the cross-process Firefox race for MCP-driven
  workloads. `browser_select` intentionally commits the new active
  browser before attempting eager lock preparation; if the Firefox lock
  is busy, the tool fails with that state left in place. Callers decide
  whether to retry the lock, switch elsewhere, or switch back.

- **MCP scratch routing + named-tab integration for tool handlers**.
  **Status:** landed. Every stateful MCP tool (`navigate`, `get_dom`,
  `screenshot`, `select_element`, `fetch`, `storage_get`,
  `storage_set`) routes through `ServerState::ensure_active_tab`,
  which returns `(TabBackend, target_id)` for a server-owned row in
  the `tabs` table named `_mcp-<server-pid>`. Tools no longer call
  `PageSession::attach(..., None)` (which picked the first page in
  `Target.getTargets` order) — the iLO failure mode is closed for the
  MCP code path too. A new `TabBackend::screenshot` engine-agnostic
  method backs the screenshot tool; the BiDi path uses
  `browsingContext.captureScreenshot`. The MCP-owned tab is visible
  in `tab list --all` for diagnostics, recycled on staleness with
  `about:blank` (agents that need a specific URL navigate to it). The
  CDP `TabBackend` is cached for the server's lifetime via
  `ServerState::ensure_backend` so the BiDi `session.new` handshake
  runs once.

- **Engine override `--engine <cdp|bidi>`** and **synthesized
  named-tab keys for external `ws://` endpoints**: open design
  questions noted during the trade-off review; intentionally not in
  scope for this PR.
