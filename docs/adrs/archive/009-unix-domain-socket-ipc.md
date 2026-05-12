> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: Active
date: 2026-02-18
---

# ADR-009: Unix Domain Sockets for Extension IPC

## Context

The coordinator and companion VS Code extension need to communicate for Tier 2 integration (ADR-006). The current approach uses multiple fragile discovery mechanisms: `VSCODE_CDP_PORT` env var, a temp file at `$TMPDIR/vscode-cdp-port`, and process argument scanning. These are unreliable — env vars only propagate to child processes, temp files can be cleaned by the OS, and process scanning is platform-specific and brittle.

We need a proper IPC channel that:
- Works bidirectionally (extension can push state changes, not just respond to polls)
- Is reliable across platforms (macOS, Linux, Windows)
- Supports multiple VS Code windows without conflicts
- Cleans up automatically when either side exits

The [tjx666/vscode-mcp](https://github.com/tjx666/vscode-mcp) project demonstrates this pattern successfully: a VS Code extension listens on a Unix domain socket, and an external MCP server connects to it using a deterministic socket path derived from the workspace.

## Options Considered

### Option 1: Unix domain sockets (named pipes on Windows)
- Pro: Bidirectional communication — extension can push CDP port changes, status updates
- Pro: Deterministic path from workspace — no discovery protocol needed
- Pro: File permissions provide security (no network exposure)
- Pro: OS cleans up on process exit (mostly)
- Pro: Proven pattern in vscode-mcp project
- Con: Socket files can become stale if extension crashes without cleanup
- Con: Slightly more complex than writing a file

### Option 2: Keep current env var + temp file approach
- Pro: Simple to implement
- Pro: Already working for basic cases
- Con: Unidirectional — coordinator must poll for changes
- Con: Env vars don't propagate to independently launched processes
- Con: Temp files can be cleaned by OS or linger as stale references
- Con: Process arg scanning is fragile and platform-specific

### Option 3: HTTP server on localhost
- Pro: Well-understood protocol
- Pro: Easy to debug with curl
- Con: Requires port allocation (conflicts, firewall issues)
- Con: Exposed on network interface (security concern)
- Con: Heavier than needed for local IPC

## Decision

We chose **Unix domain sockets** (named pipes on Windows) because they provide reliable, bidirectional, secure local IPC without network exposure or port allocation. The deterministic socket path eliminates the need for a discovery protocol.

### Socket path convention

Derive the path from the workspace using a hash, following vscode-mcp's pattern:

- **macOS/Linux:** `$XDG_DATA_HOME/browser-coordinator-mcp/ipc-{hash}.sock` (fallback `~/.local/share/...`)
- **Windows:** `\\.\pipe\browser-coordinator-mcp-{hash}`

Where `{hash}` is a short hash of the workspace path, ensuring one socket per VS Code window.

### Protocol

Simple JSON request/response over the socket, one connection per request. The extension acts as the socket server (it starts first), and the coordinator connects as a client.

### Lifecycle

1. **Extension activates** — creates socket server at the deterministic path
2. **Coordinator starts** — connects to socket, requests CDP port and state
3. **Extension pushes updates** — CDP port changes, browser state (when persistent connections are needed)
4. **Either side exits** — socket file cleaned up on deactivation; stale sockets detected via health check probes

## Consequences

**Positive:**
- Eliminates fragile port discovery (env vars, temp files, process scanning)
- Bidirectional communication enables push-based state updates
- No network exposure — socket file permissions provide security
- Multi-window support via workspace-derived socket paths
- Proven pattern from vscode-mcp project reduces design risk

**Negative:**
- Need zombie socket cleanup for crashed extensions (health check probe + delete)
- Adds a shared IPC contract that both sides must agree on
- Slightly more setup code than writing a temp file

## Related

- ADR-006: [Two-Tier VS Code Integration](006-two-tier-vscode-integration.md)
- Feature spec: [VS Code Integration](../specs/features/vscode-integration.md)
- Feature spec: [CDP Proxy](../specs/features/cdp-proxy.md)
- Inspiration: [tjx666/vscode-mcp](https://github.com/tjx666/vscode-mcp) — socket server pattern, deterministic path convention, zombie cleanup
