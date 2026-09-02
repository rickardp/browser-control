//! Canonical instructions for agents using `browser-control`.

pub const AGENT_INSTRUCTIONS: &str = r#"browser-control agent instructions

Use the highest-level browser-control primitive available.
- If MCP tools are available, prefer them over shell commands.
- If MCP tools are not available, use the `browser-control` CLI.
- Do not drive raw CDP/BiDi, scrape browser profile databases, or launch a separate browser unless browser-control lacks the needed primitive.
- Use `--json` for CLI output that another tool or agent will parse.

Browser selection
- Select a browser with `browser_select` in MCP, or with CLI `-b/--browser`, `$BROWSER_CONTROL`, or `browser-control set default <selector>`.
- Selectors may be a kind (`chrome`, `edge`, `chromium`, `brave`, `firefox`), a friendly name from `list-running`, an absolute executable path, or a CDP/BiDi endpoint URL.
- If nothing is running, use `browser-control start <kind>`. It reuses a persistent per-kind profile so login state survives.
- Browser windows and automated tabs stay in the background by default. Reveal the browser only when human interaction is needed: use MCP `browser_show` or CLI `browser-control show -b <browser>`.

Tabs
- Prefer tab primitives over target IDs:
  MCP: `browser_tab_list`, `browser_tab_new`, `browser_tab_select`, `browser_tab_close`.
  CLI: `browser-control tab open <browser>/<name> [url]`, `tab list <browser> --all`, `tab adopt <browser>/<name> <target-id>`.
- For repeatable work, create or select a named tab, then address it as `<browser>/<tab>` in page-context CLI commands.
- Use target IDs only to adopt an existing unnamed tab or as a last-resort diagnostic.
- Browser-wide operations do not take tab names. Page-context operations do.

Page and network work
- Navigate with `browser_navigate`. Read pages with `browser_snapshot` (accessibility tree with `[ref=eN]` handles; add `interactive_only: true` for forms) or `browser_get_page_text` (cheapest, article-first text). Use `browser_get_html` only when you need markup and `browser_eval` when no higher-level primitive fits.
- Act on refs: `browser_find` returns refs for a short description ("search box", "Sign in button"); `browser_click`, `browser_type` (`submit: true` presses Enter), `browser_hover`, `browser_drag`, and `browser_take_screenshot` all accept a `ref`. Refs stay valid until the page navigates; on `StaleRef`, take a new snapshot. CSS `selector` remains available and routes through the Playwright sidecar.
- Screenshots are expensive in context. Take one only for a visual check, and pass `format: "jpeg"` with `max_width` (e.g. 1024), or `save_to` to write the file to disk and keep it out of the conversation.
- CLI navigation: `browser-control tab open <browser>/<name> <url>` opens or navigates a named tab (re-running with a new url navigates the existing tab). This is how you go to a URL from the CLI — never navigate by evaling `location.href`, which bypasses the tab registry and races the page load.
- Fetch authenticated APIs with `browser_fetch` or `browser-control fetch`; this runs inside the browser context so cookies, Origin, CORS, and the browser TLS stack apply.
- For large responses, binary downloads, or requests that should not run under page CORS/CSP, use `browser_curl` or `browser-control curl`. It invokes the real curl with a temporary browser cookie jar plus User-Agent, Origin, and Referer derived from the source tab. MCP responses are capped at 8 MiB; pass curl `-o <path>` for unrestricted streaming to disk.
- Read/write storage with `browser_storage_get` / `browser_storage_set` or `browser-control storage`.
- Evaluate JavaScript with `browser_eval` or `browser-control eval` when no higher-level primitive fits.
- Auth-sensitive reads reload HTTP(S) pages older than 10 minutes before evaluating so SSO can refresh tokens. CLI callers can override with `--max-age 1h`; MCP callers can pass `max_age`.

Console and network
- The MCP server captures console output and network requests for every tab a tool has touched (navigate, select, snapshot, …), from that moment on. Read them with `browser_console_messages` and `browser_network_requests`; fetch a response body with `browser_network_body` and the printed request id.
- Always pass `pattern` / `url_pattern` or `only_errors` on busy pages, and `clear: true` before an action to isolate its effects. Buffers persist across navigations; each entry shows the page it came from.
- `browser_tab_new` with a URL cannot capture that very first load; use `browser_tab_new` then `browser_navigate` when the initial traffic matters. Chromium only; on Firefox use `browser_eval` (a `window.onerror` hook, `performance.getEntriesByType('resource')`).

Cookies and login
- Wait for login with `browser_wait_for_cookie` or `browser-control wait-for-cookie --domain <regex> --name <regex>`.
- Add `--validate-url <url>` when the cookie alone is not enough and an authenticated endpoint must return 2xx.
- Export cookies with `browser_cookies` or `browser-control cookies`; use `--format netscape -o cookies.txt` for curl, wget, or yt-dlp.

MCP server setup
- Configure the host with command `browser-control` and args `["mcp"]`.
- Set `BROWSER_CONTROL` in the MCP host env to scope that server to one browser, or rely on `browser-control set default`.
- `browser_snapshot`, `browser_find`, ref-based interaction, and console/network capture are native CDP (no Node) but Chromium-only. CSS-selector interaction, `browser_press_key`, `browser_wait_for`, and `browser_pdf_save` route through a Playwright sidecar that needs `bun` or `node`. On Firefox, use the engine-agnostic primitives instead.
- Set `BROWSER_CONTROL_CAPTURE=0` in the MCP host env to disable console/network capture (it enables CDP `Runtime`, which some anti-bot scripts detect).

Recovery
- If a tab is gone or hung, list tabs, select another tab, or create a fresh named tab and retry.
- URL regex selectors are unanchored unless you add `^` or `$`.
"#;

pub fn print() {
    println!("{AGENT_INSTRUCTIONS}");
}
