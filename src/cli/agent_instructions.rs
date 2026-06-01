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

Tabs
- Prefer tab primitives over target IDs:
  MCP: `browser_tab_list`, `browser_tab_new`, `browser_tab_select`, `browser_tab_close`.
  CLI: `browser-control tab open <browser>/<name> [url]`, `tab list <browser> --all`, `tab adopt <browser>/<name> <target-id>`.
- For repeatable work, create or select a named tab, then address it as `<browser>/<tab>` in page-context CLI commands.
- Use target IDs only to adopt an existing unnamed tab or as a last-resort diagnostic.
- Browser-wide operations do not take tab names. Page-context operations do.

Page and network work
- Navigate and inspect with MCP primitives first: `browser_navigate`, `browser_get_html`, `browser_take_screenshot`, `browser_select_element`.
- Fetch authenticated APIs with `browser_fetch` or `browser-control fetch`; this runs inside the browser context so cookies, Origin, CORS, and the browser TLS stack apply.
- Read/write storage with `browser_storage_get` / `browser_storage_set` or `browser-control storage`.
- Evaluate JavaScript with `browser-control eval` when no MCP primitive fits.
- Auth-sensitive reads reload HTTP(S) pages older than 10 minutes before evaluating so SSO can refresh tokens. CLI callers can override with `--max-age 1h`; MCP callers can pass `max_age`.

Cookies and login
- Wait for login with `browser_wait_for_cookie` or `browser-control wait-for-cookie --domain <regex> --name <regex>`.
- Add `--validate-url <url>` when the cookie alone is not enough and an authenticated endpoint must return 2xx.
- Export cookies with `browser_cookies` or `browser-control cookies`; use `--format netscape -o cookies.txt` for curl, wget, or yt-dlp.

MCP server setup
- Configure the host with command `browser-control` and args `["mcp"]`.
- Set `BROWSER_CONTROL` in the MCP host env to scope that server to one browser, or rely on `browser-control set default`.
- Playwright-sidecar tools (`browser_snapshot`, `browser_click`, `browser_type`, `browser_hover`, `browser_drag`, `browser_press_key`, `browser_wait_for`, `browser_pdf_save`) require Node tooling and CDP browsers. On Firefox, use the engine-agnostic primitives instead.

Recovery
- If a tab is gone or hung, list tabs, select another tab, or create a fresh named tab and retry.
- URL regex selectors are unanchored unless you add `^` or `$`.
"#;

pub fn print() {
    println!("{AGENT_INSTRUCTIONS}");
}
