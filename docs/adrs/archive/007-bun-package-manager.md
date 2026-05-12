> **ARCHIVED.** This ADR was written for the TypeScript implementation
> (preserved on the `legacy-ts` branch). It is kept for historical reference.
> See `docs/adrs/001-rust-cli-rewrite.md` for the current direction.

---
status: Active
date: 2026-02-17
---

# ADR-007: Bun as package manager

## Context
The project needs a package manager for dependency installation and running scripts. npm is the Node.js default but is noticeably slow for install and script execution.

## Options Considered
### Option 1: npm
- Pro: Ships with Node.js, zero extra setup
- Con: Slow installs, slow script startup, verbose output

### Option 2: bun
- Pro: Dramatically faster installs and script execution
- Pro: Drop-in compatible with npm scripts and `package.json`
- Con: Extra install step for contributors

### Option 3: pnpm
- Pro: Fast, disk-efficient
- Con: Stricter node_modules layout can cause issues with some packages

## Decision
We chose **bun** because it is significantly faster for day-to-day development — installs, script runs, and lockfile resolution are all near-instant.

## Consequences
**Positive:**
- Faster dependency installs and script execution
- `bun run build`, `bun install` replace `npm` equivalents

**Negative:**
- Contributors need bun installed (`curl -fsSL https://bun.sh/install | bash`)
- Lockfile is `bun.lockb` (binary) instead of `package-lock.json`

## Related
- Tech stack: [docs/specs/tech-stack.md](../specs/tech-stack.md)
