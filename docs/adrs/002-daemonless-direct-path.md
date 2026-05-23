---
status: Active
date: 2026-05-22
---

# ADR-002: Stay daemonless — bound the direct CLI path instead

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

### Option B — Daemonless direct-CLI path

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

- Until the scratch-tab recover-once wrapper lands, the CLI can still
  surface `TabHung` to the caller. This is a known regression against
  the "always proceed" rule and is the highest-priority follow-up.
- The Firefox BiDi "Maximum number of active sessions" failure mode is
  mitigated (`session.end` on close + retry-on-collision) but not yet
  eliminated by an explicit lock. The SQLite lock follow-up closes the
  remaining race window.
- Agents cannot yet address a tab by stable name across CLI
  invocations. Until the named-tab table lands, the only addressing
  mechanisms are `--target <url-regex>` and the most-recently-used
  tab.

**Non-impact**

- The shipped CLI surface (`list-installed`, `list-running`, `start`,
  `targets`, `cookies`, `fetch`, `storage`, `eval`, `wait`,
  `wait-for-cookie`, `set` / `get` / `unset`, `mcp`) is unchanged
  except for the new `eval --timeout-ms` flag.
- The SQLite registry under the OS app-data dir is unchanged.
  ADR-001 remains in force.

## Related

- ADR-001 — Rust CLI rewrite (registry shape, lifecycle ownership).

### Follow-ups

- **Scratch-tab recover-once wrapper** around the default agent ops
  (`fetch`, `eval` against a daemon-style scratch tab). Implements the
  "always proceed" rule on the direct path.
  **Status:** landed. `src/session/scratch.rs` +
  `src/registry/scratches.rs`. The default `eval <browser>` (no
  `--target`) routes through the scratch tab with one-shot recovery
  on `TabHung` / `TabCrashed` / "no target" protocol errors.
- **`tab` CLI subcommand** backed by a SQLite `tabs` table with
  sweep-on-read for stale rows (`name`, `target_id`, `last_url`,
  `last_used_at`, `daemon_created`). Stable tab identity across CLI
  invocations.
  **Status:** landed. `src/cli/tab.rs` exposes `tab open` and
  `tab list`; every other command's `<browser>` positional accepts the
  unified `<browser>/<name>` path syntax (via `parse_target` in
  `env_resolver.rs`). Soft cap deferred; hard cap 50 with LRU close on
  budget pressure.
- **SQLite `bidi_lock` row**, acquired on first BiDi op, released on
  `Drop` of a lock guard at process exit. `pid_alive` check on acquire
  handles the crashed-CLI case. Closes the Firefox BiDi race window.
  **Status:** landed. `src/registry/bidi_lock.rs`. CLI commands that
  resolve a registered BiDi browser acquire via
  `acquire_bidi_lock_if_needed` (30 s default wait, typed
  `BidiLockBusy` on timeout). Wired into `eval` and `fetch`; other
  BiDi-capable commands inherit the existing
  `session.end`-on-close + retry-on-collision mitigation and gain the
  lock in a follow-up.
- **SQLite `browser_cache` row** with TTL for `Browser.getVersion`,
  engine, and WS endpoint — eliminates per-invocation probes.
  **Status:** still deferred. Revisit when profiling shows per-invocation
  probe cost matters.
