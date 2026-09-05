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

That is the whole sink. browser-control gains stdin support on `type` and
nothing else on that path — no knowledge of vaults, references or schemes.

## Resolvers: every vault is a CLI that prints a secret

The pipe is deliberately vault-agnostic. **browser-control must not be tied
to any one vault**; teams and individuals use different ones, and the same
person uses several. Anything that can print a secret to stdout is a
resolver:

| Vault | Resolver command | Notes |
|---|---|---|
| 1Password | `op read op://vault/item/password` | service-account token; no desktop app |
| Bitwarden | `bw get password <item>` | `BW_SESSION` from a service login |
| HashiCorp Vault / OpenBao | `vault kv get -field=password secret/site` | token or approle |
| Infisical | `infisical secrets get SITE_PW --plain` | machine identity |
| macOS Keychain / iCloud Keychain | `security find-internet-password -s site -w` | see caveat below |
| Environment / CI | `printf '%s' "$SITE_PW"` | the degenerate resolver |
| **browser-control's own profile** | `browser-control vault read <origin>` | see next section |

None of these need code in browser-control. The one exception is the last
row, because only browser-control knows its own profile and launch flags.

> **What is installed here matters for sequencing.** On this workstation only
> `security` (macOS/iCloud Keychain) is present — `op`, `bw`, `vault` and
> `infisical` are not. So the resolvers usable *today*, with no install, are
> the Keychain, the environment, and (once built) the Chromium profile. That
> is an argument for shipping the profile resolver: it needs no external tool
> and the credential is already there.

**iCloud Keychain caveat.** Apple's Passwords app has no CLI; `security` reads
the login keychain, into which iCloud Keychain items sync. Each item prompts
for access the first time unless its ACL trusts the calling binary, so the
operator must click "Always Allow" once per item while present. After that
it is non-interactive — until the binary changes and the ACL entry with it.
Usable; not the first choice for a credential that must survive upgrades
unattended.

## The Chromium profile as a vault

The operator has already saved credentials into browser-control's Chromium
profile, which is a legitimate place for them: it is browser-control's own
profile, launched by browser-control, populated deliberately for automation,
and separate from the operator's personal browser. Treating it as a vault
means a `vault read` resolver that reads that profile's `Login Data` and
prints the password for an origin.

### Why this is not credential-stealing tooling

The objection to decrypting a browser's password store is that the same code
opens a *person's* browser. Here it structurally cannot:

Chromium encrypts `Login Data` values (the `v10` prefix, verified on this
profile) with AES-128-CBC under `PBKDF2-HMAC-SHA1(password, "saltysalt",
1003, 16)`. Normally `password` is a random secret in the OS keychain — which
is why reading it prompts, and why the user's main browser is protected.
Launched with **`--use-mock-keychain`**, Chromium uses the literal string
`mock_password` instead. The switch, the constant and the salt are all
present in the Brave binary (verified).

So a decryptor keyed on `mock_password` opens **only profiles that were
launched with that flag** — browser-control's — and returns garbage against
any normal browser. The scoping is a property of the key, not of good
intentions. That is what makes it defensible in a public tool.

### Design

- **Launch flag.** browser-control adds `--use-mock-keychain` to Chromium
  launches for its own profiles. **Opt-in for existing profiles**, because it
  changes the key: credentials saved under the real Keychain key become
  unreadable to Chromium itself once the flag is on. Default-on for profiles
  created after the change. Config: `[chromium] password_store = "mock"`.
- **Migration.** Existing saved logins in a profile switching to the mock
  keychain must be re-saved once. `browser-control vault list` should show
  which entries decrypt and which do not, so the operator knows what to
  redo. This profile has nine; they were saved under the real key.
- **`browser-control vault list [--browser b]`** — origins and usernames in
  the profile's store, never values.
- **`browser-control vault read <origin> [--username u] [--browser b]`** —
  prints the password to stdout, nothing else, exit 1 with a message naming
  the origin (never the value) if absent or undecryptable.
- **Implementation.** Copy `Login Data` (SQLite is locked while the browser
  runs) to a temp file, query `logins` by `signon_realm`, decrypt
  `password_value` with the mock key. Pure Rust: `rusqlite`, `pbkdf2`,
  `aes`/`cbc`, `sha1`. No Keychain access anywhere in the code path.
- **Linux and Windows.** Linux uses `--password-store=basic` with the fixed
  password `peanuts` and the same scheme; Windows uses DPAPI-wrapped keys in
  `Local State` under the `v10` AES-GCM scheme. Same design, per-platform
  key derivation, sequenced after macOS.

### Required verification before merge

The algorithm is documented and the constants are confirmed in the binary,
but a **round-trip has not been performed**: launch a scratch profile with the
flag, save a credential through the UI, decrypt it with `vault read`, and
compare. Until that passes, this section is design, not fact.

Then, end to end:

```sh
browser-control vault read pineheights.casino | browser-control type -b brave/login --ref e7 --stdin --submit
```

## Choosing among resolvers

Prefer, in order: a vault with a scoped service account (1Password, Bitwarden,
Vault) for anything shared or anything that must survive a machine change;
browser-control's own profile for an operator's personal automation, where
the credential already lives in the browser and nothing else needs it; the
OS keychain when the ACL can be set once and the binary is stable; the
environment only in CI.

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
| Unit | mock-keychain key derivation matches a vector encrypted with the documented scheme |
| Unit | `vault read` against a `Login Data` fixture encrypted under the real-key path returns an error, not garbage |
| Unit | `vault list` output never includes a value |
| Round-trip | save a credential in a `--use-mock-keychain` profile via the UI; `vault read` returns it |

Reading the value back from the field in a test would turn the test into a
way to print the secret. Assert server-side.

## Sequence

1. CLI `type` with `--text`, mirroring `browser_type`. It is odd that it does
   not exist already.
2. `--stdin`, with the refusals above. **At this point every external vault
   already works.**
3. `--use-mock-keychain` launch flag, opt-in, default for new profiles.
4. `vault list` and `vault read` for macOS; the round-trip test.
5. Linux and Windows key derivation.
6. Docs and `agent_instructions.rs`: agents pipe from a resolver and must
   never ask the operator for a password.

Steps 1–2 are independently useful and ship first. Steps 3–5 are the
Chromium resolver and can follow at their own pace.

## Verification log

| Claim | Status | Evidence |
|---|---|---|
| Chromium fills a username but not a password for automation | verified | saved credential present; native click; submit failed on empty field |
| CDP keys cannot open the autofill dropdown | verified | `native-key-events.md` spike |
| 1Password Agentic Autofill approves on the desktop, Browserbase only | verified by docs | 1password.dev/agentic-autofill |
| Secret references are the cross-vendor standard | verified | 1Password, Bitwarden, Infisical, Vault docs |
| No CLI `type` command exists today | verified | `browser-control --help`; only `browser_type` via MCP |
| `use-mock-keychain`, `mock_password`, `saltysalt` present in the Brave binary | verified | `strings` on the framework |
| Existing profile logins use the `v10` scheme under the real key | verified | `Login Data` inspected; nine entries |
| Mock key does NOT open real-key entries | verified (small sample) | one profile `v10` entry decrypts to 38% printable garbage under `mock_password`; no Keychain access in the path |
| Mock-keychain positive round-trip (mock key opens mock-key entry) | **not yet performed** | needs a credential saved through the UI in a `--use-mock-keychain` profile |
| Same-user agent can read any resolver credential | reasoning, not measured | shared principal; no filesystem or env boundary exists between them |
