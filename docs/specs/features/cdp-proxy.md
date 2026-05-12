> **Note:** This document describes the legacy TypeScript implementation. The
> current Rust CLI is described in the project README and ADR-001. See
> `docs/adrs/archive/` for the original ADRs.

# Feature: CDP Reverse Proxy

## Overview

The coordinator runs a TCP reverse proxy that provides a stable CDP port for child MCPs. When the browser changes (type switch, restart), the proxy port stays the same — child MCPs don't need restarting.

## CDP Proxy (`cdp-proxy.ts`)

### Design

The proxy is a TCP server that forwards connections bidirectionally between clients (child MCPs) and the browser's CDP endpoint.

**Key property:** The proxy port is stable. The browser's internal port can change on every launch/restart/switch, but the proxy port written to the state file stays the same for the lifetime of the coordinator process.

### Launch Triggers

The browser can be started two ways:

1. **Lazy launch (CDP connection):** When a TCP connection arrives at the proxy port and no browser is running, the proxy automatically launches a browser via the lazy launch callback. Zero overhead until someone actually needs the browser.

2. **Explicit launch (coordinator tool):** The agent calls `coordinator_launch_browser` to start or switch browsers. The browser starts immediately.

### Connection Handling

On incoming connection:
1. If no backend port set → invoke lazy launch callback, wait for browser, then connect
2. If backend port set → connect to `127.0.0.1:<backendPort>`, pipe bidirectionally

On browser switch:
1. Old browser killed
2. New browser launched on new internal port
3. Proxy's backend target updated
4. All existing connections destroyed (forces child MCPs to reconnect)
5. Proxy port unchanged

### API

| Method | Description |
|--------|-------------|
| `listen(port)` | Start TCP server on the given port (0 = random) |
| `setBackend(port)` | Set the internal browser CDP port |
| `clearBackend()` | Clear the backend (browser stopped) |
| `closeConnections()` | Destroy all active connections |
| `getPort()` | Get the proxy's listening port |
| `close()` | Stop the proxy and close all connections |
| `onLazyLaunch(callback)` | Set the callback for lazy browser launch |

## State File (`state.ts`)

The coordinator writes its state to a well-known file so the `wrap` subcommand can discover the proxy port.

**Path:** `$TMPDIR/browser-coordinator/state.json`

**Contents:**
```json
{
  "port": 41837,
  "pid": 12345
}
```

### API

| Function | Description |
|----------|-------------|
| `writeState({ port, pid })` | Write state file (creates directory if needed) |
| `readState()` | Read and parse state file (returns null if missing/invalid) |
| `clearState()` | Remove state file |

### Lifecycle

```
Coordinator start:
  1. Start CDP proxy on port 0 (OS-assigned)
  2. Write state file with proxy port and PID
  3. Set up lazy launch callback
  4. Ready to accept CDP connections

During operation:
  - CDP connections → proxy to browser
  - coordinator_launch_browser → launch browser, update proxy backend
  - coordinator_stop_browser → stop browser, clear proxy backend
  - coordinator_restart_browser → stop, relaunch, update proxy backend

Coordinator shutdown:
  1. Stop browser (if running)
  2. Close CDP proxy (all connections)
  3. Remove state file
```

## Wrap Subcommand (`cli.ts`)

The `wrap` subcommand bridges the coordinator and child MCP:

```
npx @anthropic-community/browser-coordinator-mcp wrap -- <command> [args...]
```

### Flow

1. Read state file (poll with backoff for up to 10s)
2. Replace `{cdp_port}` → port number, `{cdp_endpoint}` → `http://localhost:<port>` in child args
3. Spawn child with `stdio: 'inherit'` (transparent passthrough to host)
4. Forward signals (SIGINT, SIGTERM, SIGHUP)
5. Exit with child's exit code
