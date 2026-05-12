> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: accepted
date: 2026-02-18
supersedes: ADR-004
---

# ADR-008: CDP Reverse Proxy with Two Independent MCP Entries

## Context

The original architecture (ADR-004) had the coordinator acting as an MCP proxy: spawning a child MCP as a subprocess, connecting to it as an MCP client, and merging both tool sets into a single unified tool list. This worked but created tight coupling between the coordinator and child MCP.

Problems with the proxy approach:
- The coordinator had to restart the child MCP whenever the browser's CDP port changed (e.g., browser switch or restart)
- The child MCP was an opaque subprocess — no way for the user to configure it independently
- Tool list merging added complexity and latency
- The coordinator took a dependency on the MCP client SDK, increasing the coupling surface

## Decision

Replace the MCP proxy architecture with a **CDP reverse proxy** and **two independent MCP entries**:

1. **CDP reverse proxy:** The coordinator runs a TCP server on a stable port that forwards connections to the browser's internal CDP port. When the browser changes, only the proxy's backend target updates — the external port stays the same.

2. **State file:** The coordinator writes its proxy port to `$TMPDIR/browser-coordinator/state.json`. The `wrap` subcommand reads this to inject the port into child MCP arguments.

3. **`wrap` subcommand:** A thin wrapper that reads the state file, replaces template variables (`{cdp_port}`, `{cdp_endpoint}`) in command args, and spawns the child with `stdio: 'inherit'`.

4. **Independent MCP entries:** The coordinator and child MCP are configured as separate servers in `.mcp.json`. Each has its own tools, lifecycle, and configuration.

## Consequences

### Positive

- **Stable port:** Child MCPs never need restarting when the browser changes. The proxy port is fixed for the coordinator's lifetime.
- **Independent configuration:** Users can configure the child MCP with any flags, environment variables, or settings without coordinator involvement.
- **Simpler coordinator:** No MCP client dependency, no tool merging, no proxy forwarding. The coordinator only manages browsers and the CDP proxy.
- **Swappable child MCPs:** Any MCP that accepts a CDP endpoint can be used — just change the `wrap` command.
- **Lower latency:** Child MCP tool calls go directly to the host, not through a proxy hop.

### Negative

- **Two MCP entries:** Users must configure two entries in `.mcp.json` instead of one. The `wrap` subcommand mitigates this but adds a conceptual step.
- **State file dependency:** The `wrap` subcommand depends on the state file being written before the child starts. Polling with backoff handles this but adds startup latency (~250ms typical).
- **Connection breaks on browser switch:** When the browser changes, existing CDP connections through the proxy are closed. Child MCPs must reconnect on next tool call (Playwright MCP handles this gracefully).

### Migration from ADR-004

- `--mcp` flag removed
- `mcp-proxy.ts` deleted
- MCP client SDK dependency removed
- Tool routing simplified: only `coordinator_*` tools handled
