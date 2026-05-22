# ADR 002: Daemonless — keep the direct-CLI path, drop the daemon

## Status

Accepted (2026-05-22). Supersedes the unreleased Phase 0 / Phase 1 daemon
work on the `daemon-phase-0` branch.

## Context

Phase 0 / Phase 1 (commits `510eac5`, `10404de`, `70e1427`) introduced a
long-lived daemon process to:

1. Arbitrate the Firefox BiDi single-session limit ("Maximum number of
   active sessions") across concurrent callers.
2. Hide stuck / wedged renderers behind a daemon-owned scratch-tab pool,
   so agents never see `TabHung`.
3. Provide a named-tab abstraction (`tab open --name X`) with idle GC.
4. Offer an exclusive lock capability (`LockedSession`) for stateful
   work.

The implementation cost was substantial:

- Cap'n Proto schemas (`schema/*.capnp`) and ~17 000 LOC of generated
  bindings under `src/generated/`.
- A custom cross-platform IPC transport (Unix domain sockets + Windows
  named pipes) under `src/daemon/transport/`.
- Daemon bringup with a state file, `flock`-based spawn-race
  serialization, and `pid_alive` probing.
- A capnp toolchain dependency installed by a workspace `xtask`,
  plumbed through `build.rs` and every CI matrix cell.
- A `Daemon` and `Tab` CLI subcommand surface aimed at developers.
- A `current_thread` runtime + `LocalSet` shape for `capnp-rpc`, which
  is already biting an integration test (`#[ignore]`'d due to a
  `Promise::from_future + nested tokio::spawn` deadlock).
- An "upgrade" problem the tool didn't have before: `brew upgrade`
  replaces the binary, but a running daemon keeps the old version
  until something restarts it.

Reviewing the four motivations against the actual workloads
`browser-control` sees (one human or one agent per terminal, rarely two
clients hammering a single browser in parallel) makes most of the
daemon's complexity speculative:

- **(1) Firefox BiDi**: real, but solvable without a daemon. Phase 1's
  retry-on-collision in `session.new` plus `session.end` on close
  (commit `1826fd3`) already handles the common case. The Firefox limit
  is "one BiDi session at a time" — a long-lived daemon-owned session
  amortizes setup latency, but does not enable parallelism that doesn't
  otherwise exist.
- **(2) Stuck renderers**: the actual fix is bounded ops with typed
  errors. Phase 1's connect-side timeouts on `CdpClient` / `BidiClient`
  and `PageSession::evaluate_with_timeout` already cover this on the
  direct path. The daemon adds *recovery* (retry on a fresh tab), but
  recovery can live in the CLI's session layer just as well.
- **(3) Named tabs**: useful, but a SQLite-backed name → target_id
  mapping plus get-or-create-on-invocation gives the same agent contract
  without a daemon owning in-memory state.
- **(4) Exclusive lock**: most realistic workloads are already serial
  per process. Where coordination is needed, the SQLite registry is
  multiprocess-friendly and can hold a process-lifetime advisory lock.

Two product constraints sharpened the decision:

- **Always proceed** (durable feedback rule): agent-facing tab/session
  ops must not surface `TabHung` / `TargetCrashed` / `Closed` to the
  caller. Detect, recreate, retry once, only then escalate. This is a
  design requirement *independent of* whether a daemon exists — the
  recovery loop can sit in the CLI as well as in a daemon.
- **No idle work** (durable feedback rule): the tool must not do work
  while the user is idle. A daemon by definition holds an upstream CDP
  WebSocket open, runs an idle-sweep timer, and may keep the browser
  from entering tab-discard / sleep states. This makes a daemon a
  *negative* for product behavior, not just an implementation choice.

## Options Considered

### Option A — Ship the daemon as designed

Keep Phase 0 / Phase 1. Accept the capnp toolchain, IPC transport,
bringup state machine, and forever-cross-platform-daemon-lifecycle
costs. Take the "no idle work" guarantee off the table.

Rejected: the speculative wins do not pay for the certain costs, and
the "no idle work" rule rules out the daemon-owned upstream socket and
idle sweep on product grounds.

### Option B — Daemonless direct-CLI path

Keep the Phase 1 robustness primitives that benefit the direct path
(`evaluate_with_timeout`, connect-side timeouts, `SessionError::TabHung`
/ `TargetCrashed`). Remove the daemon module, the capnp schemas and
generated bindings, the IPC transport, the daemon and tab CLI
subcommands, and the capnp toolchain install. Defer scratch-tab
recovery and named tabs to follow-up PRs that live in the CLI's session
layer, backed by the existing SQLite registry.

Accepted.

### Option C — Daemonless, plus the scratch-tab recovery wrapper in this PR

Same as B, but also add the recover-once wrapper around scratch-tab
ops in this PR so the "always proceed" rule is enforceable today.

Rejected for *this* PR on scope grounds (the user asked for the
smallest PR that drops the daemon). The recover-once wrapper is a
real feature, not a side effect of deletion, and it deserves its own
review. Filed as follow-up below.

## Decision

Adopt **Option B**. Concretely:

1. **Delete** `src/daemon/`, `src/cli/daemon.rs`, `src/cli/tab.rs`,
   `schema/*.capnp`, `src/generated/`, the `xtask` workspace member,
   and the daemon-related CI steps (capnp install, daemon-smoke,
   schema-drift job).

2. **Keep** the Phase 1 robustness work that lives outside the daemon
   module and benefits the direct path:

   - 5 s connect timeouts on `CdpClient::connect`, `connect_http`,
     `BidiClient::connect`.
   - `PageSession::evaluate_with_timeout` with typed
     `SessionError::TabHung`.
   - The bounded CLI eval default (`--timeout-ms 10000`).

3. **Drop** primitives that exist only to feed the daemon:

   - `CdpClient::closed_signal()` (only the daemon watches for
     upstream-WS close).
   - `src/cdp/target_registry.rs` (only the daemon attached one).

4. **Defer** the following to follow-up PRs, captured under
   `## Related → Follow-ups` below:

   - Scratch-tab recover-once wrapper (the operational implementation
     of the "always proceed" rule).
   - SQLite-backed named-tab table + the agent-facing `tab` verbs.
   - SQLite advisory lock for Firefox BiDi single-session arbitration,
     held for the CLI process's lifetime (no leases, no renewal).
   - SQLite "on start" cache for `Browser.getVersion` / engine /
     endpoint, with TTL.

   These are deliberate additive work, not cleanups, and are easier to
   review in isolation than bundled with the daemon revert.

## Consequences

**Wins**

- ~2 000 LOC of hand-written daemon code + ~17 000 LOC of generated
  capnp bindings removed. No capnp toolchain, no IPC transport, no
  bringup state machine, no daemon-upgrade dance.
- "No idle work" becomes a structural property of the tool. Every CLI
  invocation opens a fresh upstream connection, does its work, and
  exits. Nothing is running while the user is idle.
- The CI matrix simplifies (no per-cell capnp install, no
  schema-drift job, no daemon-smoke step).
- `cargo install browser-control` works on any platform without
  pre-installing a schema compiler.

**Losses**

- The Phase 1 robustness work that lives *inside* the daemon module is
  reverted along with the daemon itself: `TabRegistry` (named-tab
  state machine), the scratch-tab pool, `probe_target`, and the
  daemon's RPC-layer `eval`. These are explicitly re-introduced in
  the follow-up PRs above, where they belong to the CLI's session
  layer instead of an RPC server.
- The Firefox BiDi "Maximum number of active sessions" failure mode
  is mitigated, but not yet eliminated by an explicit lock — the
  Phase 1 retry-on-collision in `session.new` plus `session.end` on
  close (commit `1826fd3`) is the current defence. The SQLite lock
  follow-up closes the remaining race window.
- Until the scratch-tab recover-once wrapper lands, the CLI can still
  surface `TabHung` to the caller. This is a regression against the
  "always proceed" rule and is the highest-priority follow-up.

**Non-impact**

- The user-facing CLI surface for shipped subcommands
  (`list-installed`, `list-running`, `start`, `targets`, `cookies`,
  `fetch`, `storage`, `eval`, `wait`, `wait-for-cookie`, `set` /
  `get` / `unset`, `mcp`) is unchanged.
- The SQLite registry under the OS app-data dir is unchanged. ADR-001
  remains in force.

## Related

- ADR-001 — Rust CLI rewrite (registry shape, lifecycle ownership).
- Commit `1826fd3` — BiDi: end sessions on close to avoid Firefox
  "Maximum number of active sessions" (the existing mitigation).
- Commits `510eac5`, `10404de`, `70e1427` — the Phase 0 / Phase 1
  daemon work being reverted by this ADR.

### Follow-ups

- Scratch-tab recover-once wrapper around the default agent ops
  (`fetch`, `eval` against a daemon-style scratch tab). Implements
  the "always proceed" rule on the direct path.
- `tab` CLI subcommand backed by a SQLite `tabs` table with
  sweep-on-read for stale daemon-created rows (`name`, `target_id`,
  `last_url`, `last_used_at`, `daemon_created`).
- SQLite `bidi_lock` row, acquired on first BiDi op, released on
  `Drop` of a lock guard at process exit. `pid_alive` check on
  acquire handles the crashed-CLI case.
- SQLite `browser_cache` row with TTL for `Browser.getVersion`,
  engine, and WS endpoint — eliminates per-invocation probes.
