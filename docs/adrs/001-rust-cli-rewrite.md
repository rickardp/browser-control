# ADR 001: Rust CLI rewrite

## Status

Accepted (2026-05-12).

## Context

The original project (`@anthropic-community/browser-coordinator-mcp`) was an
MCP-first TypeScript server. It bundled three concerns into a single Node
process: browser lifecycle management, a CDP reverse proxy for stable child-MCP
ports, and a grab-bag of in-browser helper tools (DOM, Markdown, screenshot,
fetch). In practice this caused several recurring problems:

- **Lifecycle thrash.** Agents called `coordinator_launch_browser` /
  `coordinator_stop_browser` / `coordinator_restart_browser` aggressively
  whenever a CDP call failed, killing the user's browser session.
- **One-process coupling.** A crash in any helper tool took down the proxy and
  every child MCP attached to it.
- **Two MCP entries in `.mcp.json`** (coordinator + Playwright) was awkward and
  the reverse-proxy state file was a frequent source of confusion.
- **Distribution.** npm-as-the-only-channel meant Node.js was always on the
  critical path, even for users whose only goal was "open a browser and let an
  agent drive it".
- **Multi-session coordination.** State was per-process and ephemeral, so two
  shells (or two agents) could not see each other's browsers.

## Decision

1. **Rewrite the tool in Rust as a CLI named `browser-control`.** A single
   statically-linkable binary, distributable without Node. The CLI is the
   primary surface; MCP is one of several ways to use it.

2. **Demote the MCP server to a `mcp` subcommand.** Hosts add
   `browser-control mcp` (or `browser-control mcp --playwright`) to their
   `.mcp.json` exactly like any other stdio server. No reverse proxy, no
   separate coordinator entry.

3. **Persist browser state in SQLite under the OS app-data dir.** A small
   registry of `(name, kind, pid, engine, endpoint, profile, started_at)` is
   shared across shells and agents, with lazy liveness checks on read. This
   replaces the in-memory and state-file approaches of the TS version.

4. **CLI does not stop or restart browsers.** Agents are trigger-happy and
   will kill the user's session at the first sign of trouble. Lifecycle is a
   user concern; the CLI only `start`s (idempotent by kind) and lists. Stale
   registry rows are pruned lazily, never via an explicit `stop` from an
   agent.

5. **Distribute via crates.io and a Homebrew tap.** `cargo install
   browser-control` for Rust users; `brew install
   rickardp/browser-control/browser-control` for everyone else. The legacy npm
   package is deprecated in place.

## Consequences

The MCP server becomes a much smaller piece of the project, which is a
deliberate downsizing: the tools that survived (`navigate`, `dom`,
`screenshot`, `fetch`, `select_element`) are the ones that genuinely benefit
from running inside the controller. Lifecycle tools are gone because they
caused more harm than good, and `get_markdown` is gone because we don't want
to port Turndown to Rust for v1.

Removing the CDP reverse proxy means child MCPs (like Playwright MCP) connect
directly to the browser's CDP endpoint. To make that ergonomic we added the
`mcp --playwright` stdio passthrough, which spawns `@playwright/mcp` with the
right `--cdp-endpoint` injected. Hosts see one entry in `.mcp.json`, not two.

Persistent SQLite state means two agents in two shells can coordinate without
file-watching tricks. It also means we own a small data file on disk; we mitigate
that with a strict schema, lazy cleanup, and no migrations across the 0.x line.

The hard cut from TypeScript to Rust is disruptive for existing users. We
preserve the old code on the `legacy-ts` branch (tagged `v0-final-ts`) and
deprecate the npm package, but there is no compatibility shim. The CLI is the
contract going forward; the `BROWSER_CONTROL` environment variable is the only
piece of cross-tool surface we commit to keeping stable through 1.0.

The VS Code extension is retained but heavily simplified: the Unix-socket IPC
is gone, replaced by injecting `BROWSER_CONTROL` into the extension-launched
terminal. This eliminates an entire class of platform-specific bugs.
