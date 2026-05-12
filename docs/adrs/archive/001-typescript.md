> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: accepted
date: 2025-01-15
---

# ADR-001: TypeScript as Implementation Language

## Context

The browser coordinator MCP needs a language that provides type safety for protocol-level code (MCP messages, CDP commands), integrates well with the Node.js ecosystem, and aligns with the MCP SDK's primary language.

## Decision

Use TypeScript for all server and extension code.

## Consequences

### Positive

- The MCP SDK (`@modelcontextprotocol/sdk`) is TypeScript-first with full type definitions
- Type safety catches protocol mismatches at compile time (e.g., wrong tool argument shapes)
- VS Code extension API has first-class TypeScript support
- Node.js ecosystem provides `child_process`, `net`, `fs` — all needed for browser lifecycle management
- IDE support (autocomplete, refactoring) accelerates development

### Negative

- Requires a build step (`tsc`) before running
- Type definitions for CDP are extensive but not included — we use raw JSON for CDP communication

### Neutral

- ESM module system (`"type": "module"` in package.json) aligns with modern Node.js conventions
- Minimum Node.js 18 requirement is reasonable for the target audience (developers using Claude Code / VS Code)
