> **Note:** This document describes the legacy TypeScript implementation. The
> current Rust CLI is described in the project README and ADR-001. See
> `docs/adrs/archive/` for the original ADRs.

# Tech Stack Specification

## Runtime

- **Node.js** >= 18 (ESM modules, `"type": "module"`)
- **TypeScript** 5.7+ (compiled via `tsc`, no bundler)

## Dependencies

### Production

| Package | Version | Purpose |
|---------|---------|---------|
| `@modelcontextprotocol/sdk` | ^1.12.0 | MCP server implementation |

### Development

| Package | Version | Purpose |
|---------|---------|---------|
| `typescript` | ^5.7.0 | TypeScript compiler |
| `@types/node` | ^22.0.0 | Node.js type definitions |

## Project Structure

```
browser-control-mcp/
  cli.ts                     # Entry point, arg parsing, wrap subcommand, stdio transport
  coordinator-server.ts      # Core MCP server, coordinator tools, CDP proxy integration
  cdp-proxy.ts               # TCP reverse proxy for CDP connections
  state.ts                   # State file management (proxy port discovery)
  browser-launcher.ts        # Port allocation, browser spawn, lifecycle
  browser-detector.ts        # Cross-platform browser detection
  vscode-integration.ts      # VS Code environment detection, CDP discovery
  extension.ts               # VS Code companion extension
  package.json
  README.md
  docs/
    adrs/                    # Architecture Decision Records
    specs/                   # Product and technical specifications
      features/              # Feature specifications
  CLAUDE.md                  # Agent instructions
```

## Commands

| Command | Description |
|---------|-------------|
| `bun run build` | Compile TypeScript (`tsc`) |
| `bun run dev` | Watch mode (`tsc --watch`) |
| `bun start` | Run compiled server (`node dist/cli.js`) |
| `bun run prepare` | Pre-install build hook (`tsc`) |

## CLI

```
npx @anthropic-community/browser-coordinator-mcp [options]
npx @anthropic-community/browser-coordinator-mcp wrap -- <command> [args...]

Modes:
  (default)               Start the coordinator MCP server
  wrap                    Read state file, inject CDP port, run child command

Options:
  --browser <type>        Preferred browser: chrome, edge, chromium, brave
  --browser-path <path>   Explicit browser executable path
  --no-headless           Show browser UI (default: headless)
  --no-vscode             Disable VS Code integration detection
  --help, -h              Show help

Wrap template variables:
  {cdp_port}              Replaced with the CDP proxy port number
  {cdp_endpoint}          Replaced with http://localhost:<port>
```

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `BROWSER_COORDINATOR_DEBUG=1` | Enable debug logging to stderr |
| `VSCODE_CDP_PORT=<port>` | VS Code CDP port (set by companion extension) |

## Conventions

- All source files are in the project root (flat structure, no `src/` directory)
- Output compiles to `dist/`
- Tests use Bun's built-in test runner (`bun test`)
- No bundler — raw `tsc` compilation
