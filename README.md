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

On Windows, install via the PowerShell one-liner:

```powershell
irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/install.ps1 | iex
```

It downloads the latest release zip, extracts `browser-control.exe` to `%USERPROFILE%\.browser-control\bin`, and prepends that directory to your user `PATH`. The script is idempotent — re-running it upgrades to the latest release, and is a no-op when the requested version is already installed. To pin a version, force a reinstall, or skip the PATH update:

```powershell
$script = irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/install.ps1
& ([scriptblock]::Create($script)) -Version 0.3.5
& ([scriptblock]::Create($script)) -Force
& ([scriptblock]::Create($script)) -NoPathUpdate
```

To uninstall:

```powershell
irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/uninstall.ps1 | iex
```

By default the user data directory (`%APPDATA%\browser-control`, containing the browser registry and config) is preserved. Pass `-Purge` to remove it as well:

```powershell
$script = irm https://raw.githubusercontent.com/rickardp/browser-control/main/scripts/uninstall.ps1
& ([scriptblock]::Create($script)) -Purge
```

Requires Rust 1.80 or newer when building from source. A Node runtime
(`bun` preferred, `node`+`npm` accepted) is required only if you invoke the
Playwright-only MCP tools (`browser_click`, `browser_snapshot`, etc.); the
sidecar is spawned lazily on first use.

## Usage

The CLI groups commands into lifecycle, config, browser-wide session ops,
page-context ops, and named tabs. Most commands accept `--json` for
machine-readable output.

### Browser selection

Page-context commands (`eval`, `fetch`, `storage`, …) and browser-wide
commands (`targets`, `cookies`, `wait`, `wait-for-cookie`) accept a
`--browser` / `-b` flag (with `$BROWSER_CONTROL` env fallback) to select
the target browser:

```sh
browser-control eval -b firefox 'document.title'
BROWSER_CONTROL=chrome browser-control cookies --format netscape
browser-control fetch --browser brave https://example.com/api/me
```

Resolution order: `--browser` flag → `$BROWSER_CONTROL` env →
`browser-control set default <value>` → error.

Page-context commands also accept a tab suffix: `--browser brave/cart`
routes to a named tab. Browser-wide commands reject tab suffixes with an
error.

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

GUI browser launches are kept in the background by default. Chromium-family
browsers are started minimized, automated CDP-created tabs are opened in the
background, and macOS launches hide the just-started browser after the debug
endpoint is ready. Use `show` only when a human needs to interact with the
browser.

### `show [--browser BROWSER]`

Explicitly reveal the selected browser for login or debugging.

```sh
browser-control show -b brave
browser-control show --json
```

`show` uses the same browser selector rules as other browser-wide commands. It
activates the browser app when possible and brings a live page target to the
front, creating an `about:blank` target if none exists.

### `mcp [--browser BROWSER] [--playwright-version <X.Y.Z>]`

Start an MCP server on stdio that targets a running browser.

```sh
browser-control mcp                             # use persisted default browser
browser-control mcp -b firefox                   # target a specific kind
browser-control mcp --playwright-version 1.55    # pin a custom playwright-core
```

Browser resolution order:

1. The `--browser` / `-b` flag (or `BROWSER_CONTROL` env, merged by clap; the flag wins when both are present)
2. The persisted default from `browser-control set default <value>`
3. Otherwise, exit with an error

The server exposes engine-agnostic tools (`browser_navigate`, `browser_get_html`,
`browser_fetch`, `browser_take_screenshot`, `browser_storage_get`,
`browser_cookies`, …) that work on every supported browser including Firefox.
Playwright-only interaction tools (`browser_click`,
`browser_type`, `browser_snapshot`, `browser_press_key`, `browser_drag`,
`browser_hover`, `browser_wait_for`, `browser_pdf_save`) route through an
internal Node sidecar that wraps `playwright-core`. On the first call to one
of these tools the sidecar is spawned (prefers `bun`, falls back to `node`+`npm`)
against the active browser's CDP endpoint; on Firefox they return
`EngineUnsupported`. The `--playwright-version` flag overrides the pinned
`playwright-core` version for the sidecar.

`browser_select` switches the MCP server's active browser before preparing
engine-specific state such as the Firefox BiDi lock. If that preparation fails
(for example, another process holds Firefox's single BiDi session), the server
keeps the newly selected browser active and reports the failure. The caller can
then retry the same selection after the lock clears, switch to another browser,
or switch back explicitly.

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
should talk to. It serves as the fallback for the `--browser` / `-b` flag on
most subcommands. The syntax of the value decides how it is interpreted:

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
`start`, work over both CDP and BiDi (Firefox), and accept `--browser` / `-b`
(or `$BROWSER_CONTROL`) for browser selection. None of them launch a browser;
run `start` first.

### `targets`

List open page targets (and optionally filter by URL regex). Browser-wide —
tab suffixes are not supported.

```sh
browser-control targets                                        # table: KIND ID URL TITLE
browser-control targets -b firefox --url '^https://example\.com'
browser-control targets --json
```

### `cookies`

Export cookies from the live browser, normalised across CDP and BiDi.
Browser-wide — tab suffixes are not supported.

```sh
browser-control cookies --domain '\.example\.com$' --name '^session'    # JSON (default)
browser-control cookies --format header
browser-control cookies -b brave --format netscape -o cookies.txt       # curl/yt-dlp jar (0600)
browser-control cookies --reveal
```

`--domain` and `--name` are unanchored Rust regexes. Without `--reveal`,
values printed to a TTY are redacted; file output via `-o` always contains
full values and is `chmod 0600` on Unix. `--format netscape` produces a file
byte-compatible with the Mozilla `cookies.txt` format (see
[docs/session-ops.md](docs/session-ops.md)).

Page-context reads that commonly surface auth state (`fetch`, `eval`,
`storage get`, `storage list`, and `wait-for-cookie --validate-url`) reload
HTTP(S) pages whose document is older than 10 minutes before evaluating, so
SSO has a chance to refresh tokens. Override with `--max-age 1h`, `--max-age
30s`, etc.

### `fetch`

Run an HTTP request from inside the page's JavaScript context. Cookies,
`Origin`, CORS, and the browser's TLS stack apply — handy for hitting an API
that requires the user's session. Page-context — supports tab suffixes via
`-b browser/tab`.

```sh
browser-control fetch https://example.com/api/me
browser-control fetch -b brave -X POST -H 'Content-Type: application/json' \
    -d '{"q":1}' https://example.com/api/search
browser-control fetch --target '^https://app\.example\.com' -i \
    -o body.json https://app.example.com/api/data
```

`-i` prepends status line + response headers (like `curl -i`). `-o FILE`
writes the body to FILE (0600 on Unix).

By default `fetch` runs in a tab on the URL's origin, reusing an existing
same-origin tab if one is open and otherwise opening a new tab navigated
to the origin root. This guarantees the request carries the cookies and
honours the CORS rules of the target site, regardless of which tab the
user is currently looking at. The auto-opened tab is left open so
subsequent fetches against the same origin reuse it. Pass `--target
URLREGEX` to override and explicitly pick a tab by URL regex.

### `storage`

Read and write `localStorage` (default) or `sessionStorage` (`--namespace
session`). Storage is origin-scoped, so most uses want `--target`.
Page-context — supports tab suffixes via `-b browser/tab`.

```sh
browser-control storage get auth_token --target '^https://app\.example\.com'
browser-control storage set theme dark -b brave --target '^https://app\.example\.com'
browser-control storage list --namespace session --key-regex '^feature_' --json
```

### `eval`

Evaluate a JavaScript expression in the active page. Returns the result as
plain text by default; `--json` emits the full evaluation envelope.
Page-context — supports tab suffixes via `-b browser/tab`.

```sh
browser-control eval 'document.title'
browser-control eval -b brave/cart --json 'fetch("/api/whoami").then(r => r.json())'
browser-control eval --target '^https://app\.example\.com' 'document.cookie'
```

`--await-promise` is on by default, so async expressions just work.

### `wait`

Block until the browser's CDP / BiDi endpoint is up. Useful right after
`start` in scripts. Browser-wide — tab suffixes are not supported.

```sh
browser-control start firefox && browser-control wait -b firefox --timeout 30
```

### `wait-for-cookie`

Block until a cookie matching `--domain REGEX --name REGEX` exists in the
browser. Optional `--validate-url URL` follows up with a `fetch()` from the
page and requires a 2xx response before exiting — the typical pattern for
"wait until the user has finished logging in". Browser-wide — tab suffixes
are not supported.

```sh
browser-control wait-for-cookie \
    --domain '\.example\.com$' --name '^session_token$' --timeout 120
browser-control wait-for-cookie -b brave --domain example.com --name auth \
    --validate-url https://example.com/api/session
```

Exit status is `0` on match, non-zero on timeout.

### Migrating from hand-rolled helpers

A typical "launch browser, wait for login, call API" shell flow collapses to:

```sh
browser-control start brave
browser-control wait-for-cookie -b brave --domain clientzone.gamesglobal.com \
    --name '__Secure-next-auth.session-token' --timeout 120
SESSION_JSON=$(browser-control fetch -b brave \
    https://clientzone.gamesglobal.com/api/auth/session)
```

And any Python `write_netscape_cookie_jar()` helper that reads
`cookies.sqlite` directly is replaced by:

```sh
browser-control cookies --format netscape -o cookies.txt
# then: curl --cookie cookies.txt https://…   or   yt-dlp --cookies cookies.txt …
```

### Named tabs

Named tabs let agents manage isolated tab contexts. `tab open` creates
(or reuses) a named tab; `tab list` shows them; `tab adopt` binds an
existing unnamed tab to a name.

```sh
browser-control tab open brave/cart https://shop.example.com     # create named tab
browser-control tab list brave                                    # list registered tabs
browser-control tab list brave --all                              # include unnamed live tabs
browser-control tab adopt brave/my-tab ABC123DEF                  # adopt by target ID
browser-control eval -b brave/cart 'document.title'               # use in page-context commands
```

`tab list --all` surfaces unnamed tabs with their target IDs. Use
`tab adopt <browser>/<name> <target-id>` to bind them to a name, making
them addressable via `-b <browser>/<name>` in `eval`, `fetch`, `storage`.

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
              MCP host ──► browser-control mcp ┘
                              (resolves browser via registry / BROWSER_CONTROL)
                              (Playwright tools route through internal sidecar)
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
