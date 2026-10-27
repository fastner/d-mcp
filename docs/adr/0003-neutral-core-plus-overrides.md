# ADR 0003: Neutral Core Plus Overrides

- Status: Accepted
- Date: 2026-10-26
- Source: concept `docs/concept/2026-10-26-d-mcp.md`, WP1b plan "Core model, store and resolver"

## Context

"Define once" only works if one definition can be rendered for every harness. Claude Code and
Codex share the essentials of an MCP server (a local command or a remote URL, its arguments, its
environment and its auth), but each has settings the other lacks, such as Codex's working
directory and timeouts. A model that tries to cover every harness feature in its core either grows
without bound or silently drops settings.

## Decision

A definition is a strictly validated neutral core plus opaque per-harness override blocks.

- The core is `name`, `description`, `transport` (`stdio` with `command` and `args`, or `http` with
  `url`), `env` (stdio only) and `auth` (http only). Unknown core keys are errors, reported with
  their field path.
- One server per file, `servers/<name>.toml`; the file stem is the conflict key and must equal
  `name`. Names follow a neutral name rule that fits every harness's tool-name limits.
- `harness.codex` and `harness.claude` are the only override tables. They are opaque in WP1b and
  get allow-lists from their adapters (WP2, WP3). Settings without an equivalent elsewhere live
  there; the stdio working directory exists only as `harness.codex.cwd`.
- Defaults are fixed and explicit defaults are equivalent to omitted ones. Two hashes over
  canonical JSON identify a definition: `definition_hash` over the full model (approvals bind to
  it) and `core_hash` without `description` and overrides (duplicate detection across harnesses).

## Consequences

- Most servers are fully described by the core and render identically for every harness.
- Harness-specific behaviour stays possible without widening the core, and each adapter decides
  which override keys it accepts.
- Changing an override changes `definition_hash`, so approvals cover overrides too, while
  `core_hash` still recognises the same server across harnesses.
- A new harness feature needs either an override key or a deliberate core extension; nothing is
  dropped silently.
