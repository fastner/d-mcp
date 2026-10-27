# WP1b: Core model, store and resolver

**Plan status:** Implemented
**Source:** effective-flow plan
**Recommended workflow:** Feature (`effective-flow build`)

## Requirement

Second half of work package 1 of the concept `docs/concept/2026-10-26-d-mcp.md`. By user
decision, WP1 was split into WP1a (scaffold) and WP1b. WP1b builds the harness-independent core
of d-mcp:

- the neutral server definition: core plus override blocks, and the auth method with OAuth as the
  default;
- secret heuristics and name rules;
- the store in `~/.d-mcp` with its lock and a built-in local source;
- local sources and selections;
- pure, deterministic resolution with shadowing and fail-closed holds;
- documented JSON output and dry-run support.

The workflow is Feature.

**Dependencies and re-planning.** WP1b depends on WP1a
(`docs/plan/2026-10-26-wp1a-project-scaffold.md`). Start it only once WP1a has the plan status
`Implemented`. WP1a fully specifies its outputs, so no re-planning is needed. Before starting,
check only that the toolchain version, `rust-version` and dependency policy match what WP1a
delivered, and record any difference in the PR. Every later package (WP2–WP7) builds on the types, store
layout, exit codes and JSON documents defined here. Their re-planning gates refer to this file.

**Context:**

- **Verified:** the repository has no product code. WP1a is expected to provide the crate,
  toolchain, CI and contributor conventions.
- **Verified (Dalo conventions):**
  - std `File::try_lock` with injectable retry delays;
  - atomic writes: temp file, `persist`, then a parent-directory fsync;
  - a `schema_version` in every persisted file;
  - tests with `assert_cmd`, `predicates` and `proptest`.
- **Verified:** since Rust 1.86, `std::env::home_dir` falls back to `getpwuid` when `HOME` is
  unset. d-mcp therefore reads `HOME` directly.

## Architecture decisions

### Store

- **Location `~/.d-mcp` (user decision).** The store path comes from the first of these that is
  set:
  1. the `--store` flag;
  2. the `D_MCP_STORE` environment variable (an empty value counts as unset);
  3. `$HOME/.d-mcp`.

  A leading `~` is expanded in the first two, and relative paths are resolved against the
  current directory. If `HOME` is unset or empty and is needed, that is an environment error
  (exit 4).
- **Layout created by WP1b:**
  - `config.toml`: schema version, user sources (id, kind, absolute canonical path, priority) and
    selections.
  - `local/servers/`: the built-in local source.
  - `.lock`: the advisory lock.

  Directories are created `0700`, files `0600`. WP2 and WP5 create `state.toml`, `lock.toml` and
  `approvals.toml` on first write. Every persisted file carries `schema_version = 1`, following the
  schema rule from WP1a.
- **Built-in local source (user decision).** Its root is `<store>/local` (with `servers/` beneath
  it), its id is `local` and its priority is 0. It is implicit: it is not persisted in
  `config.toml`, and its path is always derived from the active store. It cannot be removed or
  re-prioritized. WP4 adoption writes there by default, and WP5 can treat `<store>/local` as a Git
  root.
- **Locking.**
  - Mutating commands take the lock, then read `config.toml`, then write. Reading only after the
    lock is taken avoids TOCTOU races.
  - Read-only commands (`resolve`, `source list`) and all dry runs take no lock.
  - A mutating command on an uninitialized store fails before it tries to lock.

### Definition format

- **Files and scanning.** A source root contains `servers/<name>.toml`, one server per file.
  - Only regular files are accepted. Symlinks are not followed.
  - Files over 64 KiB are rejected.
  - Hidden files, non-`.toml` files and subdirectories are ignored and reported as notices.
- **Conflict key.** The key is the file stem, as long as the stem passes the name rule. A
  malformed `servers/foo.toml` therefore still shadows `foo`. A stem that breaks the name rule is
  a per-file problem and shadows nothing. The `name` field must equal the stem.
- **Normative format.** The definition format is fixed by these minimal data examples, which the
  test fixtures use verbatim:

  ```toml
  name = "github"
  description = "GitHub tools"
  [transport]
  type = "stdio"
  command = "npx"
  args = ["-y", "@modelcontextprotocol/server-github@1.2.3"]
  [env]
  GITHUB_TOKEN = { forward = true }
  LOG_LEVEL = { value = "info" }
  [harness.codex]
  cwd = "/srv/github"
  ```

  ```toml
  name = "linear"
  [transport]
  type = "http"
  url = "https://mcp.linear.app/mcp"
  [auth]
  type = "oauth"
  client_id = "d-mcp-linear"
  scopes = ["read"]
  client_secret_env = "LINEAR_CLIENT_SECRET"
  ```

  The other `auth` forms are:
  - `type = "bearer_env"` with `var`, and optionally `oauth_supported = true`;
  - `type = "header_env"` with `header` and `var`;
  - `type = "none"`.
- **Defaults.** When omitted:
  - `args` is `[]` and `env` is `{}`;
  - `auth` is `{ type = "oauth" }`;
  - `scopes` is `[]`;
  - `oauth_supported` is `false`;
  - empty override tables are dropped.

  "Explicitly written defaults" means exactly these values.
- **Field rules.**
  - `env` is valid only for `stdio`; `auth` and its fields only for `http`.
  - Unknown keys in any core table are an error, reported with the field path.
  - Only `harness.codex` and `harness.claude` are opaque. WP2 and WP3 add allow-lists for them.
  - The stdio working directory exists only as `harness.codex.cwd`, because Claude Code has no
    equivalent.
  - Variable names (forwards, `var`, `client_secret_env`) match `^[A-Za-z_][A-Za-z0-9_]*$`, and
    header names are RFC 9110 tokens.
  - `client_secret_env` requires `client_id`.
  - `command` must be non-empty and either a bare program name or an absolute path; `~` and
    relative paths are rejected.
- **URL rules (user decisions).**
  - The scheme must be `https`. `http` is allowed only for the loopback hosts `localhost`,
    `127.0.0.0/8` and `::1`.
  - The `url` field must not carry a query.
  - User info is rejected in *any* URL-shaped value, in every checked position.
- **Neutral name rule.**
  - Allowed characters are lowercase letters, digits, `-` and `_`. The name starts with a letter
    or digit, does not end with `-` or `_`, contains no `__`, and has at most 32 characters.
  - The limit follows from a 64-character model tool-name limit minus `mcp__` and `__`, which
    leaves at least 25 characters for tool names. The 64-character limit is an assumption that
    WP3 re-verifies.
  - Source ids follow the same rule. Reserved names are adapter-specific (WP2, WP3).

### Secret checks

- **Secret forms.** An env entry is either a forward (`{ forward = true }`, passed under the same
  name) or a literal (`{ value = "..." }`). Renamed forwards are not modelled. Variable syntax
  (`$VAR`, `${VAR}`, `${env:VAR}`, `{env:VAR}`) is rejected in `command`, `args`, `url`, env
  literal values and override-table strings.
- **Heuristics** (module `credential.rs`). They are named `secret_name_heuristic` and
  `secret_value_heuristic`, so they cannot be confused with WP3's Claude Code credential-name
  list.
  - **Name heuristic.** It splits a name at `_` and `-` and matches when one segment equals one
    of the following, compared case-insensitively:
    - `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD` or `PASS`;
    - `APIKEY`, `CREDENTIAL` or `CREDENTIALS`;
    - `COOKIE` or `AUTH`.

    It also matches when the segment pair `API` `KEY` occurs. It runs on env literal names, on
    `--flag` names in `args`, and on URL query keys.
  - **Value heuristic.** It matches when a value meets any of these:
    - a known prefix: `ghp_`, `gho_`, `ghu_`, `ghs_`, `github_pat_`, `glpat-`, `sk-`, `sk-ant-`,
      `xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`, `AKIA` or `AIza`;
    - a JWT: `eyJ` followed by two `.` separators;
    - at least 32 hex characters, except in digest contexts (`sha256:`, `@sha256:`, and 40- or
      64-character hex used as a commit in a VCS spec). WP5 allows those for pinning.
    - at least 32 characters with Shannon entropy of at least 4.0 bits per Unicode scalar.

    An absolute path or URL is exempt from the entropy rule only as a whole. Its path segments
    and query values are still checked one by one.
  - **Positions checked by the value heuristic:**
    - env literal values;
    - every `args` element, plus the right side of `--flag=value`;
    - URL path segments, and the path segments and query values of URL-shaped arguments;
    - `description`, `client_id` and `scopes`;
    - every string in the override tables.
- **Errors never echo values.** Parsing runs in two phases:
  1. into an untyped, spanned TOML document (`toml_edit`'s immutable document);
  2. validation by d-mcp's own walker, which tracks the field path and maps every failure to a
     rule code.

  Errors show only file, line, column, field path and rule code. No serde or `toml` message text is
  ever printed. `#[serde(flatten)]` is not used for core types.

### Hashes

- **Two hashes over canonical JSON** (sorted keys, no whitespace) of the model after defaults:
  - `definition_hash`: domain tag `d-mcp-definition-v1` plus a newline, then the full model
    including `description` (user decision) and the override tables. WP5 binds approvals to it.
  - `core_hash`: domain tag `d-mcp-core-v1` plus a newline, then the model without
    `description` and without the override tables. WP4 uses it to detect duplicates across
    harnesses.
- **Value encoding in the hash.**
  - `scopes` is sorted and deduplicated.
  - TOML datetimes become RFC 3339 strings.
  - Integers and floats stay distinct. Floats (allowed only in overrides) use the shortest
    round-trip form; NaN and infinity are rejected.

### Sources, selections and resolution

- **Sources.**
  - `source add <path> --id <id> --priority <n>`: `--priority` is required, an unsigned integer
    of at least 1, and must be unique. A path already registered under another id, or a path
    inside the store, is rejected.
  - A user source is `unavailable` when its root or its `servers/` directory is missing or
    unreadable.
- **Selections.**
  - `select <server>` without `--harness` stores `all` literally, so future adapters are included.
  - `--harness claude|codex` can be repeated; duplicates are collapsed.
  - Re-selecting a server replaces its harness list.
  - Selecting a name that no source offers is allowed, with a warning.
  - The scope is always `user`.
- **Resolver: a pure function.** For every selected server it returns exactly one status:
  - `resolved`: the winning definition, its origin source, harnesses and both hashes.
  - `hold` with reason `invalid-winner`: the highest-priority definition is invalid (user
    decision: hold).
  - `hold` with reason `source-unavailable`: an unavailable source has a higher priority than the
    current winner, or no winner exists while a source is unavailable. The resolution could
    therefore change once that source is back (user decision: hold the affected servers).
  - `not-offered`.

  The resolver also lists shadowed definitions, per-file problems and notices. WP2 turns `hold`
  into its *Held* operation (keep the existing managed entry unchanged), and WP5 adds the reason
  `pending-approval`.

### Errors, output and dry runs

- **Exit codes**, documented together with stable error codes in `docs/reference.md`:

  | Code | Meaning | Cases |
  |---|---|---|
  | 0 | Success | Including resolution problems, holds and warnings |
  | 1 | Expected failure | Store not initialized; invalid arguments; rejected `source add`; `init` into a non-empty directory that is not a store |
  | 2 | Usage error | clap usage errors, documented as such |
  | 3 | Unsafe state | Corrupt or hand-broken `config.toml`; unsupported schema version; `StoreLocked` |
  | 4 | Environment problem | For example a missing `HOME` |
- **JSON documents** are specified in `docs/reference.md`:
  - the envelope: `schema_version`, `ok`, `data`, `problems` and `error` (`code`, `hint`);
  - the `data` of every WP1b command;
  - the closed list of problem codes: `invalid-definition`, `invalid-winner`, `not-offered`,
    `source-unavailable`, `shadowed` and `notice`.

  Warnings go to stderr, so stdout always stays valid JSON.
- **Limited `resolve` output until WP4.** It shows name, status, origin source, harnesses,
  transport kind, auth kind, forwarded variable names and hashes. It never shows literal values,
  `args` or override contents.
- **Dry runs** take no lock and create no file or directory. A dry-run `init` on a missing store
  creates nothing.
- **ADRs**, recorded under `docs/adr/` with Dalo's ADR shape. They sit next to the unnumbered
  Effective Flow setup ADR, which stays as it is.
  - 0001: store-centred management;
  - 0002: secret references only;
  - 0003: neutral core plus overrides;
  - 0004: OAuth first.

## Affected files

| File | Description |
|---|---|
| `Cargo.toml`, `Cargo.lock` | Add `serde` (derive) and `serde_json`, `toml_edit`, `sha2` and `tempfile`, `thiserror` 2, plus `proptest` as a dev-dependency |
| `src/cli.rs` | clap definitions, global options (`--store`, `--json`, `--dry-run`), dispatch for `init`, `source add/list/remove`, `select`, `unselect`, `resolve` |
| `src/lib.rs`, `src/main.rs` | Module declarations; exit-code mapping |
| `src/error.rs` | Error enum, exit codes, stable error codes |
| `src/store.rs` | Path resolution, layout and permissions, the lock with injectable delays, atomic writes with a test seam between temp write and persist, schema checks |
| `src/config.rs` | `config.toml` model, validated on load |
| `src/definition.rs` | Spanned two-phase parsing, the field walker with rule codes, the model, defaults, validation, both hashes |
| `src/credential.rs` | `secret_name_heuristic` and `secret_value_heuristic` |
| `src/source.rs` | Source scan with file-type and size checks, conflict-key handling, availability |
| `src/resolver.rs` | Pure resolution with statuses, shadowing and holds |
| `src/output.rs` | JSON envelope, text rendering, warnings on stderr |
| `tests/common/mod.rs` | Test isolation: `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_*` point into a tempdir; `D_MCP_STORE` is removed |
| `tests/cli.rs`, `tests/json_contract.rs` | Command-level and JSON-contract tests |
| `tests/fixtures/definitions/valid/`, `tests/fixtures/definitions/invalid/<error-code>/` | Fixtures whose expected error code is the directory name |
| `docs/reference.md` | Store layout, definition format with the normative examples, defaults, name and URL rules, secret heuristics, exit and error codes, JSON documents |
| `docs/adr/0001-store-centred-management.md` to `docs/adr/0004-oauth-first.md` | The four ADRs |

## Implementation details

### Approach

1. **Error model, exit-code table and JSON envelope.** Add a test that enumerates the error enum
   against the documented code list.
2. **Store**: path resolution, layout and permissions, the lock, atomic writes with their test
   seam, schema checks.
   - `init` is idempotent. It completes a partial store and reports what it added.
   - `init` refuses a non-empty directory that is not a store.
3. **Definition parsing and validation**: spanned parsing, the field walker, defaults, the field
   and URL rules, the secret heuristics, both hashes.
4. **Sources**: the implicit `local` source, `source add`, `source list` and `source remove` with
   their rejections, the scan with file checks, availability.
5. **Selections**: `select` and `unselect` with the semantics described above.
6. **Resolver and `resolve`**, with the limited output.
7. **Dry run** for every mutating command.
8. **`docs/reference.md` and the ADRs.**

### Component structure

- `definition`, `credential`, `config` and `resolver` do no I/O. `resolver` is pure.
- `source` reads files.
- `store` owns every write and the lock.
- `cli` parses, dispatches and renders.
- WP2 adds `adapter`, `state`, `sync`, `validate` and `process` without changing these boundaries.

### State management

Only the store files described above.

### API integration

Not relevant: no external programs and no network access.

### Styling approach

Not relevant: a command-line tool.

### Accessibility

Plain-text output that does not depend on colour.

### Edge cases

- **`HOME` unset or empty** and needed: exit 4. An empty `D_MCP_STORE` counts as unset.
- **Store directory without `config.toml`:** "not initialized", exit 1.
- **Partial store:** `init` completes it.
- **`init` into a non-empty directory that is not a store:** refused, exit 1.
- **Corrupt `config.toml` or unsupported schema:** exit 3, file never rewritten.
- **Lock held by another process:** retries, then `StoreLocked`, exit 3.
- **Malformed `servers/foo.toml`** in the highest-priority source: `hold(invalid-winner)` for
  `foo`.
- **Stem that breaks the name rule:** a per-file problem, shadows nothing.
- **Symlink, FIFO or file over 64 KiB in `servers/`:** a per-file problem.
- **Unavailable higher-priority source:** the affected selected servers get
  `hold(source-unavailable)`.
- **Database connection string or a URL with user info** in `args` or an env literal: rejected.
- **`--token=ghp_...` or `--api-key short`:** rejected, by the value check or the name check.
- **`${OTHER}` in an env literal:** rejected.
- **`http://` to a non-loopback host, or a query in `url`:** rejected.
- **32-character hex API key:** rejected. **A `sha256:` digest:** accepted.
- **`client_secret_env` without `client_id`:** invalid.
- **`__` in a name, or a trailing `-` or `_`:** invalid.
- **Relative or `~` command:** invalid.
- **Interrupted atomic write:** the old file stays intact.

## Acceptance criteria

- [ ] `mise run check` and `mise run check:full` pass, every CI job is green on the PR, and line
      coverage is at least the `coverage-threshold` value.
- [ ] Every fixture under `invalid/<error-code>/` fails with exactly that code, and every fixture
      under `valid/` passes. The matrix covers each heuristic rule at least once and each
      position at least once, the URL rules, variable syntax in every position, the field rules
      and the name rule.
- [ ] Both normative examples and every `auth` form parse as valid fixtures.
- [ ] Property tests show:
      - reformatting, comments, key order, `scopes` order and explicitly written defaults keep
        both hashes;
      - any change to a field changes `definition_hash`;
      - a change to `description` or the overrides changes `definition_hash` but not
        `core_hash`.
- [ ] A property test shows that resolution is deterministic for shuffled inputs.
- [ ] Canary tests: a token in a malformed definition, in a rejected literal and in a rejected
      argument never appears in stderr or `--json` output.
- [ ] Resolver tests cover every status:
      - `resolved`;
      - `hold(invalid-winner)`, including a malformed file;
      - `hold(source-unavailable)`;
      - `not-offered`;
      - shadowed definitions.
- [ ] Store tests show:
      - `--store` beats `D_MCP_STORE`, which beats `$HOME/.d-mcp`;
      - an empty `D_MCP_STORE` counts as unset;
      - an unset `HOME` gives exit 4;
      - `init` is idempotent, completes a partial store and refuses a foreign directory;
      - the implicit `local` source has priority 0 and cannot be removed;
      - permissions are `0700` and `0600`.
- [ ] Locking: a unit test with zero delays and an integration test that holds the lock through a
      second handle both show `StoreLocked` with exit 3. Read-only commands succeed while the lock
      is held.
- [ ] Command tests cover:
      - every `source add` rejection;
      - `source remove` reporting selections that are no longer offered;
      - notices for hidden, non-TOML and subdirectory files;
      - relative paths stored canonically;
      - `select` defaulting to `all`, replacing on re-selection and collapsing duplicates;
      - `unselect`;
      - selecting an unoffered name with a warning;
      - the `--dry-run` warning on read-only commands, with stdout still valid JSON.
- [ ] `--dry-run` on every mutating command leaves every store file, `.lock` included,
      byte-identical, and creates nothing on a missing store.
- [ ] The atomic-write failure injection through the test seam leaves the old file intact.
- [ ] The `--json` output of every WP1b command matches `docs/reference.md`.
- [ ] Every error-enum variant maps to its documented exit code and error code.
- [ ] `docs/reference.md` and ADRs 0001 to 0004 exist.

## Validation plan

- `mise run check` and `mise run check:full`.
- Integration and JSON-contract tests in the isolated test environment.
- Property tests for hashes and the resolver.
- Failure injection for atomic writes.
- A manual smoke test in a temporary `D_MCP_STORE`:
  1. `init`;
  2. a definition in `local`;
  3. a second source with a shadowed copy;
  4. `select`;
  5. `resolve --json`.

## Assumptions and open points

- The heuristics produce false positives and false negatives. A false positive is resolved by
  turning the literal into a forward. There is deliberately no "allow literal" escape.
- The 64-character model tool-name limit is re-verified in WP3.

## Plan review

**Result:** Approved

### Summary

| Area | Critical | Important | Note |
|---|---:|---:|---:|
| Architecture | 0 | 8 | 3 |
| Security | 1 | 4 | 2 |
| Data protection | 0 | 2 | 1 |
| Error cases | 0 | 5 | 3 |
| Testability | 0 | 2 | 3 |
| Scope | 0 | 1 | 4 |
| Maintainability | 0 | 1 | 3 |

### Findings

Round 1 (gap analysis and plan review, 2026-10-26):

- **Testability, Important:** a dry run touched `.lock`. Adjusted: dry runs take no lock and
  create nothing.
- **Data protection, Important:** parse errors could echo secrets. Adjusted: errors show the
  position and rule code only.
- **Security, Important:** literal secrets slipped through in `args`, the URL and overrides.
  Adjusted: value checks in all of these positions.
- **Security / Testability, Important:** the heuristic was not measurable. Adjusted: explicit
  rules, one fixture each.
- **Error cases, Important:** unknown keys were dropped. Adjusted: rejected with the field path.
- **Architecture, Important:** the hash was underspecified. Adjusted: domain tag, defaults
  applied, encoding rules.
- **Architecture, Important:** an invalid winner had no defined outcome. Adjusted: fail closed.
- **Error cases / Testability, Important:** exit codes for partial results were missing.
  Adjusted: documented.
- **Architecture, Important:** `cwd` was in the core. Adjusted: moved to `harness.codex`.
- **Architecture, Important:** the adoption target was missing. Adjusted by user decision: the
  built-in `local` source.
- **Testability, Important:** coverage tooling was missing. Adjusted, now part of WP1a.
- **Data protection, Important:** `resolve` output was unlimited. Adjusted: limited fields.
- Notes: missing acceptance criteria, slow lock test, placeholder files, `trust` field, edge
  cases, env on `http`, ADR naming, clippy flags, name-limit rationale, process conventions,
  scaffold scope. All adjusted.

Round 2 (deep interactive review, 2026-10-26):

- **Security, Critical:** the path and URL exemption let connection strings and URLs with user
  info through. Adjusted: user info rejected everywhere, exemption narrowed, fixtures added.
- **Security, Important:** positions were missing (`--flag=value`, flag names, query keys,
  `description`, `client_id`, `scopes`). Adjusted. By user decision, the `url` field rejects
  queries.
- **Security, Important:** there was no URL scheme rule. Adjusted by user decision: `https`, plus
  `http` only for loopback hosts.
- **Security, Important:** variable syntax was possible in env literals and overrides. Adjusted:
  rejected there too.
- **Architecture, Important:** the concrete TOML format was undefined. Adjusted: normative
  examples, defaults, rules for variable and header names.
- **Error cases, Important:** field paths cannot be had with serde. Adjusted: spanned two-phase
  parsing with a custom walker.
- **Architecture, Important:** an invalid winner was not handled downstream. Adjusted by user
  decision: `hold`, which WP2 turns into *Held*.
- **Error cases, Important:** an unparseable file had no conflict key. Adjusted: the file stem.
- **Architecture / Security, Important:** an unavailable source failed open. Adjusted by user
  decision: the affected servers are held.
- **Architecture, Important:** the location of `local` contradicted itself. Adjusted: root
  `<store>/local`, implicit.
- **Cross-package, Important:** WP4's duplicate detection needed a core-only hash. Adjusted:
  `core_hash`.
- **Architecture, Important:** hash gaps (floats, `description`, `scopes` order). Adjusted. By
  user decision, `description` is part of `definition_hash`.
- **Error cases, Important:** exit codes for corrupt config, usage errors and foreign
  directories. Adjusted: exit-code table.
- **Error cases, Important:** `select` and `source add` semantics were unclear. Adjusted.
- **Maintainability, Important:** JSON documents were undefined. Adjusted: envelope, problem codes
  and a contract test.
- **Scope, Important:** WP1 was too large for one PR. Adjusted by user decision: split into WP1a
  and WP1b.
- Notes:
  - missing acceptance criteria; `HOME` handling; lock ordering; the test seam;
  - file-type and size checks; permissions;
  - validation of `client_secret_env`, `__` and `command`;
  - the schema rule (moved to WP1a); heuristic naming; the hex and entropy rule.

  All adjusted.

## Test results

**Date:** 2026-10-27

- `mise run check`: passed. `cargo fmt --check` and clippy with `-D warnings` are clean, and all
  140 tests pass: 85 unit tests and 55 integration tests (`tests/acceptance.rs` 9,
  `tests/cli.rs` 30, `tests/definitions.rs` 6, `tests/json_contract.rs` 5, `tests/resolver.rs` 1,
  `tests/smoke.rs` 4).
- `mise run check:full`: passed. Line coverage is 99.08 % against the threshold of 85; regions
  98.19 %, functions 97.92 %. `cargo deny check` reports advisories, bans, licenses and sources
  ok, with the four known warnings about allowed but unused licenses.
- MSRV: `cargo +1.94 check --locked --all-targets` and `cargo +1.94 test --locked` passed.
- `RUSTDOCFLAGS=-D warnings cargo doc --no-deps --locked` and `cargo build --release --locked`
  passed.
- Fixtures: every `invalid/<rule-code>/` fixture fails with exactly its directory code, and
  every `valid/` fixture, both normative examples and every `auth` form parse.
- Acceptance criteria: each criterion is traced to at least one test, including the property
  tests for both hashes and resolver determinism, the canary tests, the lock tests, the
  `--dry-run` byte-identity tests and the atomic-write failure injection.
- Manual smoke test in a temporary `D_MCP_STORE`: `init`, a definition in `local`, a second
  source `team` at priority 10 with a changed copy, `select github`, then `resolve --json`. It
  reported `resolved` from `team` with both hashes and a `shadowed` problem for the `local` copy,
  exit 0.
- Not verified locally: the CI run on GitHub, including the macOS test leg. It runs on the pull
  request.

Implementation notes:

- The secret-value heuristic works per whitespace token and splits compound tokens at `=`, `;`
  and `,`. A token containing `/` is split into path segments unless it is standard base64 with
  a digit or `+`; only then is it checked whole. Without the split, the normative argument
  `@modelcontextprotocol/server-github@1.2.3` would fail the entropy rule.
- Exempt digests are excluded from the entropy input. A known prefix matches only at the start
  of a token piece and only with at least one following character.
- User info is detected wherever `scheme://` occurs in a token, including JDBC-style embedded
  URLs and the Go DSN form `user:pass@proto(`. A port or a colon in the path or query does not
  count as user info. Known false negative: an all-digit password followed by `/`.
- The name heuristic also runs on string-valued override keys and on the left side of
  `--flag=value` and secret-candidate `NAME=value` arguments. Override keys that reference an
  environment variable (last segment `env` or `var`, or a key in `env_http_headers`, with an
  identifier value) are exempt, except anywhere inside an `env` table.
- In `args`, `transport.url` and override strings a false positive is a hard rejection; the
  environment-forward remedy exists only for `env` literals. Known accepted false positives are
  documented in `docs/reference.md`.
- The validator stops at the first broken rule, so every fixture fails with exactly one code.
  Secret-looking keys appear as `<redacted>` in field paths.
- Datetimes are hashed as RFC 3339 strings with `+00:00` normalized to `Z`; `-00:00` stays
  distinct. Canonical JSON sorts keys explicitly and does not depend on `serde_json` features.
- Corner cases fixed during implementation: `unselect` of an unselected server fails with
  `not-selected` (exit 1); priority 0 exits 1 and a non-integer priority is a usage error
  (exit 2); `source add` on a directory without `servers/` succeeds with a warning;
  `source-unavailable` is checked before `invalid-winner`; lock retries total about 1.6 s.
- Store hardening: `init` follows a symlinked store root and warns about a root looser than
  `0700`; `.lock` and source files are opened with `O_NOFOLLOW` (source files also with
  `O_NONBLOCK`), with per-target constants because `libc` is not a dependency; a failed parent
  `fsync` after a successful rename is a warning; the `--dry-run` `created` list uses the real
  order; text output escapes control characters.
- 21 stable error codes; a unit test checks the error enum against the table in
  `docs/reference.md`.

## Review findings

**Date:** 2026-10-27
**Reviewer:** effective-flow-rust-reviewer (Rust bucket, three rounds); effective-flow-code-validator (tooling and documentation buckets)

### Summary

| Status | Count |
|---|---:|
| Fixed | 22 |
| Open / Not implemented | 0 |

Two further observations were closed without follow-up work: user info in `git+ssh://git@…`
is rejected by design, and a single path segment of 32 or more characters above 4.0 bits stays
rejected by the planned entropy rule.

## Open points

- No open points.
