> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: accepted
date: 2025-01-15
---

# ADR-002: MCP SDK as Protocol Framework

## Context

The coordinator needs to expose tools to AI hosts (Claude Code, Claude Desktop) via a standard protocol. It also needs to act as a client to child MCP servers (like Playwright MCP).

## Decision

Use `@modelcontextprotocol/sdk` (v1.12.0+) as both the server framework (exposing coordinator tools) and the client framework (connecting to child MCPs).

## Consequences

### Positive

- Standard protocol understood by Claude Code, Claude Desktop, and other MCP hosts
- SDK provides both `Server` and `Client` classes — the coordinator uses both simultaneously
- Stdio transport is simple and reliable for subprocess communication
- Tool schema validation is handled by the SDK
- Protocol versioning and capability negotiation are built in

### Negative

- Tied to MCP protocol evolution — breaking SDK changes require updates
- `tools/list_changed` notification is not reliably supported by all hosts (Claude Code ignores it), so all tools must be available from first `tools/list` response

### Neutral

- The SDK's stdio transport model (JSON-RPC over stdin/stdout) aligns well with subprocess-based child MCP spawning
