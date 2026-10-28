# ADR 0005: Adapter Write Strategy and Ownership

- Status: Accepted (Codex half; WP3 completes it for Claude Code)
- Date: 2026-10-27
- Source: concept `docs/concept/2026-10-26-d-mcp.md`, WP2 plan "Adapter contract, sync engine and
  Codex adapter"

## Context

d-mcp writes MCP server entries into configuration files that belong to the user and to their
coding-agent harnesses. Those files also hold entries d-mcp does not manage, comments, and
sometimes secrets typed in by hand. A sync must never overwrite or lose any of that, must notice
when the user or the harness changed a managed entry, and must keep working after a crash halfway
through a write. Harnesses differ: Codex keeps a plain TOML file that d-mcp can edit, while Claude
Code is best driven through its own CLI.

## Decision

**One adapter contract, two apply modes.** Every harness has an adapter that declares its
capabilities (apply mode, scopes, override allow-list, reserved names, comparability of hashes,
whether entries can vanish, verified harness versions, login-state capability) and implements
locate, read, render with gaps, and `apply(harness_plan)`. The sync engine depends only on this
contract. A `direct-edit` adapter (Codex) batches all operations into one atomic persist; a
`harness-cli` adapter (Claude Code, WP3) runs them one by one.

**Ownership is recorded, not marked.** d-mcp writes no marker comments into harness files. The
write state `state.toml` records, per harness, scope and entry name, the file path as resolved
before following symlinks, the hash of the entry as sent, the semantic hash of the entry as read
back, and the source definition hash. An entry is owned only when it has a record.

**Three-way planning.** Each sync compares desired (rendered), recorded and actual entries. Updates
are decided by comparing the new rendering with the recorded *sent* hash and the actual entry with
the recorded *read-back* hash; a rendering is compared with an actual entry only to detect an
identical collision when the adapter declares the two hashes comparable. A drifted or colliding
entry is never modified until the user takes it over (`sync --take-over`), and an entry is removed
only when it is unchanged since d-mcp wrote it.

**Intent before write.** Before persisting, d-mcp records `pending` intents with the expected and
the previous owned hashes, under the store lock. After verification they are promoted; on a failed
persist or verification they revert; after a crash, the next sync settles them against the actual
file. Ownership is never lost.

**Direct edits touch only managed subtrees (Codex).** d-mcp locates each managed
`mcp_servers.<name>` subtree with `toml_edit` and splices new text into the original bytes; it never
re-encodes the whole file, because that would rewrite line endings and the trailing newline. The
persist is atomic, symlink-aware and keeps the file mode (not the owner, group, extended attributes
or ACLs; a changed owner is a warning); the spliced text is checked to change nothing outside the
plan before anything is journaled; a concurrency re-check right before the persist aborts when the
file or its link target changed; a failed read-back verification restores the original bytes from
memory. d-mcp keeps no backup copy of harness files, so nothing it writes
can contain foreign secrets.

**Harness-local secrets stay local.** Codex has no environment form for the OAuth client secret.
`oauth.client_secret` is therefore a preserved key: never written, read, hashed or shown by d-mcp,
and kept on Update. A missing one is reported as a manual step. `oauth.callback_url`, which Codex
writes itself next to a client secret, and `oauth.callback_port` are preserved the same way.
An Update carries a preserved key over only while it fits the new rendering: Codex refuses its
whole configuration for an `oauth` table on a stdio entry or a client secret without a client ID.
A key that no longer fits is removed and reported by name as a `note` gap, never by value.

## Consequences

- Users can keep editing harness files by hand; d-mcp reports drift instead of fighting it, and
  `sync --take-over` accepts their change.
- Codex can re-save its own file without causing drift. Verified against Codex 0.165.0 (`codex mcp
  add` of another server): it reorders keys, rewrites numbers such as `30` to `30.0`, drops
  comments, and drops keys with default values (`args = []`, `enabled = true`, `required = false`,
  `enabled_tools = []`, `disabled_tools = []`). Entry hashes are semantic: they use one number form
  and ignore default-valued keys, and d-mcp never writes those defaults. Because an empty
  `enabled_tools` changes its meaning when dropped (no tool becomes every tool), `validate` rejects
  it. A captured Codex rewrite is part of the test fixtures.
- Codex refuses to load its whole configuration for some values (`cwd` on a streamable HTTP entry,
  a timeout it cannot convert into a duration). `validate` rejects them, so `resolve` and `sync`
  agree and such a server is held instead of written.
- Ownership survives crashes and failed writes, at the cost of a second persisted file.
- Moving `CODEX_HOME` is reported as a location change; `sync --relocate` moves ownership but never
  edits the old file.
- WP3 adds the `harness-cli` half for Claude Code, where sent and read-back hashes are not
  comparable, and completes this ADR.
