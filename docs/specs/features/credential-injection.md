# Credential injection

Getting a password into a login form from an agentic workflow, without the
value appearing in the agent's context — using tools that already exist.

## What is actually needed

An agent orchestrating a login can do everything except supply the secret.
The two obvious ways to supply it are both wrong for agentic use:

- **Resolving it in the agent** — into a prompt, an env var it can print, a
  file it can read. Anything the agent holds it can leak.
- **The workstation's own fill mechanisms** — Chromium's password autofill,
  1Password's agentic autofill, Keychain dialogs. All measured to need a
  body at the keyboard (see the verification log). Useless when the operator
  is remote.

The industry answer for non-browser automation is well established: the
automation holds a *reference*, a vault CLI with a scoped service-account
token resolves it at the moment of use. `op read op://vault/item/field`,
`bw get password`, `vault kv get -field=`, `infisical secrets get --plain`.
These exist, are maintained, and are what every secrets manager's own
guidance prescribes.

What is missing is only the last inch: a way for that resolved value to reach
a browser input **without passing through the agent**. That is a pipe.

## The design: `type` reads stdin

```sh
op read op://Automation/site/password | browser-control type -b brave/login --ref e7 --stdin
```

- `type` becomes a CLI command (today it exists only as the MCP tool
  `browser_type`), taking `--ref` and either `--text` or `--stdin`.
- With `--stdin`, the text is read from standard input, trailing newline
  stripped, and injected through the existing native `type_text` path —
  `Input.insertText` on CDP, the trusted-event script on BiDi. Same path
  `text` uses, so React-based login widgets accept it.
- `--submit` composes as it does for `text`.

That is the whole feature. browser-control gains stdin support on `type`; it
gains no knowledge of vaults, references, schemes or credentials.

### What the agent sees

The command it wrote, and `typed into e7`. The value is in a pipe between two
processes the agent spawned and never observes. The transcript records
`op://Automation/site/password`, which discloses that such a credential
exists — acceptable, and the same as any secrets-manager audit log.

### Refusals

- `--text` and `--stdin` together: error before any I/O.
- `--stdin` with nothing on stdin, or only whitespace: error. An empty
  password is almost never intended and is the usual symptom of a mis-scoped
  vault token.
- Stdin containing a newline after trimming: error. The resolver printed more
  than one line — a banner, a warning — and typing it verbatim would be wrong.
- Errors never echo stdin.

## The security model, stated honestly

**What this achieves:** the value does not appear in the agent's context,
transcript, tool results, or logs; browser-control stores nothing; the
reference is auditable; the human step moves from runtime to provisioning
(scope a service-account token to a vault, once), so at runtime there is
none.

**What this does not achieve, and nothing on a single-user workstation can:**
a guarantee that the agent *cannot* read the secret. The agent runs as the
same user as `op` and browser-control. Whatever token `op` uses, the agent
can print. This is true of any design where resolver and agent share a
principal, including one that puts the resolver inside browser-control — so
that design was rejected as extra machinery for the same property.

A real guarantee needs one of two things, and both are out of scope here:

- **A separate principal.** A resolver daemon holding credentials the agent's
  user cannot read. Real, and how production systems do it; not a workstation
  tool.
- **A human step at resolution.** 1Password Secure Agentic Autofill does
  exactly this — the agent requests, 1Password injects, a human approves —
  and it is the right product to adopt **when** its approval reaches a phone
  and it supports a local CDP browser. Today it approves via desktop
  biometrics and supports only Browserbase, so its human step lands on the
  workstation. Watch it; do not rebuild it.

So the bar this meets is *convention with audit*: the agent has no reason to
read the secret, it is visible if it does, and the scoped token limits the
blast radius. That is the same bar CI systems meet with `op run`, and it is
enough for an operator's own automation on an operator's own machine.

## What this deliberately does not do

- **No resolvers in browser-control.** The vault CLIs are the resolvers.
- **No per-use phone approval.** No secrets manager offers it for automation
  today. When one does, it belongs in that tool's CLI, and this pipe works
  unchanged.
- **No MFA.** A second factor is a different human step in a different place,
  and an agentic concern — recognise the prompt, reach the operator, relay
  the answer. See `remote-auth.md`.
- **No site knowledge.** The agent finds the field; browser-control fills it.

## Tests

| Level | Assertion |
|---|---|
| Unit | `--text` + `--stdin` refused before spawning anything |
| Unit | trailing newline trimmed; embedded newline refused; empty refused |
| Unit | error text never contains stdin |
| Integration | `printf 'x' \| browser-control type --stdin` fills a local form; assert by submitting, **never by reading the field back** |
| Integration | `--stdin --submit` submits |

Reading the value back from the field in a test would turn the test into a
way to print the secret. Assert server-side.

## Sequence

1. CLI `type` with `--text`, mirroring `browser_type`. It is odd that it does
   not exist already.
2. `--stdin`, with the refusals above.
3. Docs and `agent_instructions.rs`: agents should pipe from a vault CLI and
   must never ask the operator for a password.

## Verification log

| Claim | Status | Evidence |
|---|---|---|
| Chromium fills a username but not a password for automation | verified | saved credential present; native click; submit failed on empty field |
| CDP keys cannot open the autofill dropdown | verified | `native-key-events.md` spike |
| 1Password Agentic Autofill approves on the desktop, Browserbase only | verified by docs | 1password.dev/agentic-autofill |
| Secret references are the cross-vendor standard | verified | 1Password, Bitwarden, Infisical, Vault docs |
| No CLI `type` command exists today | verified | `browser-control --help`; only `browser_type` via MCP |
| Same-user agent can read any resolver credential | reasoning, not measured | shared principal; no filesystem or env boundary exists between them |
