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
3. **Emulation on the capture hub's long-lived session (chosen).** The hub
   already keeps one CDP session per touched tab for the server's lifetime.
   A per-tab flag rides that session and is re-applied on re-attach.

## Decision

- `browser_tab_foreground { enabled, tab | target }` toggles emulation per
  tab; `browser_tab_new` and `browser_tab_select` accept `foreground: true`;
  `browser_tab_list` reports the flag.
- The emulation is `Emulation.setFocusEmulationEnabled` plus
  `Emulation.setIdleOverride { isUserActive, isScreenUnlocked }` on the hub
  session. When event capture is disabled (`BROWSER_CONTROL_CAPTURE=0`) the
  hub still attaches a session for emulation only.
- Server-wide default: `browser-control set foreground always` or
  `BROWSER_CONTROL_FOREGROUND=1` emulates on every touched CDP tab. Default
  off, because an emulated tab runs at full speed in the background and
  pages that gate autoplay or notifications on focus consider the user
  present.
- Chromium-only. Firefox returns `EngineUnsupported` with a hint.
- No idle work is introduced: the flag is state on an existing push-only
  session; nothing polls or wakes on a timer.

## Consequences

- Games, canvas apps, and visibility-gated pages behave as if in the
  foreground while the browser stays hidden or the display is locked;
  screenshots show live content.
- The effect ends with the MCP server, a browser switch, or the tab; CLI
  commands cannot hold it because they have no long-lived session.
- Emulated tabs consume CPU as if visible.

## Related

- ADR-002 (no idle work), ADR-003 (capture hub session).
- `docs/engine-parity.md` records the Chromium-only status.
