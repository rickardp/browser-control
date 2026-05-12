> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: superseded
date: 2025-01-15
superseded-by: ADR-008
---

# ADR-004: Proxy Architecture for MCP Tool Merging

> **Superseded by [ADR-008: CDP Reverse Proxy](008-cdp-reverse-proxy.md).** The MCP proxy architecture has been replaced with a CDP reverse proxy and two independent MCP entries.

## Context

The coordinator needs to expose its own tools (browser lifecycle management) alongside the child MCP's tools (browser automation like navigate, click, screenshot). The host sees a single MCP server.

## Decision

The coordinator acts as an MCP proxy: it spawns the child MCP as a subprocess, connects to it as an MCP client, and merges both tool sets in its `tools/list` response. Tool calls are routed based on name prefix.

## Consequences

### Positive

- The host sees one unified tool list — no configuration of multiple MCP servers
- Coordinator tools (`coordinator_*`) are clearly namespaced and routed directly
- Child MCP tools are forwarded transparently — the coordinator doesn't need to understand their schemas
- Child MCP is swappable (`--mcp` flag) — not hardcoded to Playwright
- Separation of concerns: coordinator manages lifecycle, child manages automation

### Negative

- Added latency for proxied tool calls (one extra hop through coordinator)
- Child MCP's tool list is fetched once at startup and cached — dynamic tool changes from the child are not reflected
- Error propagation requires mapping child MCP errors back through the coordinator

### Routing Rules

1. Tool name starts with `coordinator_` → handled by coordinator directly
2. Tool name exists in child MCP's cached tool list → ensure browser is running, then forward to child
3. Otherwise → return error (unknown tool)
