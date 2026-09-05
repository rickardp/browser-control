# Engine parity: Chromium (CDP) vs Firefox (WebDriver BiDi)

`browser-control` presents one tool surface over two protocols. Most tools
behave identically on both engines; this page lists every place where they do
not, so agents and maintainers know what to expect before blaming a page.
"Chromium" means Chrome, Edge, Chromium and Brave over CDP; "Firefox" means
Firefox over WebDriver BiDi. See ADR-003 for the reasoning behind the native
tools and their BiDi arms.

Legend: **same** = no observable difference; **differs** = both work but not
identically; **Chromium-only** = returns `EngineUnsupported` (or is ignored)
on Firefox.

## Tool availability

| Tool | Chromium | Firefox | Notes |
|---|---|---|---|
| `browser_navigate`, `browser_get_html`, `browser_get_page_text`, `browser_fetch`, `browser_curl`, `browser_cookies`, `browser_storage_*`, `browser_wait_for_cookie`, `browser_select_element`, tab and browser management | yes | yes | same |
| `browser_eval` | yes | yes | differs: see *Evaluation* |
| `browser_snapshot`, `browser_find` | yes | yes | differs: see *Accessibility snapshot* |
| `browser_click` / `browser_type` / `browser_hover` / `browser_drag` with `ref` | yes | yes | differs: see *Input* |
| `browser_click` / `browser_type` / `browser_hover` / `browser_drag` with CSS `selector` | yes (Playwright sidecar, needs `bun`/`node`) | no | Chromium-only; use `ref` on Firefox |
| `browser_press_key` | yes (native) | yes (native) | CDP `Input.dispatchKeyEvent` / BiDi `input.performActions` |
| `browser_wait_for`, `browser_pdf_save` | yes (sidecar) | no | Chromium-only |
| `browser_take_screenshot` | yes | yes | differs: `max_width` is ignored on Firefox; see *Screenshots* |
| `browser_console_messages`, `browser_network_requests` | yes | yes (network needs Firefox 124+) | differs: see *Console and network capture* |
| `browser_network_body` | yes | no | Chromium-only: BiDi exposes bodies only through browser-side data collectors, which are deliberately not enabled |
| `browser_tab_foreground`, `browser-control tab foreground` | yes | no | Chromium-only: `Emulation.setFocusEmulationEnabled` has no BiDi equivalent; on Firefox a background tab stays hidden to the page (ADR-004) |

## Session and lifecycle

| Topic | Chromium | Firefox |
|---|---|---|
| Sessions per browser | Unlimited; every command attaches its own flat session | One WebDriver BiDi session per browser. The MCP server holds it for its lifetime (SQLite `bidi_lock`); a concurrent CLI command against the same registered Firefox waits up to 30 s, then fails with `BidiLockBusy` |
| Session release | Nothing to release | Firefox keeps the session alive after the socket closes, so every backend user sends `session.end` on exit (`TabBackend::shutdown`). A process that dies without it leaves the browser refusing `session.new` ("Maximum number of active sessions") until the browser restarts |
| Renderer crash detection | `Inspector.targetCrashed` short-circuits an in-flight op as `TabCrashed` | No crash event; a wedged context surfaces as `TabHung` after the per-op timeout |
| Tab titles (`browser_tab_list`, `list_targets`) | Real titles | Empty: `browsingContext.getTree` carries no title. `browser_snapshot` prints `document.title` from the walker instead |
| Readiness after `browser-control start --headless` | Endpoint probe succeeds | Known wart: the readiness probe can report a 30 s timeout although the browser is up and usable |
| Background tabs (minimized window, locked display) | Hidden to the page unless a `tab foreground-hold` process emulates foreground (`browser_tab_foreground` / `tab foreground`) | Hidden to the page; no emulation available |

## Evaluation (`browser_eval`, CLI `eval`, freshness probe)

| Topic | Chromium | Firefox |
|---|---|---|
| `await_promise` | Honoured | Ignored: BiDi always awaits |
| Object results | Serialised by value | Flattened from BiDi `[[key, value]]` pairs to plain JSON. `NaN`/`Infinity` become `null`; DOM nodes, functions, windows become `null`; `Map` becomes an object, `Set` an array |
| Script exceptions | Error with the exception description | Error with `exceptionDetails.text` |

## Accessibility snapshot (`browser_snapshot`, `browser_find`)

| Topic | Chromium | Firefox |
|---|---|---|
| Tree source | The browser's own accessibility tree (`Accessibility.getFullAXTree`) | An injected DOM walker (`src/dom/js/snapshot_tree.js`) that approximates roles and accessible names; pinned by `tests/walker_fixture.rs` |
| Accessible names | Full accname computation by the browser | Approximation: `aria-labelledby` → `aria-label` → `<label>` / `alt` / submit `value` / `legend` / `caption` / `figcaption` → text content for name-from-content roles → `title` → `placeholder`. Landmarks, lists, tables and dialogs are named only through ARIA and captions |
| Roles | Browser-computed, including Chromium-internal roles | Tag/ARIA table in the walker; unknown tags are `generic` and hoisted |
| Closed shadow roots | Visible (the AX tree is composed) | Invisible; elements inside them get no refs |
| Open shadow roots and `<slot>` | Visible | Visible |
| Iframe contents | Not included | Not included |
| Hidden elements | Present in the tree and dropped by the renderer; a `browser_snapshot { ref }` on a now-hidden node renders nothing | Skipped by the walker; a `browser_snapshot { ref }` on a now-hidden node reports "ref not found in the current accessibility tree" |
| `<option>` under a single `<select>` | Listed | Listed (named from `label`/text) |
| Ref identity | `backendDOMNodeId`, owned by the browser | Integer id in a page-side registry (`window.__bcRefs`). A page that clobbers that global makes refs report as stale |
| Document token (staleness) | The Document node's `backendDOMNodeId` | A random per-document stamp (`window.__bcDocToken`); a bfcache restore keeps refs valid on both |
| Large pages | Bounded by the 20 s snapshot timeout | Additionally capped at 20 000 emitted nodes (`truncated` logged) |
| Page header line | Title from the target list | Title from the walker's root node |

## Input (`ref` paths of click / type / hover / drag, screenshot `ref`)

| Topic | Chromium | Firefox |
|---|---|---|
| Click geometry | `DOM.scrollIntoViewIfNeeded` + `DOM.getContentQuads`, centre of the first visible quad clipped to the viewport | `scrollIntoView` + `getBoundingClientRect`, centre clipped to the viewport, rounded to integer CSS px |
| Click dispatch | `Input.dispatchMouseEvent` mouseMoved / mousePressed / mouseReleased | `input.performActions` pointerMove / pointerDown / pointerUp, then `input.releaseActions` |
| Occlusion | Not checked on either engine: a click on a covered element lands on the overlay | Same |
| `browser_type` default (fill) | `Input.insertText` after selecting existing content | In-page `document.execCommand('insertText')` after selecting; if the value did not take, falls back to the prototype `value` setter plus `input`/`change` events (keeps React-style value trackers in sync) |
| `browser_type { press_sequentially }` | One `Input.insertText` per character (no `keydown`/`keyup` events) | One `keyDown`/`keyUp` pair per character (real key events); `\n` becomes Enter |
| `browser_type { submit }` | Enter via `Input.dispatchKeyEvent` with `text: "\r"` | Enter via key action `` |
| Focus in background tabs | `Emulation.setFocusEmulationEnabled` so focus events fire | `element.focus()` only |
| `browser_drag` | Pointer sequence with 5 interpolated moves | Same sequence through `input.performActions` |
| HTML5 native drag-and-drop (`draggable`) | Not supported | Not supported |
| Stale ref detection | `DOM.getDocument` token compare, `NodeGone` from `DOM.*` errors | Token compare via `script.evaluate`, `NodeGone` when the registry lookup fails or the element is detached |

## Screenshots (`browser_take_screenshot`)

| Option | Chromium | Firefox |
|---|---|---|
| default (viewport PNG) | `Page.captureScreenshot` | `browsingContext.captureScreenshot` |
| `format`, `quality` | Supported | Supported |
| `full_page` | `captureBeyondViewport` | Document-origin box clip of the document's scroll size |
| `max_width` | Downscale via `clip.scale` | Ignored (BiDi has no scale) |
| `selector` clip | Document-coordinate rect from JS | Same |
| `ref` clip | `DOM.getBoxModel` border box | `getBoundingClientRect` plus scroll offsets |
| `save_to` | Supported | Supported |

## Console and network capture

| Topic | Chromium | Firefox |
|---|---|---|
| Mechanism | One long-lived CDP session per touched tab with `Runtime`, `Log`, `Network`, `Page` enabled | One global `session.subscribe` per backend for `log.entryAdded`, `browsingContext.navigationStarted` / `contextDestroyed`, and `network.beforeRequestSent` / `responseCompleted` / `fetchError`; events for untouched tabs are discarded |
| Minimum version | Any supported Chromium | Console: any supported Firefox. Network: Firefox 124+; older builds keep console capture and `browser_network_requests` returns an error naming the version |
| Console API calls | `Runtime.consoleAPICalled` with argument previews | `log.entryAdded` with structured `args` rendered by browser-control (Firefox's own `text` shows objects as `[object Object]`, so it is used only when there are no args) |
| `console.log` level | `[log]` | `[log]` (BiDi reports it as `info`; the console method decides) |
| Source labels | `console.warning` for `console.warn` | Normalised to `console.warning` too, so `pattern` filters match on both |
| Call-site location | Usually present | Often absent for top-level `console.*` calls; the source label is printed instead |
| Uncaught exceptions | `Runtime.exceptionThrown`, stack from the exception description | `log.entryAdded { type: "javascript" }`, stack synthesised from BiDi stack frames |
| Browser-generated log entries (failed resource loads, mixed content, CSP, deprecations) | Captured via `Log.entryAdded` | Not captured: check `browser_network_requests` with `status: "4xx"` or `"failed"` instead |
| Network request ids | Short CDP ids (`1234.56`) | Long BiDi ids; both are passed verbatim |
| `resource_type` | Exact CDP type | Derived from `request.initiatorType` / `destination` (Firefox 129+) and, at response time, the MIME type; may be absent |
| Redirects | Same request id, previous hop marked `[redirect]` | Same, using the 3xx `responseCompleted` |
| `page_url` stamping | `Page.frameNavigated` (commit time) | `browsingContext.navigationStarted` (slightly earlier) |
| Same-process iframes | Their console output and requests are captured (they share the tab's session) | Not captured: Firefox stamps them with the child context id |
| Workers and OOPIFs | Not captured | Not captured |
| First load of `browser_tab_new { url }` | Not captured (use `browser_tab_new` then `browser_navigate`) | Same |
| Response bodies | `browser_network_body` via `Network.getResponseBody` | Not available |
| Side effects | `Runtime.enable` is detectable by some anti-bot scripts; touched targets show `attached: true` and Chrome's automation infobar may stay visible | The subscription is not observable from page script |
| Opt-out | `BROWSER_CONTROL_CAPTURE=0` | Same |

## Playwright sidecar

The sidecar (`playwright-core` over `connectOverCDP`) is Chromium-only by
construction: Playwright cannot attach to a user-launched Firefox. On Firefox,
use refs instead of CSS selectors; `browser_wait_for` and `browser_pdf_save`
have no Firefox equivalent yet. `browser_press_key` does: it dispatches
`input.performActions` on Firefox and `Input.dispatchKeyEvent` on Chromium.

## Keeping this page honest

- `tests/walker_fixture.rs` pins the Firefox walker's output for
  `tests/fixtures/walker_sample.html`; regenerate the fixture when the walker
  changes (procedure in the test header).
- `src/session/capture.rs` has synthetic-event tests for both parsers; the
  BiDi ones encode the level, source and location rules above.
- When adding a `TabBackend` method, add a row here for its BiDi arm, even if
  the arm is `EngineUnsupported`.
