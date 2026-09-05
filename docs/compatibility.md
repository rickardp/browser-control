# Child MCP Compatibility

Browser automation MCP servers fall into two categories: those that accept a remote CDP endpoint (and can be wrapped by the coordinator), and those that launch their own browser (and run as independent MCP entries alongside the coordinator).

## Works with `wrap` (connects to coordinator's CDP proxy)

These servers accept an external CDP endpoint. Use the `wrap` subcommand to inject the coordinator's proxy port.

### @playwright/mcp (Microsoft)

The most feature-complete browser automation MCP. Uses Playwright's accessibility-first approach.

| | |
|---|---|
| **Package** | [`@playwright/mcp`](https://www.npmjs.com/package/@playwright/mcp) |
| **Protocol** | CDP (Chromium), native adapters (Firefox, WebKit) |
| **CDP flag** | `--cdp-endpoint` |
| **Browsers** | Chromium, Firefox, WebKit, Edge (via `--browser`) |
| **Note** | `--cdp-endpoint` only works with Chromium-family browsers. Firefox and WebKit require Playwright to launch its own browser. |

```json
{
  "mcpServers": {
    "browser": {
      "command": "npx",
      "args": ["-y", "@anthropic-community/browser-coordinator-mcp"]
    },
    "playwright": {
      "command": "npx",
      "args": [
        "-y", "@anthropic-community/browser-coordinator-mcp",
        "wrap", "--",
        "npx", "-y", "@playwright/mcp@latest",
        "--cdp-endpoint={cdp_endpoint}"
      ]
    }
  }
}
```

### chrome-devtools-mcp (Google)

Google's official Chrome DevTools MCP. Exposes performance tracing, network inspection, and emulation tools that Playwright MCP doesn't have.

| | |
|---|---|
| **Package** | [`chrome-devtools-mcp`](https://www.npmjs.com/package/chrome-devtools-mcp) |
| **Protocol** | CDP (via Puppeteer) |
| **CDP flag** | `--browser-url` (HTTP) or `--ws-endpoint` (WebSocket) |
| **Browsers** | Chrome only (stable, canary, beta, dev) |

```json
{
  "mcpServers": {
    "browser": {
      "command": "npx",
      "args": ["-y", "@anthropic-community/browser-coordinator-mcp"]
    },
    "chrome-devtools": {
      "command": "npx",
      "args": [
        "-y", "@anthropic-community/browser-coordinator-mcp",
        "wrap", "--",
        "npx", "-y", "chrome-devtools-mcp@latest",
        "--browser-url={cdp_endpoint}"
      ]
    }
  }
}
```

### Using both together

The coordinator, Playwright MCP, and Chrome DevTools MCP can all run simultaneously. Each child MCP connects to the same browser through the coordinator's stable CDP proxy.

```json
{
  "mcpServers": {
    "browser": {
      "command": "npx",
      "args": ["-y", "@anthropic-community/browser-coordinator-mcp"]
    },
    "playwright": {
      "command": "npx",
      "args": [
        "-y", "@anthropic-community/browser-coordinator-mcp",
        "wrap", "--",
        "npx", "-y", "@playwright/mcp@latest",
        "--cdp-endpoint={cdp_endpoint}"
      ]
    },
    "chrome-devtools": {
      "command": "npx",
      "args": [
        "-y", "@anthropic-community/browser-coordinator-mcp",
        "wrap", "--",
        "npx", "-y", "chrome-devtools-mcp@latest",
        "--browser-url={cdp_endpoint}"
      ]
    }
  }
}
```

## Runs independently (launches own browser)

These servers don't accept a remote CDP endpoint. They manage their own browser lifecycle and run as separate MCP entries — no `wrap` needed.

### @angiejones/mcp-selenium

Community Selenium MCP by Angie Jones. Uses WebDriver Classic.

| | |
|---|---|
| **Package** | [`@angiejones/mcp-selenium`](https://www.npmjs.com/package/@angiejones/mcp-selenium) |
| **Protocol** | WebDriver Classic |
| **Remote connect** | No |
| **Browsers** | Chrome, Firefox, Edge |

```json
{
  "mcpServers": {
    "selenium": {
      "command": "npx",
      "args": ["-y", "@angiejones/mcp-selenium"]
    }
  }
}
```

### @wdio/mcp (WebdriverIO)

Official WebdriverIO MCP. Uses WebDriver BiDi with Classic fallback. Supports mobile testing via Appium.

| | |
|---|---|
| **Package** | [`@wdio/mcp`](https://www.npmjs.com/package/@wdio/mcp) |
| **Protocol** | WebDriver BiDi / Classic |
| **Remote connect** | No (desktop); yes for mobile via `APPIUM_URL` |
| **Browsers** | Chrome (desktop), iOS/Android (via Appium) |

```json
{
  "mcpServers": {
    "wdio": {
      "command": "npx",
      "args": ["-y", "@wdio/mcp"]
    }
  }
}
```

### @padenot/firefox-devtools-mcp (Mozilla)

Mozilla-endorsed Firefox DevTools MCP. The only MCP server with native Firefox support via WebDriver BiDi.

| | |
|---|---|
| **Package** | [`@padenot/firefox-devtools-mcp`](https://www.npmjs.com/package/@padenot/firefox-devtools-mcp) |
| **Protocol** | WebDriver BiDi + Marionette fallback |
| **Remote connect** | No |
| **Browsers** | Firefox only |

```json
{
  "mcpServers": {
    "firefox": {
      "command": "npx",
      "args": ["-y", "@padenot/firefox-devtools-mcp", "--headless"]
    }
  }
}
```

### @modelcontextprotocol/server-puppeteer

The original reference Puppeteer MCP server. Archived as of May 2025, but still functional.

| | |
|---|---|
| **Package** | [`@modelcontextprotocol/server-puppeteer`](https://www.npmjs.com/package/@modelcontextprotocol/server-puppeteer) |
| **Protocol** | CDP (via Puppeteer) |
| **Remote connect** | No |
| **Browsers** | Chrome / Chromium |
| **Status** | Archived — prefer `@playwright/mcp` or `chrome-devtools-mcp` |

```json
{
  "mcpServers": {
    "puppeteer": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-puppeteer"]
    }
  }
}
```

## Cloud-hosted browsers

These MCP servers connect to cloud-managed browser instances. They don't use local browsers at all.

| Server | Package | Provider |
|--------|---------|----------|
| **Browserbase MCP** | [`@browserbasehq/mcp`](https://www.npmjs.com/package/@browserbasehq/mcp) | Browserbase |
| **Hyperbrowser MCP** | [`hyperbrowser-mcp`](https://www.npmjs.com/package/hyperbrowser-mcp) | Hyperbrowser |
| **Cloudflare Playwright MCP** | [`@cloudflare/playwright-mcp`](https://www.npmjs.com/package/@cloudflare/playwright-mcp) | Cloudflare Workers |

These run their own infrastructure and don't interact with the coordinator.

## Browser protocol support

The coordinator's CDP reverse proxy is inherently Chromium-only. Here's why, and what the alternatives are for other browsers.

### Why only Chromium?

CDP (Chrome DevTools Protocol) is the only browser remote debugging protocol with broad MCP server support. The coordinator's proxy forwards raw TCP/WebSocket CDP connections — it doesn't understand or translate the protocol, just pipes bytes.

Firefox and Safari use different protocols that are incompatible with CDP:

| Browser | Protocol | Coordinator support | Child MCP support |
|---------|----------|--------------------|--------------------|
| Chrome, Edge, Chromium, Brave | CDP | Direct (CdpSession) | `@playwright/mcp`, `chrome-devtools-mcp` (via `wrap`) |
| Firefox | WebDriver BiDi | Direct (BidiSession, `--browser firefox`) | `@padenot/firefox-devtools-mcp` (standalone only) |
| Safari | SafariDriver (WebDriver) | Not supported | None mature — `lxman/safari-mcp-server` is experimental |
| WebKit (Playwright's) | Playwright internal protocol | Not supported | `@playwright/mcp --browser webkit` (standalone only) |

### Firefox

Firefox doesn't speak CDP but supports WebDriver BiDi natively (Firefox 129+). The coordinator now supports Firefox directly via `--browser firefox` or `coordinator_launch_browser({ browserType: "firefox" })`. When Firefox is active, the coordinator's own tools work via BiDi, but child MCPs (Playwright MCP, Chrome DevTools MCP) that require CDP will not work through the proxy.

For Firefox-specific DevTools features, `@padenot/firefox-devtools-mcp` can run as an independent MCP entry alongside the coordinator.

Playwright's `--browser firefox` launches a patched Firefox binary with Playwright's own protocol — it cannot connect to an existing Firefox instance.

### Safari / WebKit

Safari has no externally-accessible remote debugging protocol. Safari's Web Inspector Protocol is private and not designed for automation.

Playwright's `--browser webkit` launches a custom WebKit build, not real Safari. It's useful for testing WebKit rendering but not for controlling Safari.

For Safari testing, the community `lxman/safari-mcp-server` uses SafariDriver but is early-stage (macOS only, single session).

### WebDriver BiDi

[WebDriver BiDi](https://w3c.github.io/webdriver-bidi/) is a W3C standard aiming to be the cross-browser successor to CDP. Chrome, Firefox, and Safari are all implementing it.

`browser-control` uses BiDi for Firefox across the whole native tool surface: navigation, evaluation, screenshots, accessibility snapshots with refs, ref-based input, and console/network capture. The Playwright sidecar (CSS-selector interaction, `browser_wait_for`, `browser_pdf_save`) and `browser_network_body` remain Chromium-only; `browser_press_key` is native on both engines. Every behavioural difference between the two engines is listed in [engine-parity.md](engine-parity.md).
