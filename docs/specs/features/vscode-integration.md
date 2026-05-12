> **Note:** This document describes the legacy TypeScript implementation. The
> current Rust CLI is described in the project README and ADR-001. See
> `docs/adrs/archive/` for the original ADRs.

# Feature: VS Code Integration

## Overview

When the coordinator runs inside VS Code (typically via Claude Code), it can detect the VS Code environment and optionally connect to VS Code's internal Chromium for in-editor browser automation. Two tiers provide flexibility: zero-config external browser (Tier 1) and deeper native integration (Tier 2).

## Environment Detection (`vscode-integration.ts`)

**Detection signals:**
- `TERM_PROGRAM=vscode`
- `VSCODE_PID` environment variable
- `VSCODE_CWD` environment variable
- `VSCODE_INJECTION` environment variable

**API:**
- `isInVSCode()` — quick boolean check
- `detectVSCode()` — returns `VSCodeEnvironment` with tier, CDP port, and detection details

**Opt-out:** `--no-vscode` CLI flag disables all VS Code detection.

## Tier 1: External Browser

**Behavior:** Standard browser launch outside VS Code. The coordinator detects it's in VS Code but launches Chrome/Edge/Chromium as a separate process.

**Requirements:** None — works identically to non-VS Code environments.

**When used:**
- Companion extension is not installed
- VS Code CDP port is not discoverable
- User explicitly wants a separate browser

## Tier 2: Native CDP

**Behavior:** Connects to VS Code's own Chromium instance via CDP. Browser automation happens inside the editor — the user sees it directly.

**Requirements:**
1. Companion extension installed and activated
2. `~/.vscode/argv.json` contains `"remote-debugging-port": 0`
3. VS Code restarted after initial configuration

### CDP Port Discovery

The coordinator discovers VS Code's CDP port through multiple methods (tried in order):

1. **Environment variable:** `VSCODE_CDP_PORT` (set by companion extension)
2. **Port file:** `$TMPDIR/vscode-cdp-port` (written by companion extension)
3. **Process arg scanning:** Parse VS Code's process arguments for `--remote-debugging-port=<N>`
   - Linux: Read `/proc/<pid>/cmdline`
   - macOS: `ps` command
   - Windows: `wmic` command

**API:**
- `discoverCDPPort()` — tries all methods, returns port or null
- `scanProcessArgs(pid)` — platform-specific process arg parsing
- `findWebviewTarget(cdpPort)` — query CDP endpoint for webview targets

## Companion Extension (`extension.ts`)

A VS Code extension that bridges the gap between the coordinator and VS Code's internal Chromium.

### Activation

- Activates on VS Code startup
- Checks `~/.vscode/argv.json` for `remote-debugging-port` setting

### Setup Flow

```
First install:
  1. Extension activates
  2. Reads ~/.vscode/argv.json
  3. "remote-debugging-port" not found
  4. Prompts user: "Enable CDP for browser automation?"
  5. User accepts → writes "remote-debugging-port": 0
  6. Prompts VS Code restart

After restart:
  1. Extension activates
  2. Discovers actual CDP port (OS assigned from port 0)
  3. Sets VSCODE_CDP_PORT env var
  4. Writes port to $TMPDIR/vscode-cdp-port
  5. Shows status bar item with CDP status
```

### Commands

- **Show status:** Display current CDP configuration and port
- **Open preview:** Open a URL in VS Code's Simple Browser
- **Enable CDP:** Write configuration to argv.json

### Port Sharing

The extension shares the discovered CDP port via:

1. `VSCODE_CDP_PORT` environment variable (for child processes)
2. Port file at `$TMPDIR/vscode-cdp-port` (for independent processes)

### Cleanup

On deactivation, the extension deletes the port file to prevent stale port references.

## Graceful Degradation

```
Is VS Code detected?
  ├─ No → Launch external browser (standard path)
  └─ Yes
      ├─ Is --no-vscode set? → Launch external browser
      └─ No
          ├─ Is CDP port discoverable? → Tier 2 (native CDP)
          └─ No → Tier 1 (external browser in VS Code)
```
