> **Note:** This document describes the legacy TypeScript implementation. The
> current Rust CLI is described in the project README and ADR-001. See
> `docs/adrs/archive/` for the original ADRs.

# Agent Boundaries

Rules and constraints for AI agents working in this codebase.

## Code Style

- TypeScript with ESM modules (`import`/`export`, not `require`)
- No bundler — code must compile with `tsc` alone
- Flat file structure in project root (no `src/` directory)
- Compiled output goes to `dist/`

## Architecture Constraints

- **The coordinator is a controller + CDP proxy, not an MCP proxy.** It manages browser lifecycle and provides a stable CDP port. It does not proxy MCP tool calls or merge tool lists.
- **Two independent MCP entries.** The coordinator and child MCP are configured as separate MCP servers in `.mcp.json`. The `wrap` subcommand bridges them via the state file.
- **Zero config by default.** User settings are optional, never required. The server must work with `npx @anthropic-community/browser-coordinator-mcp` and no flags.
- **Lazy browser launch.** Browsers must not spawn at server startup. They launch either when a CDP connection arrives at the proxy (lazy) or when `coordinator_launch_browser` is called (explicit).
- **Stable CDP proxy port.** The proxy port stays the same when the browser switches (Chrome → Edge) or restarts. Child MCPs never need restarting due to port changes.
- **Coordinator tools only.** The coordinator exposes only `coordinator_*` tools. Child MCP tools are managed by the child MCP independently.

## Browser Support

- **Supported:** Chrome, Edge, Chromium, Brave (all Chromium-based, CDP-compatible)
- **Aspirational:** Firefox (lacks CDP — not currently targetable)
- **Not supported:** Safari/WebKit

## VS Code Integration

- **Tier 1 (External Browser):** Always available, zero config. Must not require the companion extension.
- **Tier 2 (Native CDP):** Requires companion extension. Must degrade gracefully to Tier 1 if extension is not installed.
- **`--no-vscode` flag:** Must completely disable VS Code detection when set.

## What Not to Do

- Do not add a `src/` directory — keep the flat file structure
- Do not add a bundler (webpack, esbuild, rollup) — `tsc` is sufficient
- Do not proxy MCP tool calls — the coordinator only exposes its own tools
- Do not send `tools/list_changed` notifications — hosts don't reliably support it
- Do not eagerly connect to CDP — let the child MCP handle its own CDP connection timing
- Do not add configuration files (YAML, TOML, JSON config) — use CLI flags and env vars only
- Do not add database or persistence beyond the state file — this is a stateless process
