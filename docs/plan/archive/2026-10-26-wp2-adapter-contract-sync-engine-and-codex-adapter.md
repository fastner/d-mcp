# WP2: Adapter contract, sync engine and Codex adapter

**Plan status:** Implemented
**Source:** effective-flow plan
**Recommended workflow:** Feature (`effective-flow build`)

## Requirement

Work package 2 of the concept `docs/concept/2026-10-26-d-mcp.md`. It delivers the first
end-to-end sync, into Codex at user scope. It covers:

- the adapter contract and the shared process runner;
- sync planning with dry run;
- the write state, with intent records and hashes;
- the full entry lifecycle: add, update, remove only when unchanged, drift and gap reporting,
  take-over, unmanage;
- format-preserving editing of `~/.codex/config.toml`, including the Codex secret fields, OAuth
  rendering and the `harness.codex` override block;
- the adapter capability for reading the Codex login state, without calling it during `sync`.

The workflow is Feature: this is the first user-visible write capability.

**Dependencies and re-planning.** WP2 builds on the implemented WP1a and WP1b
(`docs/plan/archive/2026-10-26-wp1a-project-scaffold.md`,
`docs/plan/archive/2026-10-26-wp1-core-model-store-and-resolver.md`). This plan was re-planned
against the merged WP1 code (`origin/main` e3bc451): module, type and error names below are the
real ones. WP3, WP4 and WP5 depend on the adapter contract, the write state, the process runner
and the sync planner defined here. The plan carries a Codex verification gate that is part of
its acceptance criteria.

**Context:**

- **Verified (Codex configuration reference, research of 2026-10-26):**
  - Codex reads `$CODEX_HOME/config.toml`, by default `~/.codex/config.toml`, with
    `[mcp_servers.<name>]` tables.
  - The documented fields:

    | Group | Fields |
    |---|---|
    | Transport | `command`, `args`, `env`, `env_vars` (strings or `{name, source}`), `cwd`, `url` |
    | Auth and headers | `bearer_token_env_var`, `http_headers`, `env_http_headers`, `http_headers_helper`, `scopes`, `oauth.client_id`, `oauth.client_secret`, `oauth_resource`, `auth` |
    | Control | `enabled`, `required`, `startup_timeout_sec` (alias `startup_timeout_ms`), `tool_timeout_sec`, `enabled_tools`, `disabled_tools` |
    | Tool approval | `default_tools_approval_mode`, `tools.<tool>.*` |
    | Other | `experimental_environment` |

  - `auth` is `oauth` by default, or `chatgpt`.
  - There is no inline `${VAR}` expansion.
  - OAuth login runs through `codex mcp login`.
- **Verified:** Codex is not installed on this machine.
- **Verified (WP1 code on `origin/main`):**
  - Crate constraints: edition 2024, `rust-version = "1.94"`, `unsafe_code = "forbid"`, no
    `libc` dependency; `toml_edit` 0.25 (`DocumentMut`) is already a dependency and WP1 parses
    with it through `spanned::parse_document`.
  - `store` owns every write (`src/lib.rs` module contract). `store::write_atomic_with` forces
    mode `0600`, writes its temp file into `path.parent()`, and its `before_persist` seam maps
    any failure to `io-error` (exit 4). `StoreWarning::ParentSyncFailed` reports a failed parent
    `fsync` after a successful rename.
  - `store::Environment` holds `home`, `store` and `cwd`; it has no `CODEX_HOME` and no `PATH`.
    An empty `D_MCP_STORE` counts as unset; relative paths resolve through
    `Environment::absolute`.
  - `StoreLock::acquire` retries for about 1.6 s, then fails with `store-locked` (exit 3).
    A dry run takes no lock and creates, changes or deletes no file, `.lock` included.
    `state.toml` is already a known store entry.
  - The JSON envelope (`src/output.rs`, `docs/reference.md`): `ok` is `true` on exit 0 and
    `false` otherwise, `data` is `null` on failure, and only `resolve` reports `problems`.
    `Ui::success` returns only 0 or 1.
  - The resolver is pure over source snapshots whose file entries are
    `Result<Definition, FileProblem>`. Statuses are `resolved`, `hold` (reasons
    `invalid-winner`, `source-unavailable`) and `not-offered`; `ProblemCode` is a closed,
    documented list, and problems name a `Rule` from `src/rule.rs`.
  - `harness.<name>` overrides are stored as an opaque `serde_json::Map`; datetimes become
    strings, integers and floats stay distinct. WP1's name heuristic already rejects
    secret-looking string-valued override keys, except environment-reference keys (`*_env`,
    `*_var`, keys in `env_http_headers`) outside an `env` table.
  - Hashes: `definition_hash` and `core_hash`, tagged `d-mcp-definition-v1` and `d-mcp-core-v1`,
    over canonical JSON with explicitly sorted keys; `tagged_sha256` and `write_canonical` in
    `src/definition/hash.rs` are private.
  - `Error` (`src/error.rs`) is a closed enum; a unit test checks every variant against the
    `<!-- error-codes -->` table in `docs/reference.md`.
  - Selections are `HarnessSet::All` or `HarnessSet::Only(non-empty set)`.
  - Test isolation (`tests/common/mod.rs`) sets `HOME`, `CLAUDE_CONFIG_DIR`,
    `CODEX_HOME=<tmp>/codex` and `XDG_*`, removes `D_MCP_STORE`, but inherits `PATH`.
- **Assumption:** behaviour not covered by the reference is checked in the Codex verification gate.

## Architecture decisions

- **Process runner** (`src/process.rs`, owned by WP2 and reused by WP3 and WP5).
  - It runs without a shell, with stdin null and an explicit environment: the inherited one plus
    the resolved `CODEX_HOME`, with an API hook for WP3's child-environment injection.
  - stdout and stderr are read on reader threads with a size cap; the timeout polls `try_wait`.
    On timeout only the direct child is killed, and reader threads are not joined afterwards. A
    grandchild (for example the npm `codex` shim's native binary) may outlive the kill; this
    limit is documented, because process-group kill needs `unsafe` or `libc`.
  - **Child output is never printed (decision from review).** stdout is parsed in memory only.
    Output reports only the program, the exit status or signal, and `timeout` or `not-found`,
    because a harness CLI can echo foreign secrets from a broken config line.
- **Adapter contract.** Each adapter declares:
  - its identifier and its apply mode, `DirectEdit` or `HarnessCli`;
  - the scopes it can discover and write (Codex in v1: discover user, write user);
  - how it reads entries (name, scope, file, semantic content);
  - how it renders a resolved definition into a native entry plus a list of gaps;
  - whether rendered and read-back hashes are comparable (`DirectEdit` Codex: yes; WP3: no);
  - its override-key allow-list with per-key types, and which keys of the *other* harnesses'
    override blocks are security-relevant;
  - its reserved names (Codex: none);
  - whether its entries can vanish on their own (Codex: no);
  - the login-state capability (see "Codex login state");
  - the harness versions it was verified against;
  - `apply(harness_plan)`, which returns a result per entry. `DirectEdit` adapters batch all
    operations into one atomic persist; `HarnessCli` adapters (WP3) run them one by one.

  A native-to-neutral import function is part of the contract, but it is implemented in WP4,
  where adoption and duplicate detection need it.
- **Override validation without coupling (user decision: separate `validate` step).**
  `parse_definition` stays adapter-agnostic, so WP1's valid parse fixtures stay valid.
  - `validate` runs between source scan and `resolve`, in both `sync` and the `resolve` command.
    It walks the adapter registry and turns a definition whose `harness.codex` table breaks the
    allow-list or a per-key type into `Err(FileProblem { rule, field, line, column })`.
  - The existing `invalid-definition` and `invalid-winner` problems then report it, and the
    hold fails closed. No new `ProblemCode` is needed; two new rule codes are added to
    `src/rule.rs` and the rule table in `docs/reference.md`: `override-key` and `override-type`.
  - Consequence: an invalid Codex override invalidates the whole definition, so the server is
    held for every harness, Claude Code included.
  - `harness.claude` stays opaque until WP3, so WP3 will newly invalidate definitions with
    unknown Claude keys (migration note for WP3).
  - The per-key type check reads the spanned TOML node kind, so a datetime is never accepted as
    a string.
- **Gaps.** When a definition carries something a harness cannot express, rendering returns
  `Gap{field, severity, reason}`. Gaps appear in `sync` text and JSON. In v1,
  `harness.codex.disabled_tools` and `enabled_tools` are security-relevant: the Claude Code
  adapter reports them as an `important` gap. Codex `note` gaps: `auth = none`, an
  `oauth_supported` flag on `bearer_env` that Codex ignores, and the `client_secret_env` manual
  step. An `important` gap blocks the entry: it is not written.
- **Write state** (`state.toml`, created here, schema version 1, documented in
  `docs/reference.md` like `config.toml`). A record is keyed by harness, scope and name. It holds:
  - the file path as resolved before following symlinks (`$CODEX_HOME/config.toml`), so
    retargeting a dotfile link is not a location change;
  - `sent`: the hash of the entry as sent (rendered);
  - `readback`: the semantic hash of the entry as read back;
  - `definition_hash`: the source definition hash (WP5 binds approvals to it);
  - the time as Unix epoch seconds and the d-mcp version;
  - a status, `owned` or `pending`;
  - for `pending`: the expected `sent`/`readback` plus the previous owned hashes
    (`prev_sent`, `prev_readback`), or none for an Add.

  A corrupt `state.toml` fails with `state-corrupt` (exit 3); another schema version with
  `schema-unsupported` (exit 3). A changed `definition_hash` with an identical rendering
  refreshes the record without writing the harness file.
- **Pending intent.** Before persisting, d-mcp writes a `pending` record with the expected
  hashes and the previous owned hashes, under the store lock.
  - After verification the record is promoted to `owned`.
  - On a failed persist or failed verification, an Update or Remove reverts to the previous owned
    record; only an Add whose entry is absent drops the record.
  - On the next sync, a `pending` record whose expected read-back hash equals the actual hash is
    promoted; one whose actual hash equals `prev_readback` reverts to the previous owned record.
    This covers a crash between the file write and the state write without losing ownership.
- **Entry hashes.** New tagged hashes over canonical JSON (`d-mcp-codex-entry-v1`), used for both
  the sent and the read-back hash of a direct-edit adapter. WP1's private canonical writer and
  tagged hash become a shared `canonical::tagged_hash(tag, &Value)` instead of a copy. The
  semantic hash covers the full `mcp_servers.<name>` subtree, including all sub-tables. Before
  hashing, values are canonicalized:
  - `env_vars` strings become `{name, source = "local"}`;
  - `startup_timeout_ms` becomes `startup_timeout_sec`;
  - integers and floats get one canonical number form. This deliberately differs from WP1's
    definition hashes, where they stay distinct, because Codex may rewrite `30` as `30.0`;
  - datetimes keep WP1's `+00:00` → `Z` rule;
  - keys are sorted.

  Harness-local preserved keys (`oauth.client_secret`) are excluded from the hash and never read
  into any model or output.
- **Update decision.** An Update is planned when the newly rendered hash differs from the recorded
  sent hash and the actual hash equals the recorded read-back hash. The plan never compares a
  rendered entry with the read-back entry directly, except for identical-collision detection in
  adapters that declare the two hashes comparable.
- **Sync operations** per harness and entry:
  - **Add:** desired, not recorded, absent.
  - **Update:** as defined above.
  - **Remove:** not desired, recorded, actual hash equals the recorded read-back hash.
  - **Drift:** recorded, actual hash differs. Only reported.
  - **Collision:** desired, not recorded, a same-name entry exists. Only reported.
  - **Identical collision:** like Collision, but the adapter declares comparable hashes and the
    actual entry's hash equals the rendering. It is resolved by `--take-over` without writing
    anything.
  - **Vanished:** recorded, desired, absent. For Codex the entry is re-added and reported as
    "removed outside d-mcp; run `unmanage <server> --harness codex` to keep it removed".
  - **Held:** the resolver reports `hold` for a server (WP1b reasons `invalid-winner` and
    `source-unavailable`; WP5 adds `pending-approval`), **or `not-offered` for a recorded,
    still-selected server (user decision)**. If the entry is recorded, it is left unchanged, and
    the reason is reported. If it is not recorded, nothing is planned.
  - **Orphaned:** recorded, not desired, absent. The record is dropped.
  - **Location changed:** the recorded file path differs from the resolved one, for example
    because `CODEX_HOME` changed. Reported; nothing is planned for that harness until the user runs
    `sync --relocate`.
  - **Unchanged.**
- **Take-over.** `sync --take-over <name>` sets `readback = actual` and `sent = actual`, then plans
  normally in the same run: an identical collision plans Unchanged, a non-identical entry plans an
  Update. It applies to Drift, Collision and identical Collision. WP4 adoption reuses exactly this
  operation. An unknown name is an error (`unknown-take-over`, exit 1).
- **Relocate.** `sync --relocate` rewrites the recorded paths of that harness to the newly
  resolved location, then plans normally, which usually yields Vanished and a re-add. It reports
  that the old file still contains the formerly managed tables and that d-mcp no longer owns
  them; it never edits the old file.
- **Unmanage (user decision on the selection).** `unmanage <server> [--harness codex]` (without
  `--harness`: every harness with a record) removes the record and drops that harness from the
  server's selection. The entry is left as it is, so the next sync does not report a Collision.
  - A selection of `all` becomes `Only(known adapters minus the harness)`; the conversion is
    reported. An empty result removes the selection.
  - A server with no record fails with `not-managed` (exit 1). A `--harness` value without an
    adapter (`claude` in WP2) is a usage error (exit 2).
- **Store write API (user decision: new store function).** `store` gains
  `write_atomic_file(path, bytes, mode: Option<u32>, before_persist)`, whose seam returns a d-mcp
  `Result`, so a concurrency mismatch maps to `harness-file-changed` (exit 3). `mode: None` keeps
  `0600` for new files; `write_atomic` stays the `0600` store wrapper. `StoreWarning` warnings are
  surfaced as `warning:` lines for harness files too. `sync` never writes files itself.
- **Direct-edit write rules (Codex):**
  1. Resolve a symlinked `config.toml` to its target first. Open the target without following
     further links and without blocking (WP1's `no_follow` with non-blocking semantics), require
     a regular UTF-8 file, keep its bytes in memory and its byte hash.
  2. Apply the operations with `toml_edit`, touching only managed subtrees.
  3. In the `before_persist` seam, re-resolve the link and re-read the target. If the link target
     or the bytes changed, abort that harness with `harness-file-changed` (exit 3).
  4. Persist through `store::write_atomic_file` into the target's directory. The existing file
     mode is kept; a new file gets `0600`.
  5. Re-read and verify the expected hashes. If verification fails and the file still has the
     bytes d-mcp wrote, restore the in-memory original atomically and exit 3
     (`harness-verify-failed`); a failed restore is reported loudly with the same code.
- **No on-disk backup (user decision).** d-mcp keeps no copy of harness files anywhere, so no file
  it writes can contain foreign secrets.
- **Managed region and comments.**
  - Add always writes standard tables, using the file's line ending (LF or CRLF).
  - Remove deletes the `mcp_servers.<name>` subtree. `toml_edit` attaches comments above a header
    to that table's prefix decor; Remove moves that decor to the next item so comments above the
    header are kept. d-mcp writes no marker comments.
  - An owned entry that the user turned into an inline or dotted-key style is reported as
    unsupported (exit 1) and left untouched.
  - An `mcp_servers` root defined as an inline table or through dotted keys makes the whole
    harness unsupported for writing (exit 1); nothing is written.
  - Parse errors are reported with the position from `spanned::line_column` only; `TomlError`
    text is never printed.
- **Codex rendering:**
  - **stdio:** `command`, `args`. Forwards go into `env_vars`, non-secret literals into `env`.
    `cwd` comes from `harness.codex`.
  - **http:** `url`. `bearer_env` becomes `bearer_token_env_var`, `header_env` becomes
    `env_http_headers`.
  - **`oauth`:** `client_id` goes to `oauth.client_id`, `scopes` to `scopes`, and `auth` is
    omitted (Codex's default).
  - **`client_secret_env` (user decision):** Codex has no environment form for the client secret
    (assumption, verified in the Codex gate). d-mcp therefore treats `oauth.client_secret` as a
    harness-local preserved key: never written, never read, never hashed, kept on Update. When it
    is missing, d-mcp reports a manual step (`note` gap): enter it once into the Codex table.
  - **`auth = none`:** Codex then falls back to OAuth; reported as a `note` gap.
- **`harness.codex` override allow-list with types:**

  | Key | Type |
  |---|---|
  | `cwd` | absolute path string, no `~` |
  | `enabled`, `required` | boolean |
  | `startup_timeout_sec`, `tool_timeout_sec` | non-negative integer or float |
  | `enabled_tools`, `disabled_tools` | array of strings |

  Deliberately excluded in v1 for security reasons: `default_tools_approval_mode`,
  `tools.<tool>.approval_mode`, `http_headers_helper` (it runs a local command) and
  `experimental_environment`. An owned entry that contains them counts as drift.
- **`CODEX_HOME` and Codex presence.** `store::Environment` gains `codex_home: Option<OsString>`
  and `path: Option<OsString>`, read in `from_process`. An empty `CODEX_HOME` counts as unset; a
  relative one resolves through `Environment::absolute`; otherwise `$HOME/.codex` applies. The
  binary is detected by a lookup of `codex` in `Environment.path`, without executing it.
- **Codex absent (decision).** If neither the `codex` binary nor the config file exists, Codex is
  skipped and reported as "harness not present" (exit 0). d-mcp creates no file and no directory.
  If only the file is missing but the binary is present, d-mcp creates the file with `0600` and
  its directory with `0700`.
- **Dry run and lock.** Both commands check the store before locking (`require_initialized`).
  `sync --dry-run` takes no lock, writes neither `state.toml` nor `config.toml`, and treats a
  matching `pending` record as owned for planning only. `unmanage --dry-run` only reports.
- **Output rules until WP4.** Output (text, JSON, errors) contains only harness, scope, file path,
  entry name, operation, outcome, gaps and line or column numbers, plus hashes of owned entries.
  It never contains field values, hashes of foreign or unowned entries (those stay in
  `state.toml` until WP4's keyed display hashes), child process output or `toml_edit` error text.
- **Envelope and exit codes for `sync` (user decision: data also on failure).** WP2 extends the
  envelope contract: a non-zero exit of `sync` may carry `data` with the per-entry outcome, with
  `ok: false` and `error` set to the aggregate code. `sync` also reports resolver problems in
  `problems`. `output` gains a way to finish a report with an explicit exit code.
  - 0: everything is applied, unchanged, held, re-added after Vanished (with a notice), or the
    harness is not present; `note` gaps do not change the code.
  - 1 (`sync-blocked`): at least one entry is blocked by Drift, Collision, Location changed, an
    unsupported entry or root style, or an `important` gap.
  - 3: `harness-file-changed`, `harness-config-invalid` (invalid TOML, non-UTF-8 or non-regular
    `config.toml`), `harness-verify-failed`, `state-corrupt`, `schema-unsupported`.
  - 4: an environment problem (`io-error`, unset `HOME`).

  Across harnesses the highest-priority code wins, in the order 3, 4, 1, 0. Global errors
  (`store-locked`, `config-corrupt`, an uninitialized store) abort before any harness is touched.
- **Codex login state (user decision: capability and tests only).** The adapter declares a
  login-state capability that reads Codex's own server listing (`codex mcp list`, with a JSON
  option if one exists) through the process runner, with values `logged-in`, `needs-login`,
  `not-applicable` and `unknown`. `sync` does not call it; WP4's `status --auth` is its first
  consumer. d-mcp never reads Codex token storage.

## Affected files

| File | Description |
|---|---|
| `Cargo.toml` | No new dependency planned (`toml_edit` 0.25 is present). Any new crate must be named here and pass `deny.toml`; no `libc`, no `unsafe`, MSRV 1.94 |
| `src/process.rs` | Shared external-program runner |
| `src/adapter/mod.rs` | Contract, operation, gap and login-state types, capability declaration, registry |
| `src/adapter/codex.rs` | Codex adapter |
| `src/validate.rs` | Override validation over the adapter registry; invalid overrides become `FileProblem`s |
| `src/state.rs` | Write state with pending and owned records |
| `src/sync.rs` | Three-way planner, batch and per-entry executor, verification, restore from memory, exit-code aggregation |
| `src/canonical.rs` (or `src/definition/hash.rs`) | Shared `tagged_hash(tag, &Value)` and canonical writer |
| `src/cli.rs` | `sync` (`--harness`, `--take-over`, `--relocate`) and `unmanage`; `validate` in `resolve` too |
| `src/error.rs` | New variants with stable codes: `harness-file-changed` (3), `harness-config-invalid` (3), `harness-verify-failed` (3), `state-corrupt` (3), `sync-blocked` (1), `not-managed` (1), `unknown-take-over` (1); `schema-unsupported` reused |
| `src/rule.rs` | Rule codes `override-key`, `override-type` |
| `src/store.rs` | `Environment.codex_home` and `Environment.path`; `write_atomic_file` with mode and d-mcp `Result` seam |
| `src/output.rs` | Report with explicit exit code; `data` on a non-zero `sync` exit |
| `src/lib.rs` | New modules |
| `tests/common/mod.rs` | `TestEnv::with_path(dirs)` and `without_codex()` controlling `PATH`; a missing-`CODEX_HOME` case |
| `tests/support/fake-codex.sh` | Executable `#!/bin/sh` fake `codex` for login-state listings, a timeout (portable `sleep`), a grandchild holding the pipe, oversized output, and canaries on stdout and stderr |
| `tests/fixtures/codex/` | `config.toml` fixtures: comments (including above managed headers), sub-tables, CRLF, no trailing newline, last-table entries, foreign servers with canary literals, invalid TOML with a canary on the broken line, a symlinked config, inline and dotted `mcp_servers` roots |
| `tests/fixtures/codex-overrides/` | `valid/` and `invalid/<rule-code>/` override fixtures for `validate` |
| `tests/sync_codex.rs` | Integration tests |
| `tests/json_contract.rs` | `sync` and `unmanage` documents, including `data` on a non-zero exit |
| `docs/reference.md` | Sync semantics, operations, gaps, envelope change, error-code table, rule codes, `state.toml`, Codex mapping, override allow-list with types and exclusions |
| `docs/adr/0005-adapter-write-strategy-and-ownership.md` | ADR, Codex half; WP3 completes it |

## Implementation details

### Approach

1. **Process runner**, the adapter contract with its registry, and the `validate` step (also in
   `resolve`).
2. **Store extensions:** `Environment.codex_home`/`path`, `write_atomic_file`, the shared
   canonical hash.
3. **Write state** with pending and owned records, under the store lock.
4. **Codex read path:** locate the file, parse it with `toml_edit`, extract subtrees, compute
   canonical semantic hashes.
5. **Codex rendering** with gaps, override validation, the preserved-key rule and the
   `client_secret_env` reporting.
6. **Pure sync planner** with all operation types. It is unit-tested without files, including a
   test adapter that produces an `important` gap.
7. **Direct-edit executor:**
   1. Hold the store lock for the whole sync (not in a dry run).
   2. Batch the operations and write pending records.
   3. Run the concurrency re-check in the persist seam, then the atomic and symlink-aware
      persist.
   4. Verify, restoring from memory if needed.
   5. Promote or revert the records.
8. **CLI:** `sync` and `unmanage`, with the output rules, the envelope extension and exit codes.
9. **Login-state capability** tested through the fake `codex`; the real one in the Codex gate.
10. **Documentation and ADR.**

### Component structure

`sync` depends only on the adapter contract. The Codex adapter depends on `definition`, `store`
and `toml_edit`. `validate` sits between source scan and the resolver.

### State management

The write state is the only new persisted state. Records move from `pending` to `owned` only after
verification, and back to the previous owned record on failure.

### API integration

`codex mcp list` is the only external call, made through the process runner, and only by the
login-state capability, not by `sync`.

### Styling approach

Not relevant: a command-line tool.

### Accessibility

Plain-text output that does not depend on colour.

### Edge cases

- **Codex absent:** skipped, nothing created, exit 0.
- **Binary present, file missing:** the file is created with `0600`.
- **Invalid TOML, non-UTF-8 or non-regular `config.toml`:** abort for Codex with
  `harness-config-invalid`, show the position only.
- **Symlinked `config.toml`:** the link is kept and the target is written; a retargeted link
  between read and persist aborts with exit 3.
- **Foreign same-name table:** Collision. Identical content: identical collision.
- **A managed table edited by hand:** Drift. `--take-over` accepts it, then the normal Update
  follows.
- **A managed entry the user deleted:** Vanished, re-added with the "removed outside d-mcp"
  notice.
- **A recorded server no longer offered by any source:** Held, entry kept.
- **`CODEX_HOME` changed:** Location changed, nothing planned until `--relocate`.
- **File changed between read and persist:** abort, exit 3.
- **Persist fails or crash after persist, before the state is promoted:** ownership is kept;
  the next sync promotes or reverts the pending record.
- **Verification fails after persist:** the original is restored from memory, exit 3.
- **An owned entry turned into inline or dotted style, or an inline/dotted `mcp_servers` root:**
  unsupported, left untouched, exit 1.
- **`oauth.client_secret` set by hand in a managed table:** preserved, never hashed or read, no
  drift.
- **An excluded security key added to an owned entry:** Drift.
- **A selection that targets only `claude`:** nothing planned for Codex.
- **`unmanage` of `all`:** converted to an explicit list without `codex`, reported.

## Acceptance criteria

- [ ] `mise run check` and `mise run check:full` pass; line coverage stays at or above
      `coverage-threshold`; every CI job is green on the PR.
- [ ] On every fixture, all bytes outside the managed `mcp_servers.<name>` subtrees are unchanged
      after sync. This includes comments above managed headers, CRLF files and files without a
      trailing newline.
- [ ] A second sync right after the first plans only Unchanged.
- [ ] Each operation type has a test with the expected file, write-state and exit-code outcome:
      Add, Update, Remove, Drift, Collision, identical Collision, Vanished, Held (including
      `not-offered`), Orphaned, Location changed (with `--relocate`) and Unchanged.
- [ ] Without `--take-over`, a drifted entry is never modified.
- [ ] With `--take-over`, Drift is accepted and followed by an Update in the same run; an
      identical collision is taken over without a write.
- [ ] Failure-injection tests cover:
      - a file or link target changed between read and persist: abort, unchanged file, exit 3;
      - a crash between persist and promotion: promoted on the next run;
      - a failed persist of an Update: ownership is kept;
      - a failed verification: the original bytes are restored, exit 3.
- [ ] Symlink and mode tests: the link is preserved, the mode is preserved, new files are `0600`.
- [ ] Rendering tests cover:
      - forwards in `env_vars`;
      - `bearer_token_env_var` and `env_http_headers`;
      - `oauth.client_id` and `scopes`;
      - preservation of `oauth.client_secret`;
      - `cwd` from the override;
      - the `auth = none` and `client_secret_env` note gaps;
      - no secret value in any rendered table.
- [ ] Every `tests/fixtures/codex-overrides/invalid/<rule-code>/` fixture fails `validate` with
      that code (unknown key, excluded key, wrong type including a datetime `cwd`); every valid
      one passes; WP1's parse fixtures stay valid; `resolve` and `sync` agree on the held server.
- [ ] Canary tests: no canary from a foreign entry, from a broken TOML line, or from the fake
      `codex` stdout or stderr appears in stdout, stderr or the store; no hash of a foreign entry
      appears in output.
- [ ] Fake-`codex` tests map listings to `logged-in`, `needs-login`, `not-applicable` and
      `unknown`, including a missing binary, a timeout, a grandchild holding the pipe and oversized
      output; `sync` never calls `codex`.
- [ ] With a controlled `PATH` and neither the Codex binary nor the config file present, sync
      creates nothing and reports "harness not present" with exit 0.
- [ ] `sync --dry-run` and `unmanage --dry-run` leave every store and harness file, `.lock`
      included, byte-identical.
- [ ] `unmanage` tests: `all` becomes an explicit list, the last harness removes the selection,
      `not-managed` exit 1, an unknown harness exit 2.
- [ ] The `--json` output of `sync` and `unmanage` matches `docs/reference.md`, including `data`
      with `ok: false` on exit 1 and 3; every new error variant maps to its documented code and
      exit code.
- [ ] Codex verification gate passed: on a machine with Codex, the managed servers appear in
      Codex's own listing with the expected transport and command or URL. The Codex version is
      recorded in the adapter's verified-versions declaration.

## Validation plan

- Unit tests for the planner, the canonicalization, the pending-record transitions and
  `validate`.
- Integration tests with fixtures, the fake `codex`, a controlled `PATH` and `CODEX_HOME` in a
  tempdir.
- Failure-injection tests for concurrency, crash recovery, failed persist and restore.
- JSON-contract tests for the envelope extension.
- A manual verification with an installed Codex (the Codex verification gate).

## Assumptions and open points

- Codex has no environment form for `oauth.client_secret`. If the verification finds one, render
  `client_secret_env` through it instead of the preserved-key rule.
- `codex mcp list` reports a login state, ideally as JSON. If it does not, the Codex login state is
  limited to `unknown`.
- Codex may rewrite numbers (`30` as `30.0`); the unified number form in the entry hash absorbs
  this.
- Codex profile files (`$CODEX_HOME/<profile>.config.toml`) are not discovered in v1.
- Project-scope discovery (`<project>/.codex/config.toml`, with its trust level) moves to WP4,
  where it is used.
- Killing a timed-out child does not reach its grandchildren; documented, not fixed in v1.
- Codex verification gate B is part of the acceptance criteria, not a planning blocker. During
  implementation, on a machine with Codex installed, verify the following and record the Codex
  version:
  - the field names and canonicalization above;
  - the absence of an environment form for `oauth.client_secret`;
  - `codex mcp list` output and login-state reporting;
  - `toml_edit` round-trips on Codex-written files.

## Plan review

**Result:** Approved

**Round 2 (2026-10-27, re-planning gate A against the merged WP1 code).**

### Summary

| Area | Critical | Important | Note |
|---|---:|---:|---:|
| Architecture | 1 | 4 | 0 |
| Security | 1 | 2 | 1 |
| Data protection | 1 | 1 | 0 |
| Error cases | 1 | 6 | 0 |
| Testability | 0 | 2 | 1 |
| Scope | 0 | 1 | 2 |
| Maintainability | 0 | 0 | 1 |

Each finding is counted once under its primary area.

### Findings

- **Error cases / Security, Critical.** The real atomic-write API forces `0600`, writes beside the
  symlink and maps a concurrency abort to exit 4. Adjusted by user decision: new
  `store::write_atomic_file` with mode and a d-mcp `Result` seam.
- **Architecture / Testability, Critical.** Partial `sync` outcomes had no envelope
  representation. Adjusted by user decision: `data` also on a non-zero exit, with an aggregate
  error code.
- **Data protection / Error cases, Critical.** Pending records lost ownership on a failed
  persist. Adjusted: previous owned hashes kept and restored.
- **Security / Data protection, Critical.** Child stderr could echo foreign secrets. Adjusted:
  child output is never printed.
- **Testability / Scope, Important.** The override allow-list collided with WP1's valid parse
  fixtures. Adjusted by user decision: separate `validate` step, own override fixtures.
- **Architecture, Important.** Override results could not flow into the closed resolver problem
  list. Adjusted: invalid overrides become `FileProblem`s with new rule codes; `resolve` validates
  too.
- **Security / Error cases, Important.** The allow-list checked keys only. Adjusted: per-key types
  on the spanned node kind.
- **Architecture, Important.** `Environment` lacked `CODEX_HOME` and `PATH`. Adjusted.
- **Data protection / Maintainability, Important.** Hash names and number canonicalization
  diverged from WP1 without saying so. Adjusted: `definition_hash`, a new tagged entry hash, a
  shared canonical helper, the deliberate difference documented.
- **Architecture / Maintainability, Important.** Affected files omitted the WP1 files WP2 changes,
  and the stale `toml_edit` addition. Adjusted.
- **Error cases, Important.** Dry-run and lock behaviour of `sync` and `unmanage` were unspecified.
  Adjusted.
- **Security, Important.** Hashes of foreign entries were printed. Adjusted: only owned-entry
  hashes until WP4.
- **Error cases / Scope, Important.** `not-offered` had no operation. Adjusted by user decision:
  Held.
- **Architecture, Important.** Take-over and identical collision were underspecified. Adjusted:
  both hashes set, comparability as an adapter capability.
- **Error cases, Important.** `--relocate` was undefined. Adjusted.
- **Error cases, Important.** The process timeout was not feasible without `unsafe`. Adjusted:
  `try_wait` polling, reader threads, documented grandchild limit.
- **Error cases, Important.** The exit-code mapping was incomplete. Adjusted.
- **Scope / Error cases, Important.** `unmanage` could produce an invalid selection. Adjusted by
  user decision: `all` becomes an explicit list.
- **Error cases, Important.** TOML structure edge cases (inline/dotted roots, comment decor, CRLF,
  FIFO) were missing. Adjusted.
- **Testability, Important.** Test isolation did not control `PATH`. Adjusted.
- **Scope, Note.** The login state had no WP2 consumer. Adjusted by user decision: capability and
  tests only, no call during `sync`.
- **Security, Note.** Symlink retargeting between read and persist. Adjusted: re-resolved in the
  persist seam.
- **Testability, Note.** The `important`-gap path had no trigger. Adjusted: planner test adapter;
  Codex note gaps and reserved names declared.
- **Maintainability, Note.** The record time format was unspecified. Adjusted: epoch seconds,
  `state.toml` documented.
- **Scope, Note.** Stale statements (global flags, WP1 plan path, coverage command, the Vanished
  hint). Adjusted.

**Round 1 (2026-10-26, before WP1 existed).** 1 Critical, 16 Important and 8 Note findings, all
adjusted: no on-disk backup (user decision); pending intent records; `apply(harness_plan)` with an
apply mode; gaps from rendering; strict output rules; symlinks and modes; Codex skipped when
absent; managed region and comments; hash canonicalization; `client_secret_env` as a preserved key
(user decision); verified OAuth mapping; WP2 owns `src/process.rs`; `unmanage` drops the harness
from the selection; one take-over operation; acceptance criteria for login state, OAuth and exit
codes; excluded override keys justified; Location changed; Vanished re-add with notice; separate
`validate` step; project read moved to WP4; two re-planning gates; the record stores the sent hash
(from the WP3 review).

## Test results

**Date:** 2026-10-27

- `mise run check`: passed. `cargo fmt --check` and clippy with `-D warnings` are clean, and all
  233 tests pass: 133 unit tests and 100 integration tests (`tests/sync_codex.rs` 43,
  `tests/cli.rs` 30, `tests/acceptance.rs` 9, `tests/json_contract.rs` 7,
  `tests/definitions.rs` 6, `tests/smoke.rs` 4, `tests/resolver.rs` 1).
- `mise run check:full`: passed. Line coverage is 98.31 % against the threshold of 85; regions
  97.52 %, functions 96.97 %. `cargo deny check` reports advisories, bans, licenses and sources
  ok, with the four known warnings about allowed but unused licenses.
- MSRV: `cargo +1.94 check --locked --all-targets` and `cargo +1.94 test --locked` passed.
- `RUSTDOCFLAGS=-D warnings cargo doc --no-deps --locked`, `cargo build --release --locked` and
  `git diff --check` passed.
- Acceptance criteria: each criterion is traced to at least one test, including byte
  preservation on every Codex fixture (comments above and after managed tables, CRLF, no
  trailing newline), one test per sync operation, failure injection for every crash and
  concurrency point, the override fixture matrix, canaries and the JSON contract.
- Codex verification gate passed with `codex-cli 0.165.0`, run only against temporary `HOME`
  and `CODEX_HOME` directories:
  - the field names and canonical forms (`env_vars` in both forms, `startup_timeout_ms`, inline
    `oauth`, `scopes`, `env_http_headers`) are accepted;
  - Codex has no environment form for `oauth.client_secret`, so the preserved-key rule stands;
  - `codex mcp list --json` returns an array of
    `{name, enabled, disabled_reason, transport{type}, startup_timeout_sec, tool_timeout_sec,
    auth_status}`; observed `auth_status` values are `unsupported`, `bearer_token`,
    `not_logged_in` and `unknown`; `o_auth` maps to `logged-in` from the binary's strings, not
    from a live login;
  - Codex re-serializes the whole `mcp_servers` subtree on its own `mcp add` and `mcp remove`
    (reordered keys, `30` as `30.0`, default-valued keys dropped, `oauth.callback_url` added);
    d-mcp's managed entries still plan Unchanged afterwards, and Codex-written bytes outside
    managed entries survive a d-mcp sync;
  - Codex's own listing shows d-mcp's stdio, OAuth, bearer and header servers with the expected
    transport, command or URL;
  - Codex refuses `cwd` and `env_vars` on http entries, an `oauth` table on stdio entries,
    `oauth.client_secret` without `oauth.client_id`, and a timeout of `1e300`.
- Manual smoke test in temporary `D_MCP_STORE` and `CODEX_HOME`: `init`, a definition in
  `local`, `select`, `sync --dry-run --json`, `sync`, a second `sync` (Unchanged), a hand edit
  (Drift, exit 1), `--take-over` (Update) and `unmanage` (`all` became `["claude"]`).
- Not verified locally: the CI run on GitHub, including the macOS test leg.

Implementation notes:

- Byte splicing: `toml_edit` 0.25 re-encoding loses CRLF and a missing trailing newline, so
  d-mcp uses `toml_edit` only to locate each entry's byte region and to render new entries, and
  splices the text into the original bytes. The last table's region ends where the document's
  trailing decor starts. Before writing, d-mcp re-parses the spliced text and checks that every
  entry outside the plan keeps its hash (`harness-edit-failed`, exit 3, nothing written).
- An entry whose sub-tables are split by a foreign table, or that has no header of its own, is
  unsupported, like inline and dotted entries.
- Override sites (TOML kind and position) come from `parse_definition_with_sites` and travel in
  `FileEntry.override_sites`; `Definition` is unchanged.
- `unmanage` of `all` becomes the known harnesses minus the removed one (`["claude"]` in WP2).
- `state-corrupt` and `schema-unsupported` are global errors with `data: null`, because the state
  is shared across harnesses.
- A pending Add may carry previous hashes, so ownership survives a failed Vanished re-add; a
  pending Add without previous hashes is dropped when its entry does not match. A take-over or
  relocation carries its record into the journal as the previous hashes.
- Overrides: `tools` is excluded with the other security keys; a `[harness.codex.env]` table
  fails `override-key`; `cwd` on a non-stdio transport fails `override-transport`; timeouts must
  be finite, non-negative and at most 31536000 seconds; `enabled_tools = []` is rejected,
  because Codex drops it on re-save and that turns "allow no tool" into "allow every tool".
- Entry hashes strip default values (empty `args`, `env`, `env_vars`, `disabled_tools`,
  `enabled = true`, `required = false`), which d-mcp no longer renders either.
- Preserved keys are `oauth.client_secret`, `oauth.callback_url` and `oauth.callback_port`. On
  Update they are kept only while they fit the new rendering (the secret needs
  `oauth.client_id`; the callback keys need an http entry that authenticates through OAuth);
  otherwise they are dropped with a `note` gap naming the key. They are skipped during the
  read walk, so their values never enter a model.
- An identical collision blocks (exit 1) until `--take-over`.
- `--harness` on `sync` and `unmanage` accepts only `codex` in WP2; `claude` is a usage error
  (exit 2).
- New error codes: `harness-file-changed`, `harness-config-invalid`, `harness-verify-failed`,
  `harness-edit-failed`, `state-corrupt` (exit 3), `harness-path-not-utf8` (exit 4),
  `sync-blocked`, `not-managed`, `unknown-take-over` (exit 1). New rule codes:
  `override-key`, `override-type`, `override-transport`.
- Atomic replacement does not keep a foreign owner, group, xattrs or ACLs; a changed owner
  produces a warning.
- The process runner kills only the direct child on timeout; reader threads are not joined.
  The fake `codex` uses only shell builtins plus an absolute `sleep`.
- Test seams (`Hooks`, `SyncHooks`) are `#[doc(hidden)]`; `src/sync/run.rs` is split into
  `executor`, `report` and `unmanage`.
- Codex 0.165.0 was installed for the gate with `npm install -g --prefix ~/.local
  @openai/codex`, by user decision.

## Review findings

**Date:** 2026-10-27
**Reviewer:** effective-flow-rust-reviewer (Rust bucket, three rounds); effective-flow-code-validator (tooling and documentation buckets)

### Summary

| Status | Count |
|---|---:|
| Fixed | 17 |
| Open / Not implemented | 0 |

Two further observations were closed without follow-up work here, because the re-planning
gates of later work packages own them: the child process inherits the working directory and
process `PATH` (relevant once WP4 calls the login-state capability), and the `HarnessCli` side
of the adapter contract is not exercised yet (WP3).

## Open points

- No open points.
