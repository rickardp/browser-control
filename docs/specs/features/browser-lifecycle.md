> **Note:** This document describes the legacy TypeScript implementation. The
> current Rust CLI is described in the project README and ADR-001. See
> `docs/adrs/archive/` for the original ADRs.

# Feature: Browser Lifecycle Management

## Overview

The coordinator manages the full lifecycle of a browser process: detection, port allocation, launch, health monitoring, restart, and cleanup. This is the core capability that enables child MCPs to focus purely on automation.

## Components

### Browser Detection (`browser-detector.ts`)

Scans the system for installed Chromium-based browsers.

**Supported browsers (in preference order):**
1. Chrome
2. Edge
3. Chromium
4. Brave

**Platform-specific detection:**

| Platform | Method |
|----------|--------|
| macOS | Check `/Applications/*.app/Contents/MacOS/*` paths |
| Linux | Check `/usr/bin/*` paths, fallback to `which` |
| Windows | Check `Program Files` paths via environment variables |

**API:**
- `detectBrowsers()` — returns all installed browsers with name, type, and path
- `findBrowser(preferredType?)` — returns first available browser, respecting optional preference

### Port Allocation (`browser-launcher.ts`)

Pre-allocates a free TCP port for CDP before launching the browser.

**Method:** Bind a temporary TCP server to port 0 (OS assigns a free port), record the port, close the server immediately. This is the same pattern used by Jest, Vitest, and Playwright.

**API:**
- `getFreePort()` — returns a free port number

### Browser Launch (`browser-launcher.ts`)

Spawns a browser process with CDP enabled on the pre-allocated port.

**Browser flags:**
```
--remote-debugging-port=<port>
--user-data-dir=<tmpdir>
--no-first-run
--no-default-browser-check
--disable-background-networking
--disable-default-apps
--disable-extensions
--disable-sync
--disable-translate
--metrics-recording-only
--mute-audio
--headless=new            (unless --no-headless)
about:blank               (initial page)
```

**Temporary profile:** Each launch creates an isolated user data directory in `$TMPDIR` to avoid interfering with the user's real browser profile.

**Readiness detection:** The CDP WebSocket URL is parsed from browser stderr output. The coordinator also waits for the CDP port to accept TCP connections (polling every 100ms, 15-second timeout).

**API:**
- `launchBrowser(port, options)` — returns `BrowserInstance` with process handle, port, WebSocket URL, and temp directory path
- `waitForPort(port, timeoutMs)` — waits for CDP port to accept connections

### Browser Shutdown (`browser-launcher.ts`)

Graceful two-phase shutdown:

1. Send `SIGTERM` to browser process
2. Wait up to 5 seconds for exit
3. If still alive, send `SIGKILL`
4. Delete temporary user data directory

**API:**
- `stopBrowser(instance)` — gracefully stops and cleans up

## Coordinator Tools

These tools are exposed to the AI host for direct browser management:

| Tool | Description | Requires Browser |
|------|-------------|-----------------|
| `coordinator_list_browsers` | List installed CDP-capable browsers | No |
| `coordinator_status` | Show current state (browser, proxy port, VS Code tier) | No |
| `coordinator_launch_browser` | Explicitly launch or relaunch with options | Creates one |
| `coordinator_stop_browser` | Stop running browser | Yes |
| `coordinator_restart_browser` | Kill and relaunch. Proxy port stays the same. | Yes |

## Lazy Launch Flow

The browser can be launched two ways:

### Trigger 1: CDP connection (lazy)
```
Startup:
  1. Start CDP proxy on port 0 (OS-assigned) → e.g., 41837
  2. Write state file: { port: 41837, pid: <pid> }
  3. No browser running — zero memory, zero CPU

First CDP connection (child MCP calls a tool):
  4. Child MCP connects to proxy port 41837
  5. Proxy sees no backend → triggers lazy launch callback
  6. getFreePort() → e.g., 52100 (internal port)
  7. launchBrowser(52100, opts) spawns Chrome
  8. Proxy sets backend to 52100, pipes connection through
  9. Child MCP's CDP session works transparently
```

### Trigger 2: Explicit launch (coordinator tool)
```
Agent calls coordinator_launch_browser:
  1. getFreePort() → internal port
  2. launchBrowser(port, opts) spawns browser
  3. Proxy backend updated to new internal port
  4. Existing proxy connections closed (child MCPs reconnect)
```
