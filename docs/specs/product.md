> **Note:** This document describes the legacy TypeScript implementation. The
> current Rust CLI is described in the project README and ADR-001. See
> `docs/adrs/archive/` for the original ADRs.

# Product Specification

## Vision

Browser Coordinator MCP is the missing layer between AI coding agents and browser automation. It solves a fundamental architectural problem: MCP servers like Playwright MCP can automate browsers but cannot manage browser processes themselves. The coordinator fills this gap — launching browsers, managing their lifecycle, and proxying automation tools — so that AI agents get seamless browser access with zero configuration.

## Problem

When an AI agent needs to interact with a browser (inspect a page, debug frontend code, take screenshots), the current workflow requires manual browser setup or complex configuration. Playwright MCP expects a browser to already be running with CDP enabled on a known port. This creates friction:

1. Users must manually launch browsers with correct flags
2. Port allocation is manual and error-prone
3. No integration with the IDE (VS Code) where developers actually work
4. Browser lifecycle is unmanaged — processes leak, ports conflict

## Solution

A coordinator MCP server that:

1. **Pre-allocates** a free CDP port at startup
2. **Spawns** a child MCP server (Playwright MCP by default) configured with that port
3. **Launches** the actual browser lazily on the first tool call
4. **Proxies** all browser automation tools from the child MCP
5. **Manages** the full browser lifecycle (launch, restart, stop, cleanup)
6. **Integrates** with VS Code for in-editor browser automation

## Users

### Primary: AI Coding Agent Operators

Developers using Claude Code or Claude Desktop who need browser interaction during coding sessions. They want:

- Zero-config browser access from their AI agent
- Browser tools available alongside coding tools
- No manual browser management

### Secondary: Frontend Developers

Developers debugging frontend applications who benefit from AI-assisted browser interaction:

- Live page inspection during development
- Screenshot-based visual debugging
- Automated testing through natural language

## Non-Goals

- **Not a Playwright replacement.** The coordinator manages browser lifecycle and proxies tools — it does not reimplement browser automation.
- **Not a general browser automation framework.** It specifically serves the MCP ecosystem for AI agent use.
- **Not a testing framework.** While it can assist with testing, it is not designed as a test runner.

## Success Metrics

- **Zero-config startup:** `npx @anthropic-community/browser-coordinator-mcp` works without any flags
- **Transparent proxying:** Child MCP tools work identically whether called directly or through the coordinator
- **Lazy launch overhead:** < 3 seconds for first browser tool call
- **Clean shutdown:** No leaked browser processes after MCP server exits
