# ADR 0005: Adapter Write Strategy and Ownership

- Status: Accepted
- Date: 2026-10-27 (Codex half), 2026-10-28 (Claude Code half), amended 2026-10-29 (WP4 discovery)
- Source: concept `docs/concept/2026-10-26-d-mcp.md`, WP2 plan "Adapter contract, sync engine and
  Codex adapter", WP3 plan "Claude Code adapter", WP4 plan "Discovery, adoption, redaction, status
  and doctor"

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
`harness-cli` adapter (Claude Code) runs them one by one. `apply` receives an apply context (the
environment, the process runner, a neutral working directory and a variable lookup) and returns an
outcome per entry (`applied`, `failed`, `drift`, `rolled-back`, `lost`); the executor promotes the
applied entries, settles the others against the harness file, and reports each entry's outcome.

**Ownership is recorded, not marked.** d-mcp writes no marker comments into harness files. The
write state `state.toml` records, per harness, scope and entry name, the file path as resolved
before following symlinks, the hash of the entry as sent, the semantic hash of the entry as read
back, and the source definition hash. An entry is owned only when it has a record. A provenance
marker (`written`) tells whether d-mcp itself wrote the entry with those hashes; it is set on every
promoted d-mcp write and cleared by a take-over.

**Three-way planning.** Each sync compares desired (rendered), recorded and actual entries. Updates
are decided by comparing the new rendering with the recorded *sent* hash and the actual entry with
the recorded *read-back* hash; a rendering is compared with an actual entry only to detect an
identical collision when the adapter declares the two hashes comparable. A drifted or colliding
entry is never modified until the user takes it over (`sync --take-over`), and an entry is removed
only when it is unchanged since d-mcp wrote it.

**Intent before write.** Before persisting, d-mcp records `pending` intents with the expected and
the previous owned hashes, under the store lock: all at once for a direct-edit adapter, one right
before each CLI call for a harness-CLI adapter. After verification they are promoted; on a failed
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

**Claude Code is written through its CLI and only read by d-mcp.** `~/.claude.json` belongs to
running Claude Code sessions and holds account data, caches and history. d-mcp never opens it for
writing (a structural test guards this): it adds entries with `claude mcp add-json -s user`,
removes them with `claude mcp remove -s user`, and updates by removing and adding. It reads the
file only to plan, verify and discover, deserializing just the user-scope `mcpServers` values, the
names of local-scope entries and, per project, the trust and legacy approval flags (see the
amendment below), and keeps no bytes. Every call runs from a neutral working directory with
an explicit child environment (`PATH`, `HOME`, `CLAUDE_CONFIG_DIR`, `DISABLE_AUTOUPDATER=1`, no
inherited `MCP_CLIENT_SECRET`); the client secret of `client_secret_env` is passed only through
`MCP_CLIENT_SECRET` of that child. When not even a fresh temporary directory is neutral, nothing
is run for Claude Code (`harness-no-neutral-dir`). With an empty `CLAUDE_CONFIG_DIR`, Claude Code
treats its working directory as its configuration home (legacy `.config.json`, `backups/`), so the
child always runs from a fresh temporary directory that is removed afterwards, never from the
store root.

**One semantic hash for Claude Code.** Claude Code normalizes entries when it writes them (it adds
`args = []`, drops unknown keys, writes JavaScript numbers). The Claude Code entry hash
(`d-mcp-claude-entry-v1`) is computed over a canonical form that absorbs this and is applied alike
to the rendering and the read-back entry, so the two hashes are comparable: the journal records the
sent hash as the expected read-back, identical collisions are detected, and a normalization the
form does not absorb fails verification instead of silently losing ownership.

**Drift check and rollback for a CLI that cannot write atomically.** Right before each removal,
d-mcp re-reads the file and skips the entry as drift when its hash changed since planning; the
remaining window is accepted and absence is verified afterwards. When the add of an Update fails,
the entry just removed is re-added from memory, but only if its record carries the provenance
marker, so foreign or taken-over content never reaches a command line. Otherwise, or when the
rollback fails, the entry is reported as lost by field names, its record reverts to the previous
hashes and the next sync re-adds it. A failed verification is settled against the file like any
pending record: a new entry stays unowned, a replaced one keeps its previous hashes and is drift on
the next sync. A CLI timeout is "unknown": d-mcp settles from the file and
never rolls back blindly.

**Amendment 2026-10-29 (WP4): what d-mcp reads besides user-scope entries.** The decision stays:
`~/.claude.json` is never opened for writing, and its values are read only for user-scope entries.
Discovery of the other scopes adds these reads, all by explicit allow-list with every other key
skipped unread:

- *Local scope* (`projects.<path>.mcpServers`): entry names and the project path only. The values
  are never deserialized, hashed, compared, shown or adopted; `adopt` refuses a local-scope entry.
- *Project trust and legacy approval flags*, per project in `.claude.json`:
  `hasTrustDialogAccepted`, `enabledMcpjsonServers`, `disabledMcpjsonServers` and
  `enableAllProjectMcpServers`, as a boolean or a list of server names. Any other type is read as
  absent. They decide whether an entry of the repository's `.mcp.json` counts as approved.
- *Repository and settings files*, which are not `.claude.json`: `<repository>/.mcp.json` is read
  with its entry values (it belongs to the repository, not to a running session) and imported
  through the same redacted path as user-scope entries, and the three approval keys are read from
  Claude Code's user settings and, in a trusted workspace, the project's `.claude/settings.json`
  and `.claude/settings.local.json`. Every other key of a settings file, such as `env`, is skipped
  unread. These files are opened through the size-capped, no-follow read used for definitions.
  Managed settings are not read.
- *Codex project layers* (`.codex/config.toml`) and the Codex trust level of the repository root are
  read the same way, read-only, with entry values.

Discovery never runs a harness CLI and never writes. Entry content from any scope reaches output
only through the redacted view, and a hash of an entry d-mcp does not own is shown only keyed (see
`docs/reference.md`, Redaction and Display hashes).

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
- Claude Code writes are not atomic across a sync: each entry succeeds or fails on its own, and a
  harness can end with some entries applied and one rolled back or lost. The report shows each
  entry's outcome; the record is never dropped, so the next sync repairs a lost entry.
- A running Claude Code session rewrites `~/.claude.json` from memory and can undo a d-mcp add or
  bring back a removed entry. The next sync reports the first as vanished (and re-adds it) and the
  second as an unowned entry.
- Replacing a Claude Code entry clears its stored OAuth tokens and client secret (verified against
  2.1.310), so an Update of an OAuth server reports that a new login is needed.
- Reading trust and approval flags lets d-mcp report whether a repository's `.mcp.json` entry is
  approved, pending or disabled and whether it shadows a managed user entry, at the cost of
  depending on Claude Code's settings keys (verified against 2.1.312 for the approval rules). A
  settings file d-mcp cannot read makes the approval `unknown`, never a guess.
- d-mcp depends on the `claude` CLI being on `PATH` to write Claude Code entries. Without it, a sync
  with operations fails for that harness (`harness-cli-not-found`) before anything is journaled.
