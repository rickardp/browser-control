# Credential injection

browser-control as the boundary a secret crosses to reach a login form —
without ever existing in the agent that drives the login.

## The problem this solves

An agent orchestrating a login can do everything except supply the secret.
Every existing way of supplying it is broken for agentic use in one of two
directions:

- **It exposes the secret to the agent.** Reading it from a vault into the
  agent's context, an environment variable it can print, or a file it can
  `cat`. Anything the agent can read it can leak — into a transcript, a log,
  a tool result, a prompt-injection exfiltration.
- **It demands a body at the workstation.** Chromium's own password fill needs
  a real gesture the renderer never delivers. 1Password's Secure Agentic
  Autofill — architecturally exactly right — approves via Touch ID on the
  desktop app and supports only Browserbase. macOS Keychain prompts a dialog.
  None of these work when the operator is on a phone somewhere else.

Both directions were measured, not assumed; see the verification log.

The general shape of the fix is the one the industry converged on for
non-browser automation — secret *references* resolved at the moment of use —
applied at the one place that can enforce it for a browser: the process that
owns the CDP connection.

## The principle

**The agent handles references. browser-control handles values. The value
exists only between the resolver and the input field.**

```
agent                 browser-control                    vault
  │  type --ref e7         │                               │
  │  --secret op://v/i/p   │  resolve(op://v/i/p) ────────►│
  │───────────────────────►│◄──────────────── "hunter2"    │
  │                        │  Input.insertText("hunter2")  │
  │◄── "typed into e7" ────│  (value dropped)              │
```

Three properties fall out, and they are the whole point:

1. **The agent cannot obtain the value.** It has no resolver credentials —
   browser-control holds them — so `--secret` is the only route, and that
   route ends in a form field, not in a tool result.
2. **Nothing is stored.** browser-control keeps no secret store and no cache.
   Each use resolves afresh. There is nothing to decrypt, back up or leak.
3. **Every vault works.** browser-control runs a configured command and reads
   its stdout. `op read`, `bw get password`, `security find-generic-password`,
   `vault kv get -field=…`, `infisical secrets get --plain`, or anything
   else that prints a secret. No vendor code in the tool.

The human step moves from *runtime* to *provisioning*: the operator puts the
credential in a vault and scopes a service-account token to it, once. At run
time there is no human step at all — the login is effectively tier 1 in
`remote-auth.md` terms, even for a site the operator does not control.

## Design

### The flag

`--secret <reference>` on `type` (CLI) and `browser_type` (MCP), mutually
exclusive with `text`:

```sh
browser-control type -b brave/login --ref e7 --secret op://Automation/pineheights/password
browser-control type -b brave/login --ref e7 --secret keychain://browser-control/pineheights
browser-control type -b brave/login --ref e7 --secret env:PINEHEIGHTS_PASSWORD
```

The reference is opaque to browser-control apart from its scheme, which
selects a resolver.

### Resolvers

Configured, never built in:

```toml
# ~/.config/browser-control/config.toml
[secrets.resolvers]
op       = "op read {ref}"
keychain = "security find-generic-password -w -s {service} -a {account}"
bw       = "bw get password {ref}"
env      = "builtin"
```

A resolver is a command template. browser-control substitutes the reference
(or its parsed parts for schemes like `keychain://service/account`), runs it
with browser-control's own environment, and takes stdout with trailing
whitespace removed. Non-zero exit is an error naming the scheme and the exit
code — never the output.

`env:` is the one built-in, because it needs no process and is the natural
fallback for CI. It reads from browser-control's environment, which is not the
agent's.

### Where the resolver's own credentials live

The resolver must itself be tier 1 — no human step — or the workstation
problem returns one level down. Concretely:

- 1Password: a **service account** token (`OP_SERVICE_ACCOUNT_TOKEN`), scoped
  to one vault holding exactly the automation's credentials. Not the desktop
  app, whose approval is Touch ID.
- Keychain: the item's ACL must already trust the `security` binary for this
  user, or it prompts. Set it up once while present; test it with `-w` from
  a non-interactive shell before relying on it.
- Environment: set for the browser-control process, not exported to the agent.

These tokens are set in browser-control's environment or config, and the
agent's process must not inherit them. **This is the security boundary and it
is a deployment fact, not a code guarantee** — the spec should say so plainly
rather than pretend the tool can enforce it.

### Injection

Resolved values go through the existing native `type_text` path:
`Input.insertText` on CDP, the ref-typed script on BiDi. Same trusted-event
behaviour as `text`, which matters — it is what React-based login widgets
require. `--submit` composes as it does today.

The value is held in memory only for the duration of the call and is never
formatted into any error, trace, or log line. The `CommandTrace` for a
`--secret` call records the scheme and the reference, never the value.

### Refusals

- `--secret` and `text` together: error before any I/O.
- A scheme with no configured resolver: error naming the scheme and the
  config path.
- Resolver stdout empty: error. An empty password is almost never intended
  and is a common symptom of a mis-scoped token.
- Resolver stdout containing a newline after trimming: error, because the
  resolver has printed more than one secret or a warning banner, and typing
  it verbatim would be wrong.

## What this deliberately does not do

- **No per-use phone approval for secret release.** No mainstream secrets
  manager offers it today; 1Password's version is desktop-only. The model is
  provisioning-time approval plus audit logging, which is what every secrets
  manager's automation guidance already prescribes. If a phone-approval
  mechanism appears, it belongs in the resolver, not here.
- **No MFA handling.** A second factor is a *different* human step with a
  *different* location — the phone — and an agentic concern: recognise the
  prompt, reach the operator, relay the answer. That lives in agent recipes
  and a notification channel, not in browser-control. See `remote-auth.md`.
- **No site knowledge.** browser-control does not know what Okta or a Next.js
  login looks like. The agent finds the field; browser-control fills it.
- **No secret store.** Rejected in favour of references because storing
  credentials inside a public automation tool creates exactly the capability
  credential-stealing tooling is classified on, and because the vault already
  exists.

## Threat model, stated plainly

**Protects against:** the secret appearing in the agent's context, transcript,
tool results, or logs; the secret being written to disk by browser-control;
prompt injection instructing the agent to reveal a credential (it has none).

**Does not protect against:** an agent that has been given the resolver's
own token (deployment error); a compromised browser-control binary; a page
that exfiltrates a filled password (that is the site's problem and true of any
login). It also does not hide the *reference*: `op://Automation/site/password`
in a transcript tells a reader that such a credential exists, which is
acceptable and normal.

## Tests

| Level | Assertion |
|---|---|
| Unit | scheme parsing for `op://`, `keychain://a/b`, `env:X`, unknown scheme |
| Unit | template substitution, including `{service}`/`{account}` for two-part refs |
| Unit | trailing-newline trim; embedded-newline refusal; empty-output refusal |
| Unit | `--secret` + `text` refused before any process spawns |
| Unit | error messages never contain resolver stdout |
| Integration | `env:` resolver fills a real input; `CommandTrace` output has no value |
| Integration | a resolver that exits 1 produces an error naming the exit code, and the field is untouched |
| Integration | `--secret --submit` submits a form with the resolved value (assert server-side, not by reading the field) |

The last one is the important one: never test by reading the value back from
the field, or the test itself becomes a way to print the secret.

## Sequence

1. Reference parsing and resolver config, with unit tests. No browser.
2. `env:` built-in and `--secret` on the CLI `type` path.
3. MCP `browser_type` gets `secret`, routed natively.
4. Integration tests against a local form.
5. Docs: README, `agent_instructions.rs` (agents must learn to *prefer*
   `--secret` over ever asking for a password), CHANGELOG.

## Relationship to the rest

This is the third of three pieces that together make remote login general:

| Piece | Where | Status |
|---|---|---|
| Trusted native input, foreground control, persistent sessions | browser-control | done — `native-key-events.md` was the last gap |
| Credential injection by reference | browser-control | **this spec** |
| Finding the phone door, relaying MFA, reaching the operator | agent recipes + a notification channel | designed in `remote-auth.md`; channel not built |

## Verification log for the claims above

| Claim | Status | Evidence |
|---|---|---|
| Chromium fills a username but not a password for automation | verified | saved credential present; native click; submit failed on empty field |
| CDP keys cannot open the autofill dropdown | verified | `native-key-events.md` spike |
| 1Password Agentic Autofill approves on the desktop and needs Browserbase | verified by docs | 1password.dev/agentic-autofill |
| Secret references are the cross-vendor standard | verified | 1Password `op://`, Bitwarden, Infisical, Vault all document runtime resolution |
| macOS Keychain read blocks on a GUI prompt when the ACL is unset | verified | `security find-generic-password` timed out non-interactively |
