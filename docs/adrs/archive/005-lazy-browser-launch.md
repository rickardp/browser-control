> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: accepted
date: 2025-01-15
---

# ADR-005: Lazy Browser Launch

## Context

MCP servers start when the host application launches. If the coordinator eagerly launches a browser at startup, it adds 2-3 seconds of delay and consumes resources even if the user never uses browser tools.

## Decision

The browser launches lazily on the first child MCP tool call, not at coordinator startup. At startup, only a free CDP port is pre-allocated and the child MCP is spawned (without a browser).

## Startup Sequence

1. **Coordinator starts** — no browser process
2. **Pre-allocate port** — bind to port 0, record assigned port, close immediately
3. **Spawn child MCP** — pass `--cdp-endpoint=http://localhost:<port>` to child
4. **Child registers tools** — all tools available immediately via `tools/list`
5. **First child tool call** — coordinator launches browser on the pre-allocated port, waits for CDP ready, then forwards the call

## Consequences

### Positive

- Zero startup delay — MCP server is ready instantly
- No wasted resources if browser tools are never used in a session
- Port is deterministic from the child MCP's perspective (pre-allocated before child starts)
- The child MCP (Playwright) also connects to CDP lazily, so this works seamlessly

### Negative

- First browser tool call has ~2-3 second latency (browser startup)
- Small race window between port allocation and browser binding (negligible in practice — same pattern used by Jest, Vitest, Playwright)

### Neutral

- Coordinator-only tools (`coordinator_list_browsers`, `coordinator_status`) work immediately without launching a browser
