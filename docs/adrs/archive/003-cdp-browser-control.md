> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: accepted
date: 2025-01-15
---

# ADR-003: CDP as Browser Control Protocol

## Context

The coordinator needs to launch browsers with remote debugging enabled so that child MCPs (like Playwright MCP) can connect and automate them. Multiple browser automation protocols exist.

## Decision

Use Chrome DevTools Protocol (CDP) as the browser control protocol. Browsers are launched with `--remote-debugging-port=<port>` and child MCPs connect via CDP WebSocket.

## Consequences

### Positive

- CDP is natively supported by all Chromium-based browsers (Chrome, Edge, Chromium, Brave)
- Playwright MCP uses CDP for browser connection — direct compatibility
- Port-based architecture enables the coordinator to pre-allocate a port, pass it to the child MCP, and launch the browser on the same port later (lazy launch)
- CDP WebSocket URL is emitted on browser stderr — easy to parse and verify readiness
- VS Code's internal Chromium also exposes CDP when configured with `--remote-debugging-port`

### Negative

- Firefox does not support CDP (uses its own remote protocol) — Firefox support is aspirational only
- Safari/WebKit lack CDP support — not targetable
- CDP is a Chromium implementation detail, not a formal standard (though widely adopted)

### Neutral

- The coordinator does not speak CDP directly — it only manages the port and process. The child MCP handles actual CDP communication.
