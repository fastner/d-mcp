# WP4: Discovery, adoption, redaction, status and doctor

**Plan status:** Implemented
**Source:** effective-flow plan
**Recommended workflow:** Feature (`effective-flow build`)

## Requirement

Work package 4 of the concept `docs/concept/2026-10-26-d-mcp.md`. It lets a user safely take
over an existing setup and makes the state visible:

- an inventory across all discoverable scopes of both harnesses, with duplicates, conflicts and
  drift;
- preview-first, lossless adoption that replaces every env and header literal with a variable
  reference;
- one redaction layer for all entry content in output;
- status and doctor, covering:
  - login status;
  - the warning for static tokens where OAuth is possible;
  - the missing-variable check with its GUI caveat;
  - harness version, shadowing and recurring-vanish warnings;
- uninstall.

The workflow is Feature.

**Dependencies.** WP4 builds on WP1b (definitions, validation, resolver), WP2 (adapter contract,
write state, sync and take-over, Codex adapter) and WP3 (Claude Code adapter), all merged. WP6
builds on the JSON documents defined here. WP5 (Git sources) is not implemented: adoption into a
personal Git source is out of scope here and belongs to WP5's own re-planning gate.

**Context.** Re-planned on 2026-10-29 against `origin/main` a28521e. Module, type and function
names below refer to the real code. The concept still carries the old uninstall wording and
omits `claude mcp login`; it is corrected in a separate concept review (see Assumptions).

**Facts taken over from WP3's verification spike:**

- The Claude Code config file is `Environment::claude_config_file()`: the legacy
  `.config.json`, else `$CLAUDE_CONFIG_DIR/.claude.json`, else `$HOME/.claude.json`.
- `claude mcp remove` clears the stored OAuth tokens and the client secret of that entry.
- An unset `${VAR}` reaches a Claude Code server as literal text, so d-mcp renders forwards as
  `${VAR:-}`.
- The credential-name list (`is_credential_name`) deliberately over-blocks by prefix and suffix
  (for example `AWS_*`, `*_PROXY`); this stays by decision.
- `claude mcp login` exists.

## Architecture decisions

### Adapter contract extensions

- **`Scope` stays the write scope and the state key** (`src/adapter/mod.rs`, only `User`). v1
  still writes user scope only.
- **Discovery type.** A new `DiscoveredScope { User, Local { project: PathBuf }, Project { root:
  PathBuf } }` and an adapter method `discover(env, repo_root) -> Vec<ScopeRead>`. Each
  `ScopeRead` holds its `Location`, its entries and a per-scope error. One unparsable file is
  reported for its scope and never aborts the inventory.
- **Import.** `import(&ScopeRead, name) -> Result<Imported, Vec<ImportGap>>` replaces the current
  `import(&HarnessRead, &str) -> Option<Definition>`. `Imported` holds the candidate definition
  and the list of replaced fields and normalizations. `ImportGap` holds field path and reason,
  names only.
- **Raw values** go into a `RawEntry(serde_json::Value)` newtype with a names-only `Debug`, no
  `Serialize` and no `Display`, the same pattern as `Parsed` (`src/adapter/claude/file.rs`) and
  `PrivateRead` (`src/adapter/mod.rs`). Pre-validation candidates never use `Definition`'s derived
  `Debug`.
- **Codex login state** runs `codex mcp list --json` from a neutral directory (store root or a
  temp dir), like Claude Code. Today the invocation has no `current_dir`; WP2 deferred this to
  WP4.

### Discovered scopes

- **Claude Code:**
  - user scope from `claude_config_file()`, values read as today;
  - local scope: **names and project paths only** (user decision, ADR 0005 unchanged). Local
    entries are listed, never hashed, compared or adopted. A local entry whose project path no
    longer exists is listed as orphaned;
  - project scope: `.mcp.json` of the current repository, values read through `RawEntry`.
- **Codex:** user scope from `$CODEX_HOME/config.toml`, and project scope
  (`.codex/config.toml` of the current repository, with its trust level).
- **Current repository:** the nearest ancestor of the working directory that contains `.git`
  (directory or file), falling back to the working directory. No `git` process.
- **Repository files** are read through the size-capped, no-follow open used for definitions and
  `ReadOnlyFile`.
- **Unverified facts** (Codex 0.165 project-scope loading and its trust-level key; Claude
  `.mcp.json` approval keys) are checked in a verification gate at the start of implementation
  (see Acceptance criteria). Until verified, the corresponding trust is reported as
  `unknown-trust`.

### Inventory model

- **Class**, one of `managed`, `drifted`, `unmanaged`, `vanished`, `pending`,
  `location-changed`, plus `local` for names-only local-scope entries. Owned classes are derived
  from a read-only `plan_harness` (`src/sync.rs`), not from a second classifier.
- **`shadowed_in: [{scope, path}]`** is a separate attribute, so a managed or drifted user entry
  can also be shadowed. Sources: Claude local scope (from `HarnessRead.shadowed`), Claude project
  `.mcp.json`, and Codex project scope.
- **Duplicate:** the same name in two value-read scopes with an equal `core_hash` of the imported
  definition. **Conflict:** the same name with differing content. `core_hash` of an unmanaged
  import is never output, only the verdict.
- `foreign-owner` (plugin and enterprise configs) is **not** part of v1.

### Redaction

- **Existing invariants stay and are not re-implemented:** child stdout is parsed only and child
  stderr is drained and dropped (`src/process.rs`); WP2/WP3 reports carry no field values
  (`src/sync/report.rs`); parser errors carry only file, line and column.
- **New redacted view (allow-list, default deny)** for every place WP4 renders entry content:
  - **visible:** entry name, transport kind, URL scheme, host and port, env, header and variable
    *names*, auth kind, override *keys*, presence of `oauth.clientId` (not its value), and project
    paths (sync shadowing notices already print them);
  - **redacted:** `command`, `args`, `cwd`, URL user info, path, query and fragment, env and header
    values, `description`, `client_id`, `scopes`, override values, and anything not listed.
- The canary suite covers the existing commands as well as the new ones.

### Hashes

- **Display hashes** of unmanaged and taken-over entries are keyed: HMAC-SHA-256 over the
  adapter's `entry_hash`, with a random 32-byte key in `<store>/.hash-key` (mode `0600`).
  Hashes therefore cannot be used to guess low-entropy secrets.
- **Key creation (user decision):** `init` creates the key, and any mutating command creates it
  under the store lock if it is missing. Read-only commands never create it; without a key they
  show no display hashes and hint `d-mcp init`.
- `.hash-key` is added to `KNOWN_ENTRIES` (`src/store.rs`) and to the store layout docs.
- **Plain hashes** are shown only for records with `written = true`. A record created by
  `--take-over` (`written` absent) holds the foreign entry's hash and is shown keyed. JSON uses
  distinct fields: `hash` (owned, plain) and `display_hash` (keyed).
- **Dependencies:** `hmac` (RustCrypto, alongside `sha2`) and `getrandom`, checked by
  `cargo deny`.

### Adoption: preview first, lossless, literal-free

1. **Preview.** `adopt <harness>:<scope>:<name>...` without `--apply` is read-only (no lock, no
   write). It shows:
   - the definition d-mcp would write;
   - a per-harness, redacted before/after diff of every entry that would be taken over;
   - per entry the `display_hash`, and `write: "none" | "update"` (an identical rendering is
     taken over without a write);
   - the list of normalizations by field.

   `--apply --dry-run` equals the preview.
2. **Every env and header literal becomes a reference (user decision).**
   - Env literals become forwards of the same key name.
   - A literal bearer header becomes `bearer_env`; another single literal header becomes
     `header_env`.
   - `--keep-literal <NAME>` applies to env entries only, and only if WP1b's
     `secret_value_heuristic` passes on the value. Header values are always references.
   - Codex preserved keys (`oauth.client_secret`, `callback_url`, `callback_port`) are not literals
     to replace; they stay harness-local (ADR 0005).
3. **Variable names.** Env literals keep their key name. Only header fields get proposals:
   `<SERVER>_TOKEN` for bearer, `<SERVER>_<HEADER>` otherwise (upper case, `-` turned into `_`).
   A proposal that fails `is_valid_variable_name`, or `is_credential_name` for an http target, is
   refused with the field named and needs `--var <field>=<NAME>`. `--var` also overrides any
   proposal.
4. **Validation and refusals.** The candidate is serialized to TOML and must pass
   `parse_definition` and `validate_definition` unchanged. Any rule code is the refusal reason,
   with the field path but never the value; this covers `variable-syntax`, `url-scheme`,
   `url-query`, `url-userinfo`, `name-rule` and `command-rule`. A native name that breaks the name
   rule is refused with `name-rule`; there is no renaming. Further refusals:
   - more than one header, or one header plus OAuth (`Auth` carries exactly one header);
   - any native key outside the adapter mapping, including the deliberately excluded keys
     (`http_headers_helper`, approval modes, `experimental_environment`, `headersHelper`);
   - an entry with `supported_style = false` (inline or dotted Codex tables);
   - any `important` gap from `render` for a target harness (for example a credential name, a
     reserved name, or `enabled_tools` for Claude Code);
   - `args`, `cwd` or a URL part failing `secret_value_heuristic`;
   - a renamed forward (`${OTHER}` under key `NAME`), a non-empty `${VAR:-default}`, `type: sse`;
   - a failed round trip (below);
   - a local-scope origin (names only, see Discovered scopes).
5. **Lossless round trip.** Import treats `${NAME}` and `${NAME:-}` under key `NAME` as a
   forward. The check compares the adapter's `entry_hash` of `render(imported)` with the actual
   entry after applying a declared normalization list: the replaced literals, plus
   `${VAR}`→`${VAR:-}` for Claude Code. Every normalization appears in the preview.
6. **Target source: `local` only (user decision).** There is no `--source` flag. An existing
   `local/servers/<name>.toml` is refused. Before apply, d-mcp resolves with the new definition in
   memory and refuses unless the adopted definition wins. Writing uses a new definition
   serializer plus `store::write_atomic` into `local/servers/`; WP1 has no source write API.
7. **Selection.** The selection is an explicit harness list: exactly the harnesses whose entries
   are adopted. An existing selection of the name is refused unless its harness set equals that
   list.
8. **Scope widening.** A project-scope origin needs `--to-user-scope`, and relative paths are
   refused. The origin entry is left untouched, because v1 writes user scope only. The preview and
   the apply report state that the origin entry still holds its literals and shadows the adopted
   user entry in that project; doctor reports this as `warn`.
9. **Cross-harness conflicts** need `--from <harness>:<scope>`. The other harness's entry then
   takes the chosen core, and the preview shows that diff. Duplicates merge the override tables of
   both origins.
10. **OAuth.**
    - `--prefer-oauth` sets `auth = oauth`. `--oauth-supported` keeps `bearer_env` and sets
      `oauth_supported = true`.
    - **Claude Code OAuth entries (user decision).** A take-over of an OAuth-capable Claude Code
      entry (`uses_oauth`) that needs a write (`write: "update"`, remove plus add) is refused
      unless `--accept-relogin` is given, because `remove` clears the stored tokens and the client
      secret. If the entry has `oauth.clientId`, `--client-secret-env <NAME>` is also required, and
      that variable is part of the variable gate. d-mcp never proposes `client_secret_env` on its
      own, because it cannot see whether a secret is stored.

### Apply as a scoped sync

- **Binding to the preview (user decision).** `adopt --apply` requires
  `--expect <harness>:<scope>:<name>=<display-hash>` for every entry to be taken over. WP6 passes
  the values through from the preview.
- **Sync executor extensions** (`src/sync.rs`, `src/sync/executor.rs`):
  - an entry point that runs under a caller-held `StoreLock` (today `sync()` takes its own lock,
    and a second lock in the same process fails with `store-locked`);
  - `only: BTreeSet<String>`, limiting planning and writes to the adopted servers. Other servers
    are not planned, not reported, and their records stay unchanged;
  - `take_over: BTreeMap<String, ExpectedHash>`. An entry whose actual hash differs from the
    expected one is refused with the new error `adopt-entry-changed` (exit 3) before any
    journaling. The CLI `sync --take-over` keeps its by-name semantics.
- **Order under the lock:**
  1. re-read the harnesses and check the expected hashes, the variable gate, CLI presence, render
     gaps and resolution against an in-memory snapshot that includes the candidate;
  2. only then write the definition, then the selection;
  3. take over and sync the adopted entries.

  A failure after step 2 leaves definition and selection in place and is reported per entry with
  sync's outcomes. A Claude Code `lost` entry is re-added in reference form as `vanished` on the
  next sync.
- **Variable gate (user decision).** Before any take-over, every variable that replaces a literal
  must be set, non-empty, and **equal to the replaced literal** in the current shell. The
  comparison happens in memory and is never printed; a mismatch is refused, naming the variable.
  `--allow-env-mismatch` overrides the equality check, and `--allow-missing-env` overrides
  presence. Both flags state in their output that the original literal is unrecoverable after
  apply. For Claude Code, rollback is impossible for adopted entries, because the original literal
  was never stored. A failure after `remove` names the server and the variables the user must
  restore.
- There are no persisted take-over grants.

### Status

Per harness and selected server: desired, recorded and actual state, the pending operation
(WP2's plan, read-only), missing variables, and with `--auth` the login state. `--auth` warns
that Claude Code's `get` connects to servers.

- Missing variables come from `Definition::forwarded_env()`, which includes the `bearer_env`,
  `header_env` and `client_secret_env` variables.
- Status checks only the current shell; a set-but-empty variable counts as missing. The output
  always states that GUI-launched harnesses may see a different environment.
- `inventory` and `status` exit 0 even with per-scope parse errors, which are reported as data.

### Doctor

Each check has severity `ok`, `warn` or `error` and a hint. `doctor` exits 1 when any check is
`error` (user decision); `warn` keeps exit 0. Corrupt or uninitialized store files are reported as
`error` checks instead of aborting.

- store integrity and schema version;
- presence and version of `claude` and `codex`: `--version` through `ProcessRunner` from a neutral
  directory, with `DISABLE_AUTOUPDATER=1` and the normal timeout, compared exactly with
  `VERIFIED_VERSIONS` (`2.1.310 (Claude Code)`, `codex-cli 0.165.0`); a mismatch is `warn`;
- drift;
- recurring vanishes: `warn` when a record's `vanish_count` is 2 or more;
- shadowing, including adopted entries whose project-scope origin still shadows them;
- static tokens where OAuth is possible (`bearer_env` with `oauth_supported`);
- the note that the credential-name list may be incomplete;
- an informational check that managed-policy deny lists can block user servers and that d-mcp
  cannot see them (promised by WP3);
- names of unmanaged inventory entries longer than WP1's name limit.

`doctor --auth` adds the login-state query.

### Uninstall

- `uninstall` without `--apply` is read-only; `--apply` acts; `--apply --dry-run` equals the
  preview.
- It first settles `pending` records like sync does.
- Owned entries that d-mcp created, whose actual hash matches their record, are removed through
  the scoped sync (Remove).
- **Adopted entries (user decision): ownership is dropped, nothing is written.** The current
  d-mcp rendering stays in the harness, through `sync::unmanage`. The output lists the replaced
  variables the entry depends on.
- Drifted entries are left and reported, and keep their records unless `--purge` is given; exit 1.
- `location-changed` records and a missing `claude` (`harness-cli-not-found`, exit 4) are reported
  and block `--purge`.
- `--purge` deletes only `KNOWN_ENTRIES` (including `.hash-key`) inside the resolved store root. It
  never follows into registered sources. For a symlinked store root it deletes the link target's
  contents only after an explicit warning.

### JSON contract for WP6

- The documents for `inventory`, `adopt`, `status`, `doctor` and `uninstall` are specified in
  `docs/reference.md`, with their class and severity enums and login-state values.
- There are no per-document versions: additive changes stay under envelope `schema_version` 1.
- `adopt`, `uninstall` and `doctor` carry `data` on non-zero exits, like `sync`.
- Tests check real output against these shapes, including enum values.

### Exit codes

- 0 complete;
- 1 `adopt-refused`, an uninstall that left drifted entries, or a doctor check at `error`;
- 3 `adopt-entry-changed` (an entry changed between preview and apply);
- 4 `harness-cli-not-found`.

Precedence across harnesses is 3, 4, 1, 0. New error codes go into the error table in
`src/error.rs` and `docs/reference.md`.

### Write state

New record keys `adopted_from`, `replaced_fields`, `vanish_count` and `last_vanish` are added to
`RECORD_KEYS`. `schema_version` stays 1 and there is no upgrade step, per the existing convention
(`docs/reference.md`): an older d-mcp refuses the file as `state-corrupt`. `vanish_count` is
incremented when a `vanished` re-add is promoted in `settle`, and is never reset.

## Affected files

| File | Description |
|---|---|
| `src/adapter/mod.rs` | `DiscoveredScope`, `ScopeRead`, `discover`, new `import` signature, `RawEntry`, `Imported`, `ImportGap` |
| `src/adapter/codex.rs` | Import with lossless-ness report; project-scope discovery with trust level; login state from a neutral directory |
| `src/adapter/claude.rs`, `src/adapter/claude/file.rs` | Import with normalizations; project `.mcp.json` discovery; local-scope names and project paths |
| `src/sync.rs`, `src/sync/executor.rs` | Caller-held lock entry point, `only` filter, expected-hash take-over, vanish counter in `settle` |
| `src/state.rs` | New record keys in `RECORD_KEYS` |
| `src/store.rs` | `.hash-key` in `KNOWN_ENTRIES`; key creation in `init` and under the lock |
| `src/inventory.rs` | Discovery across adapters and scopes, classification, duplicates and conflicts |
| `src/redact.rs` | Allow-list redacted view for entry content |
| `src/hashkey.rs` | Per-store HMAC key and display hashes |
| `src/definition/` (serializer) | Serialize an imported definition to TOML for `local/servers/` |
| `src/adopt.rs` | Preview, literal replacement, variable naming, refusals, round-trip check, scoped apply, variable gate |
| `src/status.rs` | Status, login-state query, missing-variable check |
| `src/doctor.rs` | Doctor checks |
| `src/uninstall.rs` | Uninstall preview, apply and purge |
| `src/error.rs` | `adopt-refused`, `adopt-entry-changed` and their exits |
| `src/cli.rs` | `inventory`, `adopt`, `status`, `doctor`, `uninstall` |
| `Cargo.toml`, `deny.toml` | `hmac`, `getrandom` |
| `tests/redaction.rs` | Canary suite over every command and output format, existing commands included |
| `tests/adoption.rs`, `tests/status_doctor.rs`, `tests/uninstall.rs` | Integration tests |
| `tests/json_contract.rs` | Extend with the new documents and enum-value assertions |
| `tests/support/fake_claude.rs`, `tests/support/fake-codex.sh` | Echo input to stderr; `--version`, `get`, `mcp list --json` |
| `tests/fixtures/real-setup/` | Combined fixture with at least five servers across both harnesses. Canaries in env, headers, `args`, `command`, `cwd`, URL query, user info and path, `description`, and on a broken TOML line. Also duplicates, conflicts, local-scope entries, project-scope entries, an OAuth entry with `oauth.clientId`, and inexpressible entries |
| `docs/reference.md` | Inventory classes, adoption rules, JSON documents, status and doctor checks, uninstall semantics, commands table, store layout, error table |

## Implementation details

### Approach

1. **Verification gate.** Verify Codex 0.165 project-scope loading and its trust-level key, and
   the Claude `.mcp.json` approval keys, against isolated temp `HOME`, `CODEX_HOME` and
   `CLAUDE_CONFIG_DIR`. Record the outcomes in the plan.
2. **Redacted view and canary suite** over the existing commands.
3. **Adapter contract extensions**: discovery types, `RawEntry`, the new `import` for both
   adapters, Codex login state from a neutral directory.
4. **Hash key and display hashes**, including `init` and `KNOWN_ENTRIES`.
5. **Inventory**: discovery, classification via `plan_harness`, `shadowed_in`, duplicates and
   conflicts.
6. **Sync executor extensions**: caller-held lock, `only`, expected-hash take-over, vanish
   counter.
7. **Adoption**: preview, literal replacement, variable naming, validation and refusals, round
   trip, OAuth flags, the definition serializer, then the scoped apply with `--expect` and the
   variable gate.
8. **Status and doctor.**
9. **Uninstall.**
10. **JSON contract** in `docs/reference.md`, plus contract tests.
11. **Redaction suite** over every command, text and JSON, with all canary positions. Assert that
    no canary appears in stdout, stderr, any store file or any file of `local/`.

### Component structure

`inventory`, `status`, `doctor` and `uninstall` depend on the adapter contract and the write
state. `adopt` writes the definition through the new serializer and `store::write_atomic`, the
selection through the existing config write, and harness entries only through the scoped sync.
Every rendering of entry content goes through `redact`.

### State management

- New record keys, `schema_version` unchanged (see Write state).
- New store file `.hash-key`.

### API integration

None beyond WP2 and WP3: login state through the adapter capabilities.

### Styling approach

Not relevant: a command-line tool.

### Accessibility

Plain-text output that does not depend on colour.

### Edge cases

- The same server in Claude Code and Codex with different args: a conflict; `--from` is
  required.
- A literal the heuristic considers harmless: still replaced, unless `--keep-literal` is given
  (env only).
- An inexpressible or lossy entry: refused, naming the field.
- A credential in `args`, `cwd` or the URL: refused, naming the field.
- An entry changes between preview and apply: `adopt-entry-changed`, exit 3.
- A variable is missing or differs from the literal at apply: no take-over, unless
  `--allow-missing-env` or `--allow-env-mismatch` is given.
- An OAuth-capable Claude Code entry that needs a rewrite: refused without `--accept-relogin`;
  with `oauth.clientId` also without `--client-secret-env`.
- Claude Code `add-json` fails after `remove` during a take-over: the server and its variables are
  named in the report.
- A local-scope entry: listed, never adopted; orphaned when its project path no longer exists.
- A higher-priority source offers the same name: adoption is refused.
- `local/servers/<name>.toml` already exists: adoption is refused.
- A proposed variable name is invalid or a credential name: refused, `--var` required.
- A read-only command without `.hash-key`: no display hashes, hint `d-mcp init`.
- Uninstall with drifted entries: those are left; exit 1.
- `--auth` without `claude` installed: the Claude Code login state is `unknown`.

## Acceptance criteria

- [x] The verification gate's outcomes (Codex project scope and trust key, Claude `.mcp.json`
      approval keys) are recorded in this plan before the dependent code is written.
- [x] `mise run check:full` passes (coverage at or above `coverage-threshold`, `cargo deny`).
- [x] On the `real-setup` fixture, adopt plus sync changes nothing outside the adopted entries.
      Within each adopted entry, the semantic diff contains only the replaced fields and the
      declared normalizations; with `--from`, the non-chosen harness's entry takes the chosen
      core, as shown in the preview.
- [x] The redaction suite passes. No canary appears in any stdout, stderr, JSON document, store
      file or file under `local/`, for any command, existing ones included.
- [x] Inventory tests classify every fixture entry, including `shadowed_in` and `local`, report
      the fixture's duplicates and conflicts, and show that one unparsable file does not abort the
      inventory.
- [x] Adoption tests cover:
      - literal replacement in env, bearer and other headers;
      - `--keep-literal` (env only) and `--var`;
      - variable names valid for both adapters, and refused proposals;
      - each refusal reason;
      - `--to-user-scope` and `--from`;
      - `--prefer-oauth`, `--oauth-supported`, `--accept-relogin` and `--client-secret-env`;
      - `--expect` mismatch with exit 3;
      - the variable gate for missing and differing values, with both override flags;
      - the Claude Code failure-after-remove report;
      - a failure after the definition write, reported per entry.
- [x] Status and doctor tests trigger every check at least once (including `vanish_count` ≥ 2 and
      doctor exit 1 on `error`) and use fake CLIs for `--auth` and `--version`.
- [x] Uninstall tests cover:
      - owned entries are removed;
      - adopted entries stay in the harness, unowned;
      - drifted entries are left with exit 1;
      - `--purge` deletes only known store entries and is blocked by `location-changed` records.
- [x] The JSON output of each command matches the shapes and enum values documented in
      `docs/reference.md`.

## Validation plan

- Integration tests on the combined fixture with the fake `claude`, the fake `codex` and Codex
  config fixtures.
- The redaction suite as its own CI test target.
- Manual check on the developer's real setup: preview only (`inventory`, `status`, `adopt`
  without `--apply`). `--apply` and `uninstall --apply` run only against copies of the harness
  files under a temporary `HOME`, `CODEX_HOME` and `CLAUDE_CONFIG_DIR`, with no OAuth server logged
  in; the semantic diffs are compared there.

## Assumptions and open points

- `foreign-owner` discovery (plugin and enterprise MCP configs) is deferred until their
  locations are verified.
- The concept's uninstall wording and its missing `claude mcp login` are corrected in a separate
  concept review (`effective-flow review docs/concept/2026-10-26-d-mcp.md`), with this plan's
  decisions as input. It does not block WP4.

## Plan review

**Result:** Approved

### Summary

Round 2, re-planning against `origin/main` a28521e (2026-10-29):

| Area | Critical | Important | Note |
|---|---:|---:|---:|
| Architecture | 2 | 3 | 0 |
| Security | 1 | 4 | 1 |
| Data protection | 1 | 3 | 0 |
| Error cases | 0 | 5 | 1 |
| Testability | 0 | 2 | 1 |
| Scope | 0 | 1 | 2 |
| Maintainability | 0 | 3 | 3 |

### Findings

Round 2 (re-planning):

- **Architecture, Critical.** The scoped apply could not be built on `sync()`: it takes its own
  lock, has no server filter, and take-over accepts any actual hash. Adjusted: caller-held lock
  entry point, `only` filter, expected-hash take-over with `adopt-entry-changed`.
- **Architecture, Critical.** Uninstall promised to restore adopted entries from a hash. Adjusted
  by user decision: adopted entries are only released through `unmanage`.
- **Data protection, Critical.** Taking over a Claude Code OAuth entry clears its tokens and client
  secret. Adjusted by user decision: refused without `--accept-relogin`; `--client-secret-env`
  required with `oauth.clientId`.
- **Security, Critical.** The manual validation would have rewritten the real harness files.
  Adjusted: preview only on the real setup, apply only against copies under temp roots.
- **Architecture, Important.** The adapter contract could not express multi-scope discovery or a
  lossless import report. Adjusted: `DiscoveredScope`, `ScopeRead`, the new `import`, `RawEntry`.
- **Data protection, Important.** Local-scope values are deliberately unread. Adjusted by user
  decision: names and project paths only, not adoptable.
- **Maintainability, Important.** The schema bump contradicted the convention. Adjusted: new keys,
  `schema_version` stays 1.
- **Security, Important.** The literal mapping ignored the single-header auth model and the
  existing validator. Adjusted: refusals via `validate_definition`, header and allow-list rules,
  preserved Codex keys.
- **Error cases, Important.** "Lossless" was undefined for Claude Code forwards. Adjusted: hash
  comparison with a declared normalization list.
- **Error cases, Important.** The variable gate only checked presence. Adjusted by user decision:
  value equality, `--allow-env-mismatch`.
- **Architecture, Important.** Nothing bound the preview to the apply. Adjusted by user decision:
  `--expect` with display hashes.
- **Data protection, Important.** The `.hash-key` lifecycle, layout and dependencies were unplanned,
  and taken-over hashes leaked in plain form. Adjusted by user decision: key from `init` and
  mutating commands; plus `KNOWN_ENTRIES`, dependencies, and the `hash`/`display_hash` split.
- **Security, Important.** The redaction layer duplicated existing invariants and left positions
  open. Adjusted: allow-list view; existing invariants kept.
- **Scope, Important.** The selection scope and `--from` contradicted the acceptance criterion.
  Adjusted: explicit harness list and an amended criterion.
- **Error cases, Important.** Write ordering and partial failure in the apply were undefined.
  Adjusted: check-then-write order, per-entry reporting, a definition serializer.
- **Security, Important.** `--source` allowed writes into arbitrary directories. Adjusted by user
  decision: `local` only.
- **Security, Important.** The Codex login state inherited d-mcp's working directory. Adjusted:
  neutral directory.
- **Maintainability, Important.** The JSON contract and exit codes conflicted with the envelope.
  Adjusted: envelope v1, `data` on non-zero exits, new error codes, precedence; doctor exit 1 on
  `error` by user decision.
- **Architecture, Important.** `shadowed` as an exclusive class hid ownership state. Adjusted:
  `shadowed_in` attribute, classes from `plan_harness`.
- **Data protection, Important.** A project-scope origin keeps its literals and shadows the adopted
  entry. Adjusted: preview, report and a doctor warning.
- **Error cases, Important.** Uninstall error cases were missing. Adjusted: pending settlement,
  `location-changed`, missing CLI, purge limits.
- **Testability, Important.** Discovery facts conflicted with WP3 or were unverified. Adjusted:
  real paths and a verification gate.
- **Testability, Important.** The vanish counter was underspecified. Adjusted by user decision:
  counter in `settle`, `warn` at 2.
- **Error cases, Important.** Variable-name proposals could be invalid or blocked. Adjusted:
  refusal with `--var`, proposals for headers only.
- **Maintainability, Important.** The preview convention diverged from `--dry-run`. Adjusted:
  read-only without `--apply`, `--apply --dry-run` equals the preview.
- **Testability, Note.** The acceptance criterion named the wrong task. Adjusted:
  `mise run check:full`.
- **Maintainability, Note.** Affected files were inaccurate. Adjusted.
- **Maintainability, Note.** The context still called all names provisional. Adjusted.
- **Error cases, Note.** The doctor version check was unspecified. Adjusted.
- **Scope, Note.** The managed-policy note promised by WP3 was missing. Adjusted.
- **Security, Note.** Pre-validation candidates could use `Definition`'s derived `Debug`.
  Adjusted: `RawEntry` and `Imported`.
- **Scope, Note.** The WP5 dependency was open. Adjusted: out of scope.
- **Maintainability, Note.** The missing-variable source was unnamed. Adjusted:
  `Definition::forwarded_env()`.

Round 1 (2026-10-26, before any product code; superseded where round 2 changed it): secrets in
`args` and URLs, replacement of every literal, the Claude Code take-over loss, redaction
positions, the native-to-neutral mapping, the lossless requirement, `foreign-owner` deferral, the
target source, scope widening, variable-name validation, the uninstall deviation, the tool-name
warning, the JSON contract, `doctor --auth`, the vanish counter, keyed display hashes, scoped sync
instead of grants, `--oauth-supported`, exit codes, and the re-planning gate.

Both earlier open points are resolved. The re-planning gate is closed by this round. The concept
update no longer blocks and moved to Assumptions.

## Test results

**Date:** 2026-10-29

- **Verification gate** (run only against temporary `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and
  `D_MCP_STORE` directories, no login, only `/bin/true` as a server):
  - Codex 0.165.0 loads project entries only when the user `config.toml` has
    `[projects."<repository root>"] trust_level = "trusted"`; an absent key or `"untrusted"`
    ignores them, and a key on a subdirectory has no effect. Codex loads every
    `.codex/config.toml` from the repository root down to the working directory, a project
    entry replaces a user entry of the same name, and one broken project file makes
    `codex mcp list` exit 1. d-mcp therefore reads every layer as its own scope.
  - Claude Code (installed version 2.1.312) shows unapproved `.mcp.json` entries as pending.
    Approval lives in `enabledMcpjsonServers`, `disabledMcpjsonServers` and
    `enableAllProjectMcpServers`, in `$CLAUDE_CONFIG_DIR/settings.json` (always honoured) or in
    `<cwd>/.claude/settings.json` and `settings.local.json` (honoured only with
    `projects.<cwd>.hasTrustDialogAccepted = true`). Disabled wins. Legacy approval keys under
    `projects.<path>` are moved by Claude into `settings.local.json`. Trust and approvals are
    keyed on the working directory. Whether `.mcp.json` files in intermediate directories are
    read was not determined; d-mcp reads `<repository root>/.mcp.json` only.
  - The adapter's verified version stays `2.1.310 (Claude Code)`; doctor reports the installed
    2.1.312 as a version `warn` until it is verified.
- `mise run check`: passed. fmt and clippy with `-D warnings` are clean, and all 394 tests pass:
  198 unit tests and 196 integration tests (`tests/sync_codex.rs` 44, `tests/sync_claude.rs` 32,
  `tests/cli.rs` 31, `tests/adoption.rs` 16, `tests/json_contract.rs` 14,
  `tests/status_doctor.rs` 11, `tests/inventory.rs` 10, `tests/uninstall.rs` 10,
  `tests/acceptance.rs` 9, `tests/definitions.rs` 6, `tests/redaction.rs` 5,
  `tests/smoke.rs` 4, `tests/sync_scoped.rs` 3, `tests/resolver.rs` 1).
- `mise run check:full`: passed. Line coverage is 96.44 % against the threshold of 85.
  `cargo deny check` reports advisories, bans, licenses and sources ok, with the four known
  warnings about allowed but unused licenses.
- MSRV `cargo +1.94 check --locked --all-targets --all-features` and
  `RUSTDOCFLAGS=-D warnings cargo doc --locked --no-deps --all-features` passed.
- Test isolation: `cargo test` also passes with `GITHUB_TOKEN`, `CODEX_HOME` and
  `CLAUDE_CONFIG_DIR` set in the developer environment; test commands start from an empty
  environment, and both fake CLIs refuse to run outside a marked temporary test root.
- Acceptance criteria: each criterion is traced to at least one test, including the redaction
  canary suite over every command in text and JSON (values, secret-shaped key, header and
  variable names, URL user info, path and fragment, project-scope files, fake-CLI output), the
  full inventory classification of the `real-setup` fixture, every adoption refusal reason, the
  preview binding, the variable gate across all origins, the scoped sync, every doctor check and
  the uninstall outcomes.
- Not verified locally: the CI run on GitHub, including the macOS test leg, and the manual
  preview against the developer's real setup from the validation plan.

Implementation notes and deviations from the plan, each documented in `docs/reference.md`:

- Codex project scope covers every `.codex/config.toml` layer from the repository root down to the
  working directory, per the verification gate.
- The same-name verdict has a third value, `undetermined`, for entries that cannot be imported;
  approval has an `unknown` value for unreadable settings.
- Adoption adds `write: "add"` for project-scope origins, a normalization `env-vars-sorted`, and
  refusals beyond the planned list (`user-entry-exists`, `harness-not-present`,
  `already-managed`, `duplicate-origin`, `local-unavailable`, `expect-unknown`,
  `variable-mismatch`, `invalid-name`, `client-secret-env-invalid`). `--expect` is required for
  every origin, the variable gate also runs in the preview and covers every origin, and names
  that would display redacted are never carried into a definition.
- Entries taken over with `sync --take-over` and never rewritten are released by uninstall like
  adopted ones. `--purge` is also blocked by failed harness removals and needs
  `--follow-store-link` for a symlinked store root.
- ADR 0005 has a dated amendment listing the reads WP4 adds.

## Review findings

**Date:** 2026-10-29
**Reviewer:** effective-flow-rust-reviewer (three parallel reviews, one re-check)

### Summary

| Status | Count |
|---|---:|
| Fixed | 22 |
| Open / Not implemented | 0 |

## Open points

- No open points.
