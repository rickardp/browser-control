---
status: Active
date: 2026-09-03
---

# ADR-004: Foreground emulation for background tabs

## Context

`browser-control` keeps automated browsers minimized and opens tabs in the
background so agents do not steal the user's screen. Chromium treats such
tabs as hidden: `document.visibilityState` is `hidden`, `document.hasFocus()`
is false, `requestAnimationFrame` never fires, and timers are throttled to
once per second. With the display locked the same applies to every tab. A
screenshot request forces the compositor to render a few frames, so captures
"work" but show a game that never advances, and games that check visibility
or focus switch to a paused background mode.

Measured on Brave with the display locked and the window minimized:

| Override | rAF/s | visibilityState | hasFocus |
|---|---|---|---|
| none | 0 | hidden | false |
| `Emulation.setFocusEmulationEnabled` | 60 | visible | true |
| `Target.activateTarget` / `Page.bringToFront` | 0 | hidden | true |
| `Page.setWebLifecycleState { active }` | 0 | hidden | false |
| `Emulation.setIdleOverride` | 0 | hidden | false |
| `--disable-backgrounding-occluded-windows` and related launch flags | 0 | hidden | false |

Only focus emulation changes the page's behaviour, and it is scoped to the
CDP session that enabled it: detaching the session reverts the tab (the page
receives `visibilitychange: hidden` and `blur`), while navigation within the
session keeps it. Firefox has no BiDi equivalent.

## Options Considered

1. **Launch flags at `start`.** Rejected: measured as ineffective on macOS
   with a locked display, and they require a browser restart.
2. **Per-call emulation inside each tool.** Rejected: the effect ends with the
   transient per-op session, so the game would freeze between calls.
3. **Emulation on the MCP server's capture-hub session.** Works for MCP, but
   the CLI has no long-lived session, so the CLI would need a different
   mechanism and the two surfaces would disagree about what is on.
4. **A detached holder process per tab (chosen).** One hidden subcommand,
   `browser-control tab foreground-hold <browser> <target> --timeout-s N`,
   attaches a CDP session, enables the emulation, records its PID and expiry
   in the registry, and blocks until told to stop. CLI and MCP both spawn and
   stop the same process, so they have solution parity and shared state.

## Decision

- MCP: `browser_tab_foreground { enabled = true, tab | target, timeout = "1h",
  all }`. CLI: `browser-control tab foreground <browser>/<tab> on|off
  [--timeout 1h]`. `browser_tab_list` / `tab list` report the flag.
- The emulation is `Emulation.setFocusEmulationEnabled` plus
  `Emulation.setIdleOverride { isUserActive, isScreenUnlocked }` on the
  holder's session.
- The holder is push-only: it waits on its CDP event stream (tab destroyed,
  session detached, connection closed), a termination signal, and one expiry
  deadline. Nothing polls or wakes on a timer. It is the documented exception
  to the daemonless rule in ADR-002: it exists only while a user or agent has
  asked for emulation, is bound to one tab, and has a hard expiry.
- Registry table `foreground_holders (browser_name, target_id, pid,
  started_at, expires_at)`; rows whose PID is dead are evicted on read, like
  `bidi_locks`. `off` sends SIGTERM (Unix) so the holder disables the
  emulation and detaches cleanly before deleting its row.
- Default timeout one hour; agents forget to turn things off. `enabled:
  false, all: true` / `tab foreground <browser> off` stops every holder on a
  browser.
- Default off, because an emulated tab runs at full speed in the background
  and pages that gate autoplay or notifications on focus consider the user
  present.
- Chromium-only. Firefox returns `EngineUnsupported` with a hint.

## Consequences

- Games, canvas apps, and visibility-gated pages behave as if in the
  foreground while the browser stays hidden or the display is locked;
  screenshots show live content, from the CLI and from MCP alike.
- The effect ends with `off`, the timeout, the tab, or the browser. It
  survives the MCP server and the CLI invocation that started it, which is
  the point, and also why the timeout and the stop-all command exist.
- One extra `browser-control` process per emulated tab, idle between events.
- Emulated tabs consume CPU as if visible.

## Related

- ADR-002 (no idle work; the holder is its documented exception).
- `docs/engine-parity.md` records the Chromium-only status.
