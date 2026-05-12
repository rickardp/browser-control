# browser-control

`browser-control` is a Rust CLI that manages browser processes and exposes them
over CDP (Chromium) or WebDriver BiDi (Firefox) for agent-driven development.
It keeps a small persistent registry of the browsers it has started so multiple
agents, shells, or editor sessions can coordinate on the same browser. An
optional MCP server is available as a subcommand.

## Install

Homebrew (recommended):

```sh
brew install rickardp/browser-control/browser-control
```

From source:

```sh
cargo install browser-control
```

Requires Rust 1.80 or newer when building from source. Node.js (for `npx`) is
only required if you intend to use `mcp --playwright`.

## Usage

The CLI exposes four subcommands. All of them accept `--json` (where listed
below) for machine-readable output.

### `list-installed`

Detect every supported browser installed on this machine.

```sh
browser-control list-installed
browser-control list-installed --json
```

Supported kinds: `chrome`, `edge`, `chromium`, `brave` (CDP), and `firefox`
(BiDi).

### `list-running`

List the browsers currently registered and alive. Stale entries (dead PIDs or
unreachable endpoints) are pruned lazily before printing.

```sh
browser-control list-running
```

Columns: `NAME`, `KIND`, `PID`, `ENGINE`, `ENDPOINT`, `PROFILE`, `STARTED`.

### `start [BROWSER]`

Start a browser and register it. Idempotent by kind: if a browser of the
requested kind is already alive, it is reused.

```sh
browser-control start                   # first available Chromium-based
browser-control start firefox
browser-control start chrome --headless
browser-control start chromium --profile ./my-profile --json
```

`BROWSER` may be a kind (`chrome`, `edge`, `chromium`, `brave`, `firefox`) or a
friendly instance name printed by a previous `start` (e.g. `firefox-pikachu`).
When omitted, the first available Chromium-based browser is used.

### `mcp [BROWSER] [--playwright]`

Start an MCP server on stdio that targets a running browser.

```sh
browser-control mcp                     # use most-recently-started browser
browser-control mcp firefox             # target a specific kind
browser-control mcp --playwright        # passthrough to @playwright/mcp
```

Browser resolution order:

1. `BROWSER_CONTROL` environment variable
2. The positional `BROWSER` argument
3. The most-recently-started running browser in the registry
4. Otherwise, exit with an error suggesting `browser-control start`

With `--playwright`, the CLI spawns the official `@playwright/mcp` via `npx`,
hands it the resolved CDP endpoint, and forwards stdio bidirectionally. The
host sees only Playwright MCP's tools; `browser-control`'s own MCP tools are
not exposed in that mode.

## The `BROWSER_CONTROL` environment variable

A single environment variable selects which browser the current shell session
should talk to. The syntax of the value decides how it is interpreted:

| Value form                              | Behavior                                                                                       |
|-----------------------------------------|------------------------------------------------------------------------------------------------|
| `http(s)://…` or `ws(s)://…` URL        | External CDP/BiDi endpoint. Used as-is; not registered and not managed by `browser-control`.   |
| Friendly name (e.g. `firefox-pikachu`)  | Exact match against the registry.                                                              |
| Kind (`chrome`, `firefox`, …)           | First running instance of that kind in the registry.                                           |
| Absolute path to a browser executable   | Matched against `list-installed` to derive the kind, then resolved as a kind.                  |

Engine (CDP vs BiDi) is auto-detected for URL forms by probing.

## MCP integration

`browser-control` is itself an MCP server when invoked as `mcp`. Add it to
your host's `.mcp.json` like any other stdio server.

Default tools (exposed by the Rust server):

```json
{
  "mcpServers": {
    "browser-control": {
      "command": "browser-control",
      "args": ["mcp"]
    }
  }
}
```

With `--playwright` passthrough (Playwright MCP's tool surface, but driving
the browser that `browser-control` manages):

```json
{
  "mcpServers": {
    "browser-control": {
      "command": "browser-control",
      "args": ["mcp", "--playwright"]
    }
  }
}
```

You can scope a single host invocation to a specific browser by setting
`BROWSER_CONTROL`:

```json
{
  "mcpServers": {
    "browser-control": {
      "command": "browser-control",
      "args": ["mcp"],
      "env": { "BROWSER_CONTROL": "firefox" }
    }
  }
}
```

## Architecture

`browser-control` is a thin CLI in front of a SQLite registry of browser
processes. The CLI starts and tracks browsers; agents talk to those browsers
directly over CDP or BiDi. The MCP server is just another way to reach the
same registry.

```
                  ┌───────────────────────────────────────┐
                  │ SQLite registry (OS app-data dir)     │
                  └───────────────────────────────────────┘
                                   ▲
                                   │ read / write
                                   │
   user ──► browser-control start ─┴─► spawns ──► Browser (Chrome/Edge/Firefox/…)
                                                         ▲
                                                         │ CDP / BiDi
                                                         │
              MCP host ──► browser-control mcp [--playwright] ┘
                              (resolves browser via registry / BROWSER_CONTROL)
```

The CLI does not stop or restart browsers; the user owns lifecycle. Stale
registry entries are pruned lazily on read.

## Status

Pre-1.0. The CLI surface and the `BROWSER_CONTROL` environment variable are
the intended stable contracts; everything else may shift.

The previous TypeScript MCP server (`@anthropic-community/browser-coordinator-mcp`)
is preserved on the `legacy-ts` branch and tagged `v0-final-ts`. Its npm
package is deprecated.

## License

MIT. See [LICENSE](LICENSE).
