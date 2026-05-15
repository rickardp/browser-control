# browser-control

`browser-control` is a Rust CLI that manages browser processes and exposes them
over CDP (Chromium) or WebDriver BiDi (Firefox) for agent-driven development.
It keeps a small persistent registry of the browsers it has started so multiple
agents, shells, or editor sessions can coordinate on the same browser. An
optional MCP server is available as a subcommand.

## Install

Homebrew (recommended) — tap this repo, then install:

```sh
brew tap rickardp/browser-control https://github.com/rickardp/browser-control.git
brew install browser-control
```

The formula is rendered into [`Formula/browser-control.rb`](Formula/browser-control.rb) by CI on every release, so the tap URL above is all you need.

From crates.io:

```sh
cargo install browser-control
```

Prebuilt binaries for macOS (x86_64/aarch64), Linux (x86_64/aarch64) and Windows (x86_64) are attached to every [GitHub Release](https://github.com/rickardp/browser-control/releases).

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

`--json` adds engine-specific endpoint details for tooling integration:
`cdp_port` and `cdp_ws_url` for CDP browsers, `bidi_ws_url` for Firefox.
Stale rows are re-probed before the WS URLs are printed; fields are omitted
when the probe fails.

### `start [BROWSER]`

Start a browser and register it. Idempotent by kind: if a browser of the
requested kind is already alive, it is reused.

```sh
browser-control start                   # first available Chromium-based
browser-control start firefox
browser-control start chrome --headless --json
```

`BROWSER` may be a kind (`chrome`, `edge`, `chromium`, `brave`, `firefox`) or a
friendly instance name printed by a previous `start` (e.g. `firefox-pikachu`).
When omitted, the first available Chromium-based browser is used.

`start` blocks until the browser's debugging endpoint is reachable (up to
`--wait-timeout` seconds, default 30) so the next command in a chain can
attach immediately. Pass `--no-wait` to return as soon as the process is
spawned.

`start` always uses a stable per-kind profile directory under the OS app-data
dir (macOS: `~/Library/Application Support/browser-control/profiles/<kind>/default/`;
Linux: `~/.config/browser-control/profiles/<kind>/default/`;
Windows: `%APPDATA%\browser-control\profiles\<kind>\default\`), so subsequent
starts of the same kind reuse the same browser state across reboots. This is
intentional: it avoids re-authenticating in every new browser session.

### `mcp [BROWSER] [--playwright]`

Start an MCP server on stdio that targets a running browser.

```sh
browser-control mcp                     # use most-recently-started browser
browser-control mcp firefox             # target a specific kind
browser-control mcp --playwright        # passthrough to @playwright/mcp
```

Browser resolution order:

1. The positional `BROWSER` argument (or `BROWSER_CONTROL` env, merged by clap; the argument wins when both are present)
2. The persisted default from `browser-control set default <value>`
3. The most-recently-started running browser in the registry
4. Otherwise, exit with an error suggesting `browser-control start`

With `--playwright`, the CLI spawns the official `@playwright/mcp` via `npx`,
hands it the resolved CDP endpoint, and forwards stdio bidirectionally. The
host sees only Playwright MCP's tools; `browser-control`'s own MCP tools are
not exposed in that mode.

### `set | get | unset <KEY> [VALUE]`

Manage persistent settings. The only key today is `default`, which selects the
browser used by `mcp` when no positional argument and no `BROWSER_CONTROL` env
var is present. Values accept the full `BROWSER_CONTROL` grammar (URL / kind /
friendly name / absolute path) and are validated at set-time.

```sh
browser-control set default firefox
browser-control set default ws://127.0.0.1:9222/devtools/browser/abc
browser-control get default
browser-control unset default
```

The setting is stored as TOML at:

- macOS: `~/Library/Application Support/browser-control/config.toml`
- Linux: `~/.config/browser-control/config.toml`
- Windows: `%APPDATA%\browser-control\config.toml`

Override the directory with `BROWSER_CONTROL_CONFIG_DIR`.

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

## HTTP, cookies, and storage

A small set of session subcommands lets agents and shell scripts use a *real*
browser session — with its cookies, headers, TLS stack, ad-blockers, and geo —
without scraping `cookies.sqlite`, re-implementing OAuth flows, or driving a
second headless browser. They attach to a browser already registered by
`start`, work over both CDP and BiDi (Firefox), and accept the same
`BROWSER_CONTROL` resolution rules as every other subcommand. None of them
launch a browser; run `start` first.

### `targets`

List open page targets (and optionally filter by URL regex).

```sh
browser-control targets                                    # table: KIND ID URL TITLE
browser-control targets --url '^https://example\.com'      # filter by URL regex
browser-control targets --json                             # machine-readable
```

### `cookies`

Export cookies from the live browser, normalised across CDP and BiDi.

```sh
browser-control cookies --domain '\.example\.com$' --name '^session'    # JSON (default)
browser-control cookies --format header                                 # 'Cookie: a=…; b=…'
browser-control cookies --format netscape -o cookies.txt                # curl/yt-dlp jar (0600)
browser-control cookies --reveal                                        # print values to stdout
```

`--domain` and `--name` are unanchored Rust regexes. Without `--reveal`,
values printed to a TTY are redacted; file output via `-o` always contains
full values and is `chmod 0600` on Unix. `--format netscape` produces a file
byte-compatible with the Mozilla `cookies.txt` format (see
[docs/session-ops.md](docs/session-ops.md)).

### `fetch`

Run an HTTP request from inside the page's JavaScript context. Cookies,
`Origin`, CORS, and the browser's TLS stack apply — handy for hitting an API
that requires the user's session.

```sh
browser-control fetch https://example.com/api/me
browser-control fetch -X POST -H 'Content-Type: application/json' \
    -d '{"q":1}' https://example.com/api/search
browser-control fetch --target '^https://app\.example\.com' -i \
    -o body.json https://app.example.com/api/data
```

`-i` prepends status line + response headers (like `curl -i`). `-o FILE`
writes the body to FILE (0600 on Unix). `--target URLREGEX` picks which page
to evaluate in when several are open.

### `storage`

Read and write `localStorage` (default) or `sessionStorage` (`--namespace
session`). Storage is origin-scoped, so most uses want `--target`.

```sh
browser-control storage get auth_token --target '^https://app\.example\.com'
browser-control storage set theme dark --target '^https://app\.example\.com'
browser-control storage list --namespace session --key-regex '^feature_' --json
```

### `eval`

Evaluate a JavaScript expression in the active page. Returns the result as
plain text by default; `--json` emits the full evaluation envelope.

```sh
browser-control eval 'document.title'
browser-control eval --target '^https://app\.example\.com' \
    --json 'fetch("/api/whoami").then(r => r.json())'
```

`--await-promise` is on by default, so async expressions just work.

### `wait`

Block until the browser's CDP / BiDi endpoint is up. Useful right after
`start` in scripts.

```sh
browser-control start firefox && browser-control wait --ready --timeout 30
```

### `wait-for-cookie`

Block until a cookie matching `--domain REGEX --name REGEX` exists in the
browser. Optional `--validate-url URL` follows up with a `fetch()` from the
page and requires a 2xx response before exiting — the typical pattern for
"wait until the user has finished logging in".

```sh
browser-control wait-for-cookie \
    --domain '\.example\.com$' --name '^session_token$' --timeout 120
browser-control wait-for-cookie --domain example.com --name auth \
    --validate-url https://example.com/api/session
```

Exit status is `0` on match, non-zero on timeout.

### Migrating from hand-rolled helpers

A typical "launch browser, wait for login, call API" shell flow collapses to:

```sh
browser-control start brave
browser-control wait-for-cookie --domain clientzone.gamesglobal.com \
    --name '__Secure-next-auth.session-token' --timeout 120
SESSION_JSON=$(browser-control fetch \
    https://clientzone.gamesglobal.com/api/auth/session)
```

And any Python `write_netscape_cookie_jar()` helper that reads
`cookies.sqlite` directly is replaced by:

```sh
browser-control cookies --format netscape -o cookies.txt
# then: curl --cookie cookies.txt https://…   or   yt-dlp --cookies cookies.txt …
```

## MCP integration

`browser-control` is itself an MCP server when invoked as `mcp`. Add it to
your host's `.mcp.json` like any other stdio server.

Default tools (exposed by the Rust server) include `navigate`, `get_dom`,
`screenshot`, `fetch`, `select_element`, plus the session ops introduced in
this release: `list_targets`, `cookies`, `storage_get`, `storage_set`, and
`wait_for_cookie`. See [docs/session-ops.md](docs/session-ops.md) for the
underlying model.

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
