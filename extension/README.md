# Browser Control (VS Code companion)

Companion extension for the [`browser-control`](https://github.com/rickardp/browser-control) Rust CLI.

## What it does

When VS Code's Simple Browser is open with a Chrome DevTools Protocol (CDP)
debugging port exposed, this extension:

1. **Discovers** the CDP endpoint by probing well-known ports (9222–9229) on
   `127.0.0.1` for a `/json/version` response, or uses the explicit
   `browserControl.endpoint` setting if provided.
2. **Injects** `BROWSER_CONTROL=<cdp-url>` into every integrated terminal via
   `EnvironmentVariableCollection`. Any `browser-control mcp` invocation
   started from a VS Code terminal will then target the embedded browser.
3. Shows the active endpoint in the **status bar**. Click to copy.

## Commands

- `Browser Control: Copy CDP Endpoint` — copy the discovered endpoint to clipboard.
- `Browser Control: Refresh CDP Endpoint` — re-run discovery (e.g. after launching the Simple Browser).

## Settings

- `browserControl.endpoint` — explicit CDP endpoint URL; overrides auto-discovery.
- `browserControl.probePorts` — list of TCP ports to probe (default `9222..9229`).

## How it integrates with `browser-control`

The Rust CLI reads `BROWSER_CONTROL` from the environment and connects to that
CDP endpoint instead of launching its own browser. This extension is just a
discovery helper — there is no IPC, no socket, no server.
