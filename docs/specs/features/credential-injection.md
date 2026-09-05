# Credential injection

`browser-control type --stdin` reads a value from a pipe and puts it in the
focused element. That is the whole feature.

**Status: implemented.** Verified end to end against the macOS Keychain.

## The principle

A secret has to reach a login form without passing through the agent driving
the login. The Unix answer is a pipe, and the Unix division of labour is that
**browser-control has no opinion about where the secret comes from**:

```sh
op read op://Automation/site/password | browser-control type --stdin --submit
bw get password site                  | browser-control type --stdin --submit
vault kv get -field=password kv/site  | browser-control type --stdin --submit
security find-internet-password -s site -w | browser-control type --stdin --submit
printf '%s' "$SITE_PW"                | browser-control type --stdin --submit
```

Everything to the left of the pipe is the operator's business. A managed
vault, the OS keychain, a `.env` file, an environment variable, `echo` in a
throwaway shell — all equally valid, all outside this tool. Anything that
prints a secret on stdout is a resolver, and browser-control never learns
which one was used.

**The security boundary sits at the pipe, not inside browser-control.** That
is deliberate. A tool that stored credentials, or resolved references, or
knew about vault vendors, would be taking responsibility for a decision the
operator is better placed to make — and would acquire, in a public
automation tool, a capability indistinguishable from credential-stealing
software. It does none of those things. It reads stdin.

## What browser-control guarantees

Only what a program at the end of a pipe can:

- The value goes to the focused element and nowhere else.
- It is never printed, logged, traced, or returned. The command reports a
  character count.
- Errors never include stdin.
- Nothing is written to disk, cached, or kept after the process exits.

## What it does not guarantee, and cannot

On a single-user workstation the agent and the resolver run as the same user.
Whatever the resolver can read, an agent that wants to can read too. A pipe
does not change that, and neither would any design inside this tool — a
resolver built into browser-control would need the same credentials, readable
by the same user.

A real guarantee needs a boundary that does not exist here: a separate
principal (a daemon holding credentials the agent's user cannot read), or a
human approving each release. Both belong outside browser-control. If one
appears — 1Password's agentic autofill is the closest, though it currently
approves via desktop biometrics and supports only Browserbase — it plugs in
on the left of the pipe with no change here.

So the bar met is the one CI systems meet: the value is not in the agent's
context, its use is auditable at the vault, and a scoped token bounds the
blast radius.

## Interface

```
browser-control type [--text <s> | --stdin] [--submit] [--press-sequentially]
                     [-b <browser>[/<tab>]] [--target <regex>]
```

Targets the **focused** element, not a ref. Refs live in the MCP server's
state; a separate CLI process cannot see them. Focus the field first with a
ref-based `browser_click` over MCP, or by tabbing to it.

### Refusals

Each is a real vault-CLI failure mode that would otherwise be typed verbatim
into a password field. All fire before any browser I/O.

| Input | Why it is refused |
|---|---|
| `--text` with `--stdin` | ambiguous |
| neither | nothing to type |
| empty stdin | the resolver produced nothing — usually a mis-scoped token |
| multi-line stdin | a resolver should print one secret; extra lines mean a banner or warning on stdout |

One trailing newline is stripped, since every CLI emits one. Spaces and
symbols inside the value are preserved.

## Implementation

| File | What it does |
|---|---|
| `src/session/input.rs` | `type_focused` — select existing content via `document.activeElement`, then `Input.insertText`, then optional Enter |
| `src/session/input_bidi.rs` | `type_focused` — key actions, which is what typing into focus means on BiDi. No select-all: without a node handle there is nothing to select, so clear the field first if replacement is wanted |
| `src/session/backend.rs` | `type_into_focused` — engine dispatch, no node id |
| `src/cli/type_cmd.rs` | the command, stdin handling, refusals |
| `src/main.rs`, `src/cli/mod.rs` | wiring |
| `src/cli/agent_instructions.rs` | agents must never ask for or handle a password; pipe it. One-time codes are different — single-use, safe to relay |

## Explicitly out of scope

- **Vault integrations.** The shell does resolution.
- **Reference schemes** (`op://…` parsing, resolver config). The shell does that too.
- **Reading the browser's own password store.** Considered and dropped: it
  would add a decryption capability to a public automation tool, and every
  external vault already works through the pipe.
- **MFA.** A second factor is a different human step in a different place —
  the operator's phone — and an agentic concern: recognise the prompt, ask,
  relay. See `remote-auth.md` in the games-bronze repo.
- **Site knowledge.** The agent finds the field; browser-control fills it.

## Verification

Performed against a live browser, 2026-09-05.

| Check | Result |
|---|---|
| `--text` + `--stdin`, neither, empty, multi-line | all refused before browser I/O |
| Piped value reaches a focused password field | 14 chars in, `value.length == 14` |
| `input` event fires | yes — trusted-event semantics frameworks require |
| End to end from a real vault (`security … -w \| type --stdin`) | exact secret landed; asserted as a boolean so the value was never printed |
| `--submit` | form submitted carrying all 11 characters |
| Value absent from stdout | command reports only "typed N characters" |

Tests assert by submission or by length, never by reading the value back —
otherwise the test itself becomes a way to print the secret.
