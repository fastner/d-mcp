# ADR 0001: Store-Centred Management

- Status: Accepted
- Date: 2026-10-26
- Source: concept `docs/concept/2026-10-26-d-mcp.md`, WP1b plan "Core model, store and resolver"

## Context

MCP server configuration is copied by hand into every coding-agent harness. Claude Code and Codex
each keep their own files, so the same server diverges between harnesses and machines, local fixes
get lost, and nobody can say which definition a harness actually runs. Dalo solved the same class
of problem for agent skills with a local store that treats agent directories as output targets.

## Decision

d-mcp manages MCP server definitions in a local store at `~/.d-mcp` and treats harness
configuration files as pure output targets.

- The store path comes from `--store`, then `D_MCP_STORE`, then `$HOME/.d-mcp`.
- The store owns `config.toml` (sources and selections), the advisory `.lock`, and the built-in
  `local` source at `<store>/local` with priority 0. Later work packages add write state, the
  lock file of pinned definitions, and approvals.
- Several sources are active at the same time, each with a unique explicit priority; the highest
  priority wins, and lower copies are reported as shadowed.
- Resolution is a pure, deterministic function of the store and the sources. An invalid winner or
  an unavailable higher-priority source holds the affected server instead of falling back.
- Mutating commands take the lock before reading `config.toml`; every write is atomic. Dry runs
  and read-only commands take no lock and write nothing.
- Harness files only receive entries d-mcp owns; unmanaged entries stay untouched (WP2).

## Consequences

- The resolved set is reproducible from the store plus its sources, on every machine that shares
  the same sources.
- Shadowing, invalid definitions and unavailable sources are reportable states, not silent
  overwrites or silent fallbacks.
- Anything a harness needs must be modelled in the neutral definition format before it can be
  delivered (see ADR 0003).
- d-mcp reuses Dalo's patterns as concepts, not as a shared store: the two tools keep separate
  stores.
