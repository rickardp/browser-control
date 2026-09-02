---
status: Active
date: 2026-09-02
---

# ADR-003: Native CDP observation and ref-based interaction

## Context

Public feedback on Claude in Chrome and on the browser-MCP ecosystem in
general converges on a short list of capabilities that make a browser tool
useful to a coding agent: reading console output and network requests while
reproducing a bug, addressing elements through an accessibility snapshot
with stable refs instead of screenshots or hand-written CSS selectors, cheap
text extraction, and screenshots that do not silently balloon the context.

`browser-control` had none of the observe primitives and its interaction
tools were CSS-selector only, routed through a Playwright sidecar that
requires Node or bun on the machine. `browser_snapshot` was Playwright's
`ariaSnapshot()`, which carries no refs, so an agent could read the tree but
not act on what it read without inventing a selector.

Three constraints from ADR-001/ADR-002 shape any solution:

- **No idle work.** Nothing may poll or wake on a timer while the user is
  idle.
- **Engine-agnostic by dispatch.** Chromium (CDP) and Firefox (BiDi) share
  one agent contract; engine-specific protocol lives behind `TabBackend`.
- **Always proceed.** Tab failures are recovered once before they surface.

## Options Considered

### A. Extend the Playwright sidecar

Bump `playwright-core` to a version with `_snapshotForAI` (refs), use
`page.on('console')`, `page.on('request')`, `setInputFiles`, `page.mouse`.

- Pro: fastest to build; Playwright handles auto-wait, occlusion, frames.
- Con: every new capability inherits the Node/bun dependency, which is the
  single most common setup failure and rules out slim CI images.
- Con: console/network buffering would live in a child process with its own
  lifecycle, doubling the state the server has to reconcile after a crash.

### B. Native Rust over CDP (chosen)

Use `Accessibility.getFullAXTree` for the snapshot, `backendDOMNodeId` as
the ref key, `DOM.getContentQuads` + `Input.dispatchMouseEvent` /
`Input.insertText` for interaction, and a long-lived flat session per
touched tab with `Runtime` / `Log` / `Network` / `Page` enabled for capture.

- Pro: no new runtime dependency; works headless and in CI; one process.
- Pro: the AX tree is already composed across shadow roots and carries the
  same names and roles Playwright reports.
- Con: no auto-wait or occlusion check in v1; iframes not traversed.
- Con: `Runtime.enable` is detectable by some anti-bot scripts.

### C. Mixed

Rust for capture, sidecar for refs and input. Rejected: it keeps the Node
dependency on the path agents use most (clicking what they just read).

## Decision

1. **Native CDP for observe and interact.** New code lives in `src/a11y`
   (pure tree parsing, rendering, `find`), `src/session/input.rs` (input
   dispatch), `src/session/cdp_session.rs` (shared attach/detach helper),
   and `src/session/capture.rs` (capture hub). The Playwright sidecar stays
   for CSS-selector interaction, `browser_press_key`, `browser_wait_for`,
   and `browser_pdf_save`.
2. **Refs are `backendDOMNodeId`s bound to a document token.** A ref table
   per tab is keyed by the Document node's `backendDOMNodeId`; every
   ref-based action re-reads that token with `DOM.getDocument {depth: 0}`
   and reports a typed `StaleRef` when it changed. Refs stay stable across
   snapshots of the same document.
3. **Passive capture is the sanctioned exception to "no idle work".** The
   hub keeps CDP domains enabled on tabs the server has touched and buffers
   browser-pushed events in bounded ring buffers. It never polls and never
   wakes on a timer; its one background task blocks on the event channel.
   Buffers persist across navigations until `clear`, tab close, or browser
   switch, and every entry carries the page URL it came from.
4. **Extend existing tools rather than add parallel ones.** `browser_click`
   / `browser_type` / `browser_hover` / `browser_drag` accept `ref` or a
   CSS selector; `browser_snapshot` becomes native. New tools exist only
   where nothing fit: `browser_find`, `browser_get_page_text`,
   `browser_console_messages`, `browser_network_requests`,
   `browser_network_body`.
5. **Chromium first, BiDi behind the seam.** Every new `TabBackend` method
   has a BiDi arm returning `EngineUnsupported` with a hint naming the
   engine-agnostic alternative. `browser_get_page_text` is engine-agnostic.
6. **Screenshot defaults are unchanged.** `browser_take_screenshot` keeps its
   viewport PNG; `format`, `quality`, `max_width`, `save_to`, and `ref` are
   opt-in so agents can keep pixels small or out of context entirely.
7. **Opt-out for capture.** `BROWSER_CONTROL_CAPTURE=0` disables attachment
   for users who log into anti-bot-protected sites through the managed
   browser.

## Consequences

**Wins**

- Agents can read console and network state without Node, headless or not.
- Snapshot plus ref interaction is deterministic and far cheaper in tokens
  than screenshot-and-coordinate loops.
- The sidecar becomes optional for the common read-then-act flow.

**Losses**

- Touched tabs show `attached: true` in `Target.getTargets`, and Chrome's
  automation infobar may stay visible on them.
- `Network.enable` makes the browser buffer response bodies per touched tab
  (capped at 32 MiB by the enable parameters).
- Ref-based clicks do not check occlusion or wait for animations; HTML5
  native drag-and-drop and cross-origin iframes are out of scope in v1.

**Non-impact**

- CLI behaviour, the `BROWSER_CONTROL` contract, and every existing tool's
  default output are unchanged.

## Related

- ADR-001 (Rust CLI rewrite), ADR-002 (CLI lifecycle, no idle work).
- `docs/specs/boundaries.md` records the passive-listener rule.

### Follow-ups

- BiDi arms for capture (`session.subscribe` to `log.entryAdded`,
  `network.*`) and for refs (`browsingContext.locateNodes`,
  `input.performActions`). **Status:** deferred.
- Occlusion check before ref clicks (`DOM.getNodeForLocation`). **Status:**
  deferred.
- OOPIF and worker capture via `Target.setAutoAttach` on the hub session.
  **Status:** deferred.
- `browser_batch`, file upload, back/forward navigation. **Status:** not
  planned in this ADR; see the comparison notes that motivated it.
