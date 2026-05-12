> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: accepted
date: 2025-01-15
---

# ADR-006: Two-Tier VS Code Integration

## Context

When running inside VS Code (via Claude Code), the coordinator can either launch an external browser (standard behavior) or connect to VS Code's own internal Chromium instance for tighter integration. The latter requires a companion extension to configure VS Code's startup flags.

## Decision

Support two tiers of VS Code integration:

- **Tier 1 (External Browser):** Default, zero-config. Launches a standalone Chrome/Edge/Chromium outside VS Code. Works everywhere, no extension required.
- **Tier 2 (Native CDP):** Requires companion extension. Automates VS Code's internal Chromium via CDP. User sees automation directly inside the editor.

## Tier 2 Setup Flow

1. Companion extension activates on VS Code startup
2. Checks `~/.vscode/argv.json` for `"remote-debugging-port"` setting
3. If absent, prompts user to enable it and writes `"remote-debugging-port": 0`
4. After VS Code restart, discovers the actual CDP port via:
   - `VSCODE_CDP_PORT` environment variable (set by extension)
   - Port file at `$TMPDIR/vscode-cdp-port`
   - Scanning VS Code process args for `--remote-debugging-port=<N>`
5. Shares discovered port with coordinator via env var and port file

## Consequences

### Positive

- Tier 1 works immediately with zero configuration — no barrier to entry
- Tier 2 provides superior UX: automation visible inside VS Code, no window switching
- Users can opt into Tier 2 incrementally — no forced migration
- `--no-vscode` flag allows explicitly disabling VS Code detection

### Negative

- Tier 2 requires a VS Code restart after initial extension setup
- Port discovery for Tier 2 is fragile (process arg scanning, temp files)
- Two code paths to maintain (external browser vs. VS Code CDP)

### Neutral

- VS Code environment is detected via standard environment variables (`TERM_PROGRAM=vscode`, `VSCODE_PID`)
- The coordinator auto-detects the tier — no user configuration needed beyond installing the extension
