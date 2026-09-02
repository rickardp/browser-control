#!/usr/bin/env node
// Playwright sidecar for browser-control's MCP server.
//
// Speaks NDJSON-over-stdio RPC. Holds one Playwright `Browser` connected
// over CDP for the duration of its run. Tabs are addressed by CDP
// `target_id` (the Rust side's source of truth); the sidecar maintains
// a `target_id -> Page` mapping internally, populated lazily via
// `BrowserContext.newCDPSession(page).send('Target.getTargetInfo')`.
//
// Protocol
// --------
// Request:  {"id": N, "method": "<name>", "params": {...}}
// Response: {"id": N, "result": <any>}
//           {"id": N, "error": {"message": "..."}}
//
// On unknown method: error with message "unknown method".
// On uncaught exception during handling: error with the exception
// message. The sidecar itself never crashes the JSON-RPC loop —
// errors only fail the current request.

import { chromium } from "playwright-core";
import readline from "node:readline";

// ---------------------------------------------------------------------------
// State.
// ---------------------------------------------------------------------------

let browser = null;
let context = null;
/** @type {Map<string, import('playwright-core').Page>} */
const pagesByTargetId = new Map();

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function ok(id, result) {
  send({ id, result });
}

function err(id, message) {
  send({ id, error: { message: String(message) } });
}

// Find a page by CDP target id. Probes pages we haven't seen yet by
// asking each unmapped page for its own target info via a transient
// CDP session. Caches once found.
async function getPage(targetId) {
  if (pagesByTargetId.has(targetId)) {
    return pagesByTargetId.get(targetId);
  }
  if (!context) throw new Error("not connected");
  for (const page of context.pages()) {
    if ([...pagesByTargetId.values()].includes(page)) continue;
    let session;
    try {
      session = await context.newCDPSession(page);
      const info = await session.send("Target.getTargetInfo");
      const id = info?.targetInfo?.targetId;
      if (id) pagesByTargetId.set(id, page);
      if (id === targetId) return page;
    } catch {
      // Page may have closed between iteration and lookup; skip.
    } finally {
      try {
        await session?.detach();
      } catch {
        /* ignore */
      }
    }
  }
  throw new Error(`page not found for target_id ${targetId}`);
}

// Drop a target_id from the cache (e.g. tab closed).
function forgetTarget(targetId) {
  pagesByTargetId.delete(targetId);
}

// ---------------------------------------------------------------------------
// Methods.
// ---------------------------------------------------------------------------

async function methodConnect(params) {
  const endpoint = params?.endpoint;
  if (!endpoint) throw new Error("missing 'endpoint'");
  const timeout = params?.timeout_ms ?? 5000;
  if (browser) {
    try {
      await browser.close();
    } catch {
      /* ignore */
    }
  }
  pagesByTargetId.clear();
  browser = await chromium.connectOverCDP(endpoint, { timeout });
  const contexts = browser.contexts();
  context = contexts.length > 0 ? contexts[0] : await browser.newContext();
  // Listen for new pages so future creations are caught early.
  context.on("page", async (page) => {
    try {
      const session = await context.newCDPSession(page);
      const info = await session.send("Target.getTargetInfo");
      const id = info?.targetInfo?.targetId;
      if (id) pagesByTargetId.set(id, page);
      await session.detach();
    } catch {
      /* ignore */
    }
  });
  return { ok: true, pages: context.pages().length };
}

async function methodDispose() {
  try {
    if (browser) await browser.close();
  } catch {
    /* ignore */
  }
  browser = null;
  context = null;
  pagesByTargetId.clear();
  return { ok: true };
}

async function methodSnapshot(params) {
  const page = await getPage(params.target_id);
  // `locator.ariaSnapshot()` returns a YAML-formatted accessibility tree
  // suitable for LLM consumption. Stable Playwright API since ~v1.45.
  const yaml = await page.locator("body").ariaSnapshot();
  return { snapshot: yaml };
}

async function methodClick(params) {
  const page = await getPage(params.target_id);
  const selector = params.selector;
  if (!selector) throw new Error("missing 'selector'");
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  await page.locator(selector).click(opts);
  return { ok: true };
}

async function methodType(params) {
  const page = await getPage(params.target_id);
  const selector = params.selector;
  const text = params.text;
  if (!selector) throw new Error("missing 'selector'");
  if (text === undefined) throw new Error("missing 'text'");
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  // Use `fill` for typical input fields; `pressSequentially` if simulating keystrokes.
  if (params.press_sequentially) {
    await page.locator(selector).pressSequentially(text, opts);
  } else {
    await page.locator(selector).fill(text, opts);
  }
  if (params.submit) {
    await page.locator(selector).press("Enter", opts);
  }
  return { ok: true };
}

async function methodHover(params) {
  const page = await getPage(params.target_id);
  const selector = params.selector;
  if (!selector) throw new Error("missing 'selector'");
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  await page.locator(selector).hover(opts);
  return { ok: true };
}

async function methodDrag(params) {
  const page = await getPage(params.target_id);
  const source = params.source_selector;
  const target = params.target_selector;
  if (!source || !target) throw new Error("missing 'source_selector' or 'target_selector'");
  await page.locator(source).dragTo(page.locator(target));
  return { ok: true };
}

async function methodPressKey(params) {
  const page = await getPage(params.target_id);
  const key = params.key;
  if (!key) throw new Error("missing 'key'");
  await page.keyboard.press(key);
  return { ok: true };
}

async function methodWaitFor(params) {
  const page = await getPage(params.target_id);
  const opts = {};
  if (params.timeout_ms !== undefined) opts.timeout = params.timeout_ms;
  if (params.selector) {
    const state = params.state || "visible";
    await page.locator(params.selector).waitFor({ state, ...opts });
  } else if (params.url_regex) {
    await page.waitForURL(new RegExp(params.url_regex), opts);
  } else if (params.load_state) {
    await page.waitForLoadState(params.load_state, opts);
  } else {
    throw new Error("must supply one of: selector, url_regex, load_state");
  }
  return { ok: true };
}

async function methodPdf(params) {
  const page = await getPage(params.target_id);
  const buf = await page.pdf();
  return { pdf_base64: buf.toString("base64") };
}

async function methodForgetTarget(params) {
  forgetTarget(params.target_id);
  return { ok: true };
}

const METHODS = {
  connect: methodConnect,
  dispose: methodDispose,
  snapshot: methodSnapshot,
  click: methodClick,
  type: methodType,
  hover: methodHover,
  drag: methodDrag,
  press_key: methodPressKey,
  wait_for: methodWaitFor,
  pdf: methodPdf,
  forget_target: methodForgetTarget,
};

// ---------------------------------------------------------------------------
// JSON-RPC loop.
// ---------------------------------------------------------------------------

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  if (!line.trim()) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch (e) {
    send({ id: null, error: { message: `parse error: ${e.message}` } });
    return;
  }
  const id = req.id ?? null;
  const method = req.method;
  const params = req.params || {};
  const fn = METHODS[method];
  if (!fn) {
    err(id, `unknown method: ${method}`);
    return;
  }
  try {
    const result = await fn(params);
    ok(id, result);
  } catch (e) {
    err(id, e?.message ?? String(e));
  }
});

rl.on("close", () => {
  // Best-effort cleanup if the host disconnects.
  if (browser) {
    browser.close().catch(() => {});
  }
  process.exit(0);
});
