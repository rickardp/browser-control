# Native key events

Move `press_key` off the Playwright sidecar and onto the transport each engine
already has: CDP `Input.dispatchKeyEvent` on Chromium, `input.performActions`
on Firefox.

## Why

`press_key` is the last common interaction that still needs Node. `click`,
`type`, `hover` and `drag` all went native for refs; keyboard input did not,
so a keystroke drags in `bun`/`node`, a sidecar process and a second CDP
attachment.

That attachment is not merely heavy, it is **unreliable**: in extended agent
sessions it fails with `overCDP: Timeout 5000ms exceeded` while browser-control's
own native CDP connection to the same tab is healthy. When it fails there is no
fallback — the agent is left synthesising DOM events from JavaScript, which
React and other frameworks ignore because `isTrusted` is false. Real keyboard
input is exactly where that substitution breaks down.

Keyboard access matters beyond convenience: `Tab`, `Escape`, arrow keys and
`Control+A` are how you drive comboboxes, dismiss modals, move through
listboxes and clear a field — none of which have a click target.

## Scope

**In:** `press_key` over CDP and BiDi; a shared key table; chord parsing
(`Control+Shift+K`); CLI `browser-control key`; MCP `browser_press_key` routed
natively; `press_enter` refactored onto the new path.

**Out:** `wait_for` and `pdf_save` (still sidecar; separate work). Key
*sequences* in one call — a caller can issue several. IME and composition
events. Holding a key across calls: each call is balanced, with modifiers
released in reverse order.

## Design

### 1. Key table — `src/session/keys.rs` (new)

One definition per key, consumed by both engines.

```rust
pub struct KeyDef {
    pub key: &'static str,        // KeyboardEvent.key      "ArrowDown"
    pub code: &'static str,       // KeyboardEvent.code     "ArrowDown"
    pub vk: u32,                  // windowsVirtualKeyCode  0x28
    pub text: Option<&'static str>, // insertion text, None for non-printable
    pub bidi: &'static str,       // WebDriver key value    "\u{E015}"
}
```

Coverage: `Enter Tab Escape Backspace Delete Insert Space Home End PageUp
PageDown Arrow{Up,Down,Left,Right} F1..F12`, the modifiers, and printable
ASCII. Printable keys are derived rather than tabulated: `'a'` yields
`key:"a" code:"KeyA" vk:0x41 text:"a"`, `'A'` the same with an implied Shift.

Named keys carry `text: None`. **This is load-bearing.** Chromium synthesises a
`keypress` and inserts characters from the `text` field, so sending
`text:"\u{E015}"` for `ArrowDown` types a private-use glyph into the field
instead of moving the caret. `press_enter` already relies on the inverse —
`text:"\r"` is what makes forms submit — so the table must express both.

BiDi uses the WebDriver normalised key values (`\u{E007}` Enter, `\u{E015}`
ArrowDown, `\u{E008}` Shift …), which is a different namespace from
`KeyboardEvent.key`; hence the separate `bidi` field rather than a conversion.

### 2. Chord parsing — `parse_chord(&str) -> Result<Chord>`

`"Control+Shift+K"` → modifiers `[Control, Shift]`, key `K`. Rules:

- Split on `+`; the final segment is the key, the rest are modifiers.
- A trailing `+` means the `+` key (`"Control++"`).
- Modifier aliases: `Ctrl`/`Control`, `Cmd`/`Meta`/`Super`, `Option`/`Alt`.
- Unknown names are rejected **with the closest match named**, since a
  silently-ignored keystroke is worse than an error.
- Case: named keys match case-insensitively (`escape` == `Escape`); a
  single character is taken literally, so `"a"` and `"A"` differ.

CDP modifier bitmask: Alt 1, Control 2, Meta 4, Shift 8. The mask goes on
**every** event in the chord, including the target key's — omitting it there is
the classic reason `Control+A` selects nothing.

### 3. CDP — `src/session/input.rs`

```rust
pub async fn press_key(c: &CdpClient, sid: &str, chord: &Chord) -> Result<()>
```

Order: modifier `keyDown`s (in declaration order) → target `keyDown` → target
`keyUp` → modifier `keyUp`s (reverse). Reverse release matters: a
still-pressed Shift leaks into whatever the page does next.

Event type is `keyDown` when the key has `text`, `rawKeyDown` when it does
not — Chromium's own convention, and what Puppeteer emits.

Modifiers are released in a cleanup path that runs even when the target
dispatch fails, so an error cannot strand the browser with Control held.

`press_enter` becomes `press_key(c, sid, &Chord::plain(ENTER))`; its current
behaviour, including `text:"\r"`, must survive unchanged — it is covered by
existing tests.

### 4. BiDi — `src/session/input_bidi.rs`

```rust
pub async fn press_key(c: &BidiClient, ctx: &str, chord: &Chord) -> Result<()>
```

Reuses `key_source` and extends `key_press` to interleave modifiers. BiDi
tracks modifier state itself from the keyDown/keyUp pairs, so no bitmask.
`input.releaseActions` runs on the error path, mirroring the CDP cleanup.

### 5. Backend — `src/session/backend.rs`

```rust
pub async fn press_key_on_tab(&self, target_id: &str, chord: &Chord,
                              timeout: Duration) -> Result<()>
```

Same `Cdp`/`Bidi` match as `click_node`, wrapped in `with_page_session` and
`bidi_bounded` respectively. Keyboard input goes to whatever has focus, so
unlike the other native actions it takes no `backend_node_id`.

### 6. MCP and CLI

- Add `NativeAction::PressKey`; `make_press_key` drops `method: "press_key"`
  and its sidecar params, gaining `native: Some(NativeAction::PressKey)`.
  `ref_pairs` stays empty: there is no element to address.
- New `browser-control key <chord> [--browser b/tab]`, alongside `eval` and
  `storage`. `press-key` accepted as an alias.
Five places currently document `press_key` as sidecar-only. Missing one leaves
agents believing keyboard input needs Node, so they will not attempt it:

| File | What it says now |
|---|---|
| `src/cli/agent_instructions.rs:50` | "`browser_press_key` … route through a Playwright sidecar that needs `bun` or `node`" |
| `docs/engine-parity.md:23` | table row: Chromium "yes (sidecar)", Firefox "no" |
| `docs/engine-parity.md:119` | prose repeating the sidecar list |
| `docs/compatibility.md:247` | "The Playwright sidecar (… `browser_press_key` …) … remain Chromium-only" |
| `README.md:59`, `README.md:207` | sidecar feature list |

**This makes `press_key` work on Firefox for the first time** — today it is
Chromium-only because the sidecar is. That is a parity gain, not just a
dependency removal, and the engine-parity table should move it to "yes / yes".

## Testing

Existing fake-CDP and fake-BiDi harnesses cover this shape already —
`tools.rs` asserts on recorded `Input.dispatchKeyEvent` payloads, and
`bidi/mod.rs` on recorded `input.performActions`.

| Level | Assertion |
|---|---|
| Unit | `parse_chord` on `Control+A`, `Shift+Tab`, `Control++`, `a`, `A`, bad input |
| Unit | modifier bitmask arithmetic; reverse release order |
| Unit | named keys carry no `text`; printable keys do |
| CDP | recorded event sequence for `Control+A`: 4 events, mask 2 on both middle events |
| CDP | `press_enter` payload is byte-identical to today's |
| BiDi | `performActions` payload nests modifiers correctly around the key |
| Failure | a failing target dispatch still emits modifier `keyUp`s |
| Integration | type into a field, `Control+A`, `Delete`, assert the field is empty |
| Integration | `Tab` moves focus between two inputs |

The integration pair runs on both engines, since they share no dispatch code.

## Risks

**The motivating case is not solved by this — spiked and confirmed.**

Measured against `gamesglobal.okta.com`, which has a saved credential in the
browser-control profile:

| | |
|---|---|
| Username field | **autofilled by Chromium on its own**, before any input |
| Password field | stayed empty |
| `ArrowDown` ×2 on the focused password field | no dropdown, no fill |
| Submitting afterwards | "we found some errors" — the field really was empty |

So Chromium's on-load autofill reaches a username but not a password (it
requires a user gesture the renderer never sees), and the popup does not
respond to CDP-dispatched keys — it is browser-process UI, while `Input.*`
events are delivered to the renderer.

**This does not diminish the feature**, which was verified working in the same
session: `Control+A` selected `[0,11]` in a real input, `Delete` cleared it,
printable keys inserted, `Tab` moved focus, and every event arrived with
`isTrusted: true` — which is precisely what synthetic JS events cannot do and
why frameworks ignored the old fallback.

It does mean **keyboard automation is not a route to credential autofill**, and
the docs should not imply it is.

**Key tables rot.** Scope is deliberately US-layout ASCII plus named keys.
Anything beyond that should go through `type` and `Input.insertText`, which is
layout-independent. Say so in the docs rather than growing the table.

**Sidecar parity.** Chords the sidecar accepted but the table rejects would be
a silent regression for existing callers. Grep the repo and README for
`press_key` examples and cover each in the parse tests.

## Sequence

1. Spike the autofill dropdown; record the finding either way.
2. `keys.rs` + `parse_chord` with unit tests. No I/O.
3. CDP `press_key`; refactor `press_enter`; fake-CDP tests.
4. BiDi `press_key`; fake-BiDi tests.
5. `press_key_on_tab`; route MCP natively; CLI command.
6. Integration tests on both engines.
7. Docs: `agent_instructions.rs`, `engine-parity.md`, README, CHANGELOG.

Steps 2–4 are independently reviewable. Step 5 is the only one that changes
observable behaviour, so it is the natural place to stop and check nothing
regressed.
