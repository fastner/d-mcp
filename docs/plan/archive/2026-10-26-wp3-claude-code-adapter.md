# WP3: Claude Code adapter

**Plan status:** Implemented
**Source:** effective-flow plan
**Recommended workflow:** Feature (`effective-flow build`)

## Requirement

Work package 3 of the concept `docs/concept/2026-10-26-d-mcp.md`. It syncs managed servers into
Claude Code at user scope without ever writing its state file `~/.claude.json` directly. The
adapter covers:

- read-only parsing of `~/.claude.json`;
- writes and removals through the Claude CLI, with drift checks before each removal and
  rollback from memory for d-mcp's own renderings;
- rendering of the secret forms and of OAuth definitions, including `client_secret_env`;
- rejection of variable names that Claude Code expands to empty for remote servers;
- reserved names;
- gap reporting;
- detection of vanished and shadowed entries;
- the login-state capability.

It also extends WP2's adapter contract for the `HarnessCli` apply mode.

The workflow is Feature: this is the second harness target.

**Dependencies and re-planning.** WP3 builds on the implemented WP1 and WP2
(`docs/plan/archive/2026-10-26-wp2-adapter-contract-sync-engine-and-codex-adapter.md`). This plan
was re-planned against the merged WP2 code (`origin/main` aad6891): module, type, error and rule
names below are the real ones. WP4 depends on this adapter's discovery, import and login-state
capabilities.

**Context:**

- **Verified (local `claude` 2.1.310, help output only):**
  - `claude mcp` offers `add`, `add-json`, `get`, `list`, `login`, `logout`, `remove` and
    `reset-project-choices`.
  - `add-json <name> <json>` takes `-s/--scope local|user|project` (default `local`) and
    `--client-secret`, which prompts or reads `MCP_CLIENT_SECRET`.
  - `remove` takes an optional `-s`; without it, it picks whichever scope holds the name.
  - `get` takes no `-s`. `list` and `get` have no JSON option and health-check servers.
- **Verified (Claude Code documentation, code.claude.com/docs/en/mcp):**
  - User-scope servers live in the top-level `mcpServers` of `~/.claude.json`. Local-scope servers
    live under per-project entries in the same file.
  - `${VAR}` expands in `command`, `args`, `env`, `url` and `headers` for user and local entries
    too. If a required variable is unset and has no default, Claude Code fails to parse the
    config.
  - Credential variables, such as `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
    `AWS_BEARER_TOKEN_BEDROCK`, `HTTPS_PROXY` and `NPM_TOKEN`, read as empty in a remote server's
    `url` and `headers`, and `:-default` is ignored for them. A name outside that set, such as
    `API_KEY`, expands as written.
  - OAuth uses `"oauth": {"clientId", "callbackPort", "scopes"}`, where `scopes` is one
    space-separated string.
  - A client secret can only be set when a server is added. Changing it requires remove plus
    add with `--client-secret`.
  - `timeout` is in milliseconds; values below 1000 are ignored.
  - Reserved names include `workspace`, `claude-in-chrome`, `computer-use`, `Claude Preview` and
    `Claude Browser`, and `add` rejects them.
  - `claude mcp list` and `get` show `✔ Connected`, `! Needs authentication` or
    `✘ Failed to connect`.
  - `claude mcp login <name>` exists and needs an interactive terminal for its paste step when no
    browser is available.
  - Scope precedence is local over project over user.
- **Verified (WP2 code on `origin/main`):**
  - Adapter contract (`src/adapter/mod.rs`): `apply(&self, read, plan, journal, hooks)` has no
    process runner and no `Environment`; `EntryResult` carries only `readback: Option<String>`,
    and the executor removes a record whose `readback` is `None`; the outcome is reported per
    harness. `apply_mode`, `entries_can_vanish`, `discover_scopes` and `verified_versions` are
    declared but never read; the executor does not branch on `apply_mode`.
  - Write state (`src/state.rs`): a pending Add or Update needs the expected `sent` and
    `readback` hashes before the write (`Intent.expected_readback`), and `settle` promotes only
    when the expected read-back equals the actual hash; a pending Update keeps `prev_*` and
    reverts to them when the entry is absent.
  - `hashes_comparable` decides identical-collision detection and write-free take-over.
  - The executor turns `reserved_names()` into an `important` name gap and adds
    `foreign_security_gaps` from the other adapters' `foreign_security_keys`; an `important` gap
    blocks the entry (`sync-blocked`, exit 1). Override validation failures hold the server for
    every harness (exit 0 with a resolution problem).
  - `Location::present()` is `file_exists || binary_found`; not present is `not-present`, exit 0.
  - Process runner (`src/process.rs`): `Invocation { program, args, env }` without working
    directory or environment removal; the program is found through `Environment.path`, but the
    child inherits the process `PATH`; `DEFAULT_TIMEOUT` is 10 s and only the direct child is
    killed; child output is never printed.
  - `Environment` has `home`, `store`, `cwd`, `codex_home` and `path`; no `CLAUDE_CONFIG_DIR`.
  - `--harness` (`AdapterArg`) accepts only `codex`; `unmanage` of `all` already uses
    `Harness::ALL`; `state.toml` already accepts `harness = "claude"`.
  - Override types (`OverrideType`) have no integer-milliseconds or port type;
    `MAX_TIMEOUT_SECONDS` is in seconds. `harness.claude` is still opaque, and fixtures rely on
    that (`tests/fixtures/codex-overrides/valid/claude-only.toml`,
    `tests/fixtures/definitions/valid/overrides.toml`).
  - Test isolation (`tests/common/mod.rs`): `TestEnv::environment()` drops
    `CLAUDE_CONFIG_DIR` although `cmd()` sets it; `PATH` is inherited unless `with_path` or
    `without_codex` is used; a real `claude` is installed at `~/.local/bin` on this machine.
- **Note for the concept:** the concept states that Claude Code logins only work through `/mcp`.
  `claude mcp login` is the primary path and `/mcp` the fallback. Correct the concept through
  `effective-flow review docs/concept/2026-10-26-d-mcp.md`.

## Architecture decisions

- **Write through the CLI, read the file (concept decision).**
  - Add: `claude mcp add-json -s user <name> <json>`.
  - Remove: `claude mcp remove -s user <name>`; absence is verified afterwards.
  - Update: remove, then add.
  - `-s user` is always passed. The JSON is one argument, without a shell; names follow WP1's
    name rule, so no argument starts with `-`.
- **Reading `~/.claude.json` (data protection).** The adapter keeps no file bytes. It
  deserializes only `mcpServers.<name>` values (as `serde_json::Value`, every key of an entry)
  and, for discovery, `projects.<path>.mcpServers`; every other top-level key is parsed as
  `IgnoredAny`. Parse errors are reported with line and column only; serde messages are never
  printed. A parse error is retried once before failing with `harness-config-invalid`, because a
  running Claude Code session can be mid-write.
- **Adapter contract extension (user decision: extend the contract).**
  - `apply(&self, ctx: &ApplyContext { env, runner }, read, plan, journal, hooks)`.
  - `EntryResult.outcome`: `Applied`, `Failed(Error)`, `RolledBack` or `Lost`. The executor
    promotes `Applied` entries, keeps or reverts the pending record for the others, and reports
    each entry's own outcome; the harness outcome aggregates them.
  - `Journal::intend` is called per operation, immediately before each CLI call.
  - The executor branches on `apply_mode()`. The Codex adapter is adapted mechanically and keeps
    its batch semantics.
  - `Hooks` gains an "after remove, before add" seam for failure injection.
- **Comparable semantic hash (user decision).** The Claude entry hash is a tagged semantic hash
  (`d-mcp-claude-entry-v1`) over a canonical form applied identically to the rendering and the
  read-back: Claude's defaults are stripped (`type: "stdio"`, empty `args`, `env`, `headers`),
  numbers get one canonical form, keys are sorted. `hashes_comparable()` is `true`, and the
  journal records `expected_readback = sent`. The spike captures Claude's normalization for
  stdio, http, OAuth and header entries; a normalization the canonical form does not absorb makes
  verification fail (`harness-verify-failed`) rather than silently lose ownership.
- **Drift check before every removal.** Immediately before each `remove` (including the first step
  of an Update), re-read `.claude.json` and compare the entry's semantic hash with the recorded
  read-back hash. If it differs, report Drift and skip that entry. The small window between this
  re-check and `remove` is accepted and documented.
- **Rollback from memory, own renderings only (user decision).** Before `remove`, the entry just
  read is kept in memory. If the following add fails, it is re-added only when the previous
  record came from a d-mcp write (provenance marker in the record, set on every promoted d-mcp
  write and cleared by take-over and adoption). Otherwise, and when the rollback fails too, the
  entry is reported as `lost` with the names of the affected fields, the record reverts to its
  previous hashes, and the next sync re-adds the new rendering as Vanished. The record is never
  dropped. A rollback passes `--client-secret` when the planned definition's variable is set;
  otherwise it reports "client secret needed".
- **Timeouts.** CLI writes use a Claude-specific timeout (30 s). A timeout means "unknown": the
  adapter re-reads the file and settles the pending record; it never rolls back blindly.
- **File location.** `Environment` gains `claude_config_dir: Option<OsString>` (empty counts as
  unset, relative resolves through `Environment::absolute`) and `claude_config_file()`: without
  it `$HOME/.claude.json`, with it `$CLAUDE_CONFIG_DIR/.claude.json`, with a legacy
  `<config dir>/.config.json` preferred when it exists. The spike verifies this by setting `HOME`
  and `CLAUDE_CONFIG_DIR` to separate tempdirs. A changed `CLAUDE_CONFIG_DIR` is a Location
  change, and the notice names it.
- **Child process environment.** `Invocation` gains `cwd: Option<PathBuf>` and
  `remove: Vec<OsString>`. Every Claude call:
  - runs from a neutral working directory (the store root, verified to have no `.mcp.json`
    above it and no project entry in `.claude.json`), so Claude does not create or touch
    `projects["<caller cwd>"]`;
  - gets `PATH = Environment.path` (the `claude` node shim resolves `node` through it), `HOME`,
    and `CLAUDE_CONFIG_DIR` set or removed explicitly;
  - gets `DISABLE_AUTOUPDATER=1`;
  - has `MCP_CLIENT_SECRET` removed unless d-mcp sets it.
- **Rendering:**
  - **stdio:** `command`, `args`. Each forward becomes `env` with the value `${VAR}` (the spike
    decides between `${VAR}` and `${VAR:-}` from the unset-variable behaviour; documented as a
    difference from Codex, which skips unset variables). Literals pass through; WP1's
    `variable-syntax` rule keeps `$` and `${` out of literals.
  - **http:** `type: http`, `url`.
  - **Auth headers:** `bearer_env` becomes `Authorization: Bearer ${VAR}`; `header_env` becomes the
    named header with `${VAR}`.
  - **`oauth`:** `oauth.clientId` and `oauth.scopes` (joined with spaces). `oauth.callbackPort`
    comes from `harness.claude.oauth_callback_port` (decision: an adapter override, because only
    Claude Code is known to need it).
  - **`client_secret_env` (user decision):** `add-json` is called with `--client-secret`, and the
    named variable's value is placed into `MCP_CLIENT_SECRET` in the child process environment
    only. The value is never logged, persisted or printed. It is applied on Add and Update only;
    setting or rotating the variable later does not trigger a write. If the variable is unset, the
    entry is added without a secret and a manual step is reported once on that write, not on
    every sync.
- **Gaps.**
  - The Claude adapter declares `foreign_security_keys = [(Codex, "enabled_tools"),
    (Codex, "disabled_tools")]`, so the executor adds an `important` gap and such definitions are
    **not written** to Claude Code; `render` does not emit these gaps a second time. The Codex
    adapter declares no `harness.claude` key as security-relevant.
  - `harness.codex.cwd` is a `note` gap from `render`.
  - `auth = none` on http (Claude also discovers OAuth) and `oauth_supported` on `bearer_env` are
    `note` gaps, for parity with Codex.
- **Credential-name check (decision).** It applies to `bearer_env` and `header_env` variables of
  `http` servers, not to stdio forwards. It is an `important` gap from `render` for Claude Code
  only (entry blocked, `sync-blocked`, exit 1; Codex proceeds). The list contains:
  - the documented names;
  - the prefixes `ANTHROPIC_` (except `ANTHROPIC_BASE_URL`) and `AWS_`;
  - the suffix `_PROXY`.

  The list lives in one place with a citation. The manual check runs a fake HTTP MCP endpoint and
  observes an empty header for one name per category. Doctor (WP4) notes that the list may be
  incomplete.
- **Reserved names.** The documented list is kept with a citation and returned by
  `reserved_names()`, so WP2's executor blocks such entries with an `important` name gap before
  any CLI call. WP1's name rule (lowercase, at most 32 characters) already excludes
  `Claude Preview` and `Claude Browser`; only `workspace`, `claude-in-chrome` and `computer-use`
  matter. An `add-json` rejection stays a per-entry failure as defence in depth.
- **`harness.claude` allow-list (validated in WP2's `validate` step).**

  | Key | Type |
  |---|---|
  | `timeout` | integer milliseconds, 1000 to 31536000000 (new `OverrideType::Milliseconds`) |
  | `oauth_callback_port` | integer 1024 to 65535 (new `OverrideType::Port`) |

  Excluded keys: `headersHelper` / `headers_helper` (runs a local command, like Codex's
  `http_headers_helper`) and `env`. Unknown keys are invalid. **Migration note:** because
  validation failures hold the server for every harness, definitions with unknown
  `harness.claude` keys, previously opaque, are now held for Codex too; affected fixtures and
  docs move.
- **Vanished entries** are re-added and reported (WP2 planner). `entries_can_vanish = true` is
  declared for WP4 doctor. Concurrent Claude Code sessions rewrite `~/.claude.json` from memory,
  so a d-mcp add can be undone (Vanished) or a removed entry can come back (then unowned);
  both effects are documented.
- **Local-scope shadowing.** If a same-name local-scope entry exists for some project, the managed
  user entry is shadowed there. The adapter reports this as a warning per project path, and WP4
  doctor shows it. Whether `add-json -s user` accepts a name that exists in local scope is
  verified and covered by a fake test.
- **Login state (capability only, like Codex).** `sync` never calls it; WP4's `status --auth` is
  its first consumer.
  - It is queried only for managed entries with `oauth`, one `claude mcp get <name>` each. All
    other entries get `not-applicable` without any call. There is no `list` fallback.
  - Mapping: `✔ Connected` becomes `logged-in`, `! Needs authentication` becomes `needs-login`,
    and anything else (including `✘ Failed to connect`) becomes `unknown`.
  - Real outputs from 2.1.310 are recorded as test fixtures.
  - `--version` is not called during `sync`; WP4 doctor uses it.
- **Effect of remove on OAuth.** It is unverified whether `remove` clears stored OAuth tokens. If it
  does, an Update of an OAuth server reports "re-login needed" for that server.
- **Presence and exit codes (user decision on a missing CLI).**
  - No file and no binary: `not-present`, exit 0.
  - File present, binary missing: planning runs as usual (dry run included). If operations exist,
    the harness fails with `harness-cli-not-found` (exit 4) before any journaling; without
    operations, `unchanged` plus a notice.
  - New error codes: `harness-cli-not-found` (4), `harness-cli-failed` (1, per entry, naming only
    program, subcommand and exit status), `harness-cli-timeout` (4).
  - `harness.claude` allow-list violations hold the server for every harness (exit 0 with a
    resolution problem). Everything else follows WP2's aggregation (3 > 4 > 1 > 0).
- **CLI surface.** `AdapterArg` gains `claude`; the reference's "claude is a usage error" text,
  the `sync` help text and the Codex-specific `UnsupportedRoot` and relocate notices become
  adapter-provided texts. A non-object `mcpServers` means `root_supported = false`. Claude is
  registered after Codex.

## Affected files

| File | Description |
|---|---|
| `src/adapter/claude.rs` | Claude Code adapter: location, read, render with gaps, credential-name check, CLI apply with drift check and rollback, login-state capability, shadowing detection |
| `src/adapter/mod.rs` | `ApplyContext`, `EntryResult.outcome`, new override types, adapter-provided notice texts, registration (after Codex) |
| `src/adapter/codex.rs` | Mechanical adaptation to the extended contract; `foreign_security_keys` comment |
| `src/sync.rs`, `src/sync/executor.rs`, `src/sync/report.rs` | `HarnessCli` branch with per-intent journaling and per-entry outcomes; adapter-provided notices |
| `src/state.rs` | Provenance marker on records |
| `src/process.rs` | `Invocation.cwd` and `remove`; child `PATH` from `Environment.path` |
| `src/store.rs` | `Environment.claude_config_dir`, `claude_config_file()` |
| `src/validate.rs` | `harness.claude` allow-list, `Milliseconds` and `Port` types, excluded keys |
| `src/error.rs` | `harness-cli-not-found`, `harness-cli-failed`, `harness-cli-timeout` |
| `src/cli.rs` | `--harness claude`, help texts |
| `tests/common/mod.rs` | `CLAUDE_CONFIG_DIR` in `environment()`, `fake_claude_dir()`, `without_claude()`; Claude tests always use a controlled `PATH`; audit of `TestEnv::new()` call sites that leave `PATH` uncontrolled |
| `tests/support/fake_claude.rs` (Rust test helper, user decision) | Fake `claude` built as an example or test-only binary with `serde_json`: applies `add-json` and `remove` only under `mcpServers`, preserves every other key, normalizes like Claude, logs argv and `MCP_CLIENT_SECRET=<present\|absent>`, simulates failures, timeouts, a change between plan and apply, a reserved-name rejection, local-scope same-name behaviour and recorded `get` outputs, and refuses to run when its `HOME` is not under the test tempdir |
| `tests/fixtures/claude/` | `.claude.json` fixtures with account data and caches (canaries), local-scope projects and foreign servers; recorded `get` outputs |
| `tests/fixtures/codex-overrides/`, `tests/fixtures/definitions/` | Fixtures that relied on an opaque `harness.claude` move |
| `tests/sync_claude.rs`, `tests/json_contract.rs` | Integration and JSON-contract tests |
| `docs/reference.md` | Claude Code mapping, override keys, credential list, login-state semantics, error and rule tables, `--harness claude` |
| `docs/adr/0005-adapter-write-strategy-and-ownership.md` | Complete the ADR with the Claude Code half |

## Implementation details

### Approach

1. **Verification spike** with the real `claude`, only with `HOME` and `CLAUDE_CONFIG_DIR` both
   set to a fresh tempdir in the same command, recorded in the PR:
   - the `CLAUDE_CONFIG_DIR` location, including the legacy `.config.json`;
   - the normalized form Claude writes for stdio, http, OAuth and header entries;
   - the behaviour of an unset `${VAR}` in a user-scope entry (only that server, or the whole
     config) and whether Claude passes its own environment to stdio children;
   - `add-json -s user` when a local same-name entry exists;
   - whether `remove` clears OAuth tokens and the client secret;
   - recorded `get` outputs;
   - the 64-character model tool-name limit behind WP1's name rule.
2. **Contract extension** (`ApplyContext`, per-entry outcomes, per-intent journaling, process
   runner `cwd`/`remove`, `Environment.claude_config_dir`), with the Codex adapter adapted.
3. **Read path.** Lazy parsing of `mcpServers`, `projects.<path>.mcpServers` (local scope,
   discovery only) and semantic hashes.
4. **Rendering and validation:** secret forms, OAuth fields, `client_secret_env`, gaps, the
   credential-name check, reserved names and override validation.
5. **CLI apply:** drift check before `remove`, rollback for own renderings, read-back
   verification before records are promoted, timeout settling.
6. **Login-state capability** through `get` for managed OAuth entries, from the neutral working
   directory.
7. **Fake `claude` helper and tests.**
8. **Documentation and ADR completion.**

### Component structure

The adapter implements the extended WP2 contract in `HarnessCli` mode. The executor runs its
operations one by one, journaling each intent before its CLI call.

### State management

WP2's write state now also holds Claude Code records, plus a provenance marker on every record.

### API integration

The Claude CLI is called with `add-json`, `remove` and `get`, always through the process runner,
with a timeout, from the neutral working directory and without a TTY. `login` is not called here;
WP6 guides logins.

### Styling approach

Not relevant: a command-line tool.

### Accessibility

Plain-text output that does not depend on colour.

### Edge cases

- Neither `claude` nor the config file exists: `not-present`, exit 0.
- The config file exists but `claude` is missing: planned; with operations,
  `harness-cli-not-found` (exit 4) before any journaling; Codex proceeds.
- `~/.claude.json` is missing but `claude` is present: the first `add-json` creates it, and
  discovery reports no entries.
- `~/.claude.json` cannot be parsed (after one retry): `harness-config-invalid`, the error shows
  the position only.
- A foreign user-scope entry with the same name: Collision (WP2); an identical one: identical
  collision, taken over without a write.
- A local-scope entry with the same name in some project: a shadowing warning per project path.
- A hand edit between plan and `remove`: Drift, and the entry is skipped.
- `add-json` succeeds, but the read-back lacks the entry or differs: `harness-verify-failed`, the
  pending record is settled.
- A CLI call times out: the file is re-read and the pending record is settled.
- An Update whose add fails: rollback for own renderings; otherwise, or if the rollback fails too,
  the server is reported as lost and re-added on the next sync.
- A remote definition with a credential-named variable: blocked for Claude Code only.
- A reserved name: blocked by the executor before any CLI call.
- `client_secret_env` is unset: added without a secret, and a manual step is reported.
- An OAuth server is updated and `remove` cleared its tokens: "re-login needed" is reported.
- An unknown `harness.claude` key: the server is held for every harness.
- A concurrent Claude Code session rewrites the file: Vanished on the next sync, or an unowned
  entry.

## Acceptance criteria

- [ ] `mise run check` and `mise run check:full` pass; line coverage stays at or above
      `coverage-threshold`; every CI job is green on the PR.
- [ ] A structural guard proves d-mcp never opens `.claude.json` for writing (the read API is a
      read-only type, checked by a test), and a test where the fake changes nothing leaves the
      fixture byte-identical.
- [ ] In a fake test where the fake mutates only `mcpServers`, every other key of the fixture is
      semantically unchanged after sync.
- [ ] Fake-CLI tests cover:
      - Add;
      - Update, including a failed re-add with rollback, a double failure, and a failed re-add
        after take-over (no rollback, `lost`);
      - Remove, with absence verified;
      - Drift, including a change between plan and `remove`;
      - Collision and identical collision;
      - Vanished;
      - a crash between `add-json` and promotion: promoted on the next run;
      - a timeout: settled, never rolled back blindly;
      - local-scope shadowing;
      - file present, binary missing: `harness-cli-not-found` with operations, `unchanged`
        without.
- [ ] A second sync right after the first plans only Unchanged, even when the fake normalizes
      entries.
- [ ] Rendering tests cover:
      - `${VAR}` for forwards and headers;
      - `oauth.clientId` and space-joined `scopes`;
      - `callbackPort` from the override;
      - the Codex tool-filter gap (entry not written) and the note gaps;
      - no secret value in any rendered JSON;
      - `MCP_CLIENT_SECRET` present only in the child environment and never in the arguments,
        logs or output.
- [ ] Credential-name and reserved-name tests block the documented cases for Claude Code only
      and accept `API_KEY`.
- [ ] `harness.claude` override fixtures fail `validate` with their rule codes (unknown,
      excluded, out-of-range timeout and port); valid ones pass; moved fixtures stay green.
- [ ] Child-environment tests: every call runs from the neutral working directory with
      `PATH = Environment.path`, explicit `HOME` and `CLAUDE_CONFIG_DIR`,
      `DISABLE_AUTOUPDATER=1`, and without an inherited `MCP_CLIENT_SECRET`; the fake refuses to
      run outside the test tempdir.
- [ ] Login-state tests map the recorded `get` outputs correctly, make no CLI call for non-OAuth
      entries, run from the neutral working directory, and `sync` never calls `get`.
- [ ] No output contains a canary from the fixture's account or cache keys, and no serde error
      text is printed.
- [ ] The `--json` output of `sync` and `unmanage` with `--harness claude` matches
      `docs/reference.md`; every new error code maps to its documented exit code.
- [ ] Manual check with the real `claude`, in a temporary `HOME` and `CLAUDE_CONFIG_DIR`:
      - before and after sync, the parsed `.claude.json` minus the managed `mcpServers` entries
        differs only in keys Claude Code updates itself, as listed in the PR notes;
      - managed servers appear in `claude mcp list`;
      - the login state of one OAuth server is reported correctly by the capability (through a
        test or library call) before and after `claude mcp login`;
      - the credential-name observation via the fake HTTP endpoint is recorded.

## Validation plan

- The verification spike from Approach step 1, recorded in the PR.
- Fake-CLI integration tests on a controlled `PATH`, with `HOME` and `CLAUDE_CONFIG_DIR` in
  tempdirs.
- Unit tests for rendering, validation, canonical hashing and login-state mapping.
- A manual verification with Claude Code 2.1.310 or newer. The version is recorded in the
  adapter's verified-versions declaration.

## Assumptions and open points

- The credential-name list can be incomplete. Doctor says so.
- Managed-policy deny lists can block user servers; WP4 doctor mentions it.
- A concurrent Claude Code session can undo a d-mcp write; the next sync reports it.

## Plan review

**Result:** Approved

**Round 2 (2026-10-28, re-planning gate against the merged WP2 code).**

### Summary

| Area | Critical | Important | Note |
|---|---:|---:|---:|
| Architecture | 2 | 2 | 1 |
| Security | 0 | 5 | 0 |
| Data protection | 0 | 1 | 0 |
| Error cases | 0 | 5 | 4 |
| Testability | 0 | 3 | 0 |
| Scope | 0 | 0 | 2 |
| Maintainability | 0 | 1 | 1 |

### Findings

- **Architecture, Critical.** WP2's pending record needs the read-back hash before the write,
  but Claude normalizes on write. Adjusted by user decision: a comparable semantic hash over a
  canonical form.
- **Architecture, Critical.** `apply` could not express per-entry results and got no runner or
  environment. Adjusted by user decision: `ApplyContext` and per-entry outcomes.
- **Error cases, Important.** The "error operation, exit 1" did not exist in WP2. Adjusted: the
  credential check is an `important` gap; allow-list violations hold the server everywhere.
- **Error cases, Important.** "`claude` missing: exit 4" contradicted WP2's presence logic.
  Adjusted by user decision: plan, then `harness-cli-not-found` with operations; new CLI error
  codes.
- **Security, Important.** The runner had no working-directory or environment-removal control,
  and the child inherited the process `PATH`. Adjusted.
- **Security / Testability, Important.** `Environment` had no `CLAUDE_CONFIG_DIR`, and the child
  `HOME` was inherited, so a test could write the developer's real config. Adjusted.
- **Error cases, Important.** Timeout semantics for CLI writes. Adjusted: a timeout settles, never
  rolls back blindly; 30 s.
- **Architecture, Important.** Dropping the record on a double failure contradicted WP2's `prev_*`
  semantics. Adjusted: revert, report `lost`, re-add as Vanished.
- **Architecture, Important.** The tool-filter gap duplicated WP2's foreign-security mechanism.
  Adjusted.
- **Error cases, Important.** New override types and the migration were missing. Adjusted.
- **Maintainability, Important.** Codex-specific texts, the CLI enum and several files were
  missing from Affected files. Adjusted.
- **Security / Data protection, Important.** A rollback after take-over would put foreign,
  possibly secret content on argv. Adjusted by user decision: rollback only for d-mcp's own
  renderings, provenance marker.
- **Security, Important.** The client secret is applied only on Add and lost on rollback.
  Adjusted.
- **Security, Important.** An unset `${VAR}` breaks Claude's config parse. Adjusted: spike
  decides `${VAR}` or `${VAR:-}`, documented.
- **Data protection, Important.** What is held in memory from `~/.claude.json`, and what the hash
  covers. Adjusted: no file bytes, every key of an entry hashed, `IgnoredAny` elsewhere.
- **Error cases, Important.** Concurrent Claude Code sessions rewrite the file. Adjusted:
  documented, one parse retry, `DISABLE_AUTOUPDATER=1`.
- **Testability, Important.** A shell fake cannot edit JSON robustly. Adjusted by user decision:
  a Rust test helper.
- **Testability, Important.** The read-only fixture proved nothing for the same uid. Adjusted:
  structural guard plus byte comparison.
- **Testability, Important.** The inherited `PATH` made a developer's real `claude` count as
  present. Adjusted: controlled `PATH`, call-site audit.
- **Error cases, Note.** Reserved names are already enforced by WP2. Adjusted.
- **Architecture, Note.** Vanished is not driven by `entries_can_vanish`. Adjusted.
- **Scope, Note.** The login state has no WP3 consumer. Adjusted: capability only; no
  `--version` during `sync`.
- **Error cases, Note.** The re-check before `remove` leaves a small window. Adjusted: accepted,
  absence verified.
- **Error cases, Note.** Location and scope details (`-s user`, `get` without `-s`, legacy file).
  Adjusted.
- **Error cases, Note.** Note gaps for `auth = none` and `oauth_supported`; managed-policy deny
  lists. Adjusted.
- **Scope, Note.** The spike missed the normalization, unset-variable and legacy-file checks.
  Adjusted.
- **Maintainability, Note.** Stale dependency notes, WP2 path and review history. Adjusted.

**Round 1 (2026-10-26, before WP2 existed).** 9 Important and 7 Note findings, all adjusted:
rollback from the in-memory entry; Update decided on the sent hash; `client_secret_env` through
`MCP_CLIENT_SECRET` (user decision) with a token-clearing check; drift check before every
`remove`; login state only for managed OAuth entries from a neutral working directory;
local-scope shadowing; credential list by decision; documented OAuth mapping with
`oauth_callback_port`; content-preservation tests; WP2 owns the process runner; WP2 exit-code
aggregation; `timeout` units and reserved-name sources; `CLAUDE_CONFIG_DIR` marked unverified;
recorded `get` fixtures; login citation; a measurable re-planning gate.

## Test results

**Date:** 2026-10-28

- `mise run check`: passed. `cargo fmt --check` and clippy with `-D warnings` are clean, and all
  283 tests pass: 150 unit tests and 133 integration tests (`tests/sync_codex.rs` 43,
  `tests/sync_claude.rs` 32, `tests/cli.rs` 30, `tests/acceptance.rs` 9,
  `tests/json_contract.rs` 8, `tests/definitions.rs` 6, `tests/smoke.rs` 4,
  `tests/resolver.rs` 1).
- `mise run check:full`: passed. Line coverage is 97.93 % against the threshold of 85.
  `cargo deny check` reports advisories, bans, licenses and sources ok, with the four known
  warnings about allowed but unused licenses.
- MSRV: `cargo +1.94 check --locked --all-targets --all-features` and
  `cargo +1.94 test --locked` passed.
- `RUSTDOCFLAGS=-D warnings cargo doc --no-deps --locked`, `cargo build --release --locked` and
  `git diff --check` passed. A clean CI simulation in a fresh target directory, including
  `cargo llvm-cov`, found and used a fresh fake `claude`.
- Acceptance criteria: each criterion except the manual check is traced to at least one test,
  including the structural read-only guard, the byte-identity test for a no-op write, every
  fake-CLI scenario (rollback, double failure, failed re-add after take-over, crash and
  timeout settling, shadowing, missing CLI), the child-environment test and the JSON contract.
- Verification spike with `claude` 2.1.310, run only against temporary `HOME` and
  `CLAUDE_CONFIG_DIR` directories:
  - the file is `$CLAUDE_CONFIG_DIR/.claude.json`, or `$HOME/.claude.json` without the
    variable; a legacy `.config.json` in the config home wins when it exists; with the variable
    set but empty, Claude resolves its config home relative to its working directory;
  - Claude adds `args: []` to stdio entries, keeps `type` and empty `env` or `headers`, drops
    unknown keys, writes `30000.0` as `30000` and reorders `oauth` keys; the canonical hash
    absorbs all of this;
  - an unset `${VAR}` reaches the server as literal text and only that server warns, so
    forwards render as `${VAR:-}`;
  - `add-json -s user` accepts a name that exists in local scope and rejects an existing user
    name and `workspace`;
  - `remove` clears the stored OAuth tokens and the client secret, so an Update of an OAuth
    server reports "re-login needed";
  - `get` outputs for `✔ Connected`, `! Needs authentication` and `✘ Failed to connect` are
    recorded as fixtures;
  - the tool-name pattern `^[a-zA-Z0-9_-]{1,64}$` is present in the binary;
  - against a local HTTP endpoint, only the documented credential names read as empty
    (`NPM_TOKEN`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_FOUNDRY_API_KEY`,
    `AWS_BEARER_TOKEN_BEDROCK`, `AWS_SECRET_ACCESS_KEY`, `CODEARTIFACT_AUTH_TOKEN`);
    `ANTHROPIC_FOO`, `AWS_FOO`, `FOO_PROXY`, `AWS_ACCESS_KEY_ID`, `GITHUB_TOKEN` and `API_KEY`
    expand as written. The planned prefix and suffix rules therefore block more names than
    Claude blanks; they are kept as decided and documented.
- Manual check with the real `claude`: the parsed `.claude.json` minus the managed entries was
  identical before and after sync, no `projects` entry was created, the managed servers appear
  in `claude mcp list`, and the client secret reached Claude's credential store and no file
  d-mcp wrote.
- Login state (user decision: partial): `needs-login` before login was verified live against a
  local OAuth endpoint through the capability; `logged-in` after login is derived from recorded
  output with planted tokens. **Left for manual verification by the user:** the login state of
  one OAuth server before and after a real interactive `claude mcp login`.
- Manual smoke test in temporary directories: `init`, four definitions, `select`,
  `sync --dry-run --json`, a sync of both harnesses, a second sync (Unchanged), a hand edit
  (Drift), `--take-over` (Update) and `unmanage --harness claude` (`all` became `["codex"]`).
- Not verified locally: the CI run on GitHub, including the macOS test leg.

Implementation notes:

- The adapter contract gains `ApplyContext { env, runner }` and per-entry outcomes
  (`Applied`, `Failed`, `RolledBack`, `Lost`, `Drift`); the executor branches on the apply
  mode and journals each intent before its CLI call. The Codex adapter was adapted
  mechanically.
- The provenance marker lives in the record as `written` / `prev_written`; records without it
  never roll back. Older binaries refuse such state as `state-corrupt`; there is no downgrade
  path before 1.0.
- The fake `claude` is a `[[test]] harness = false` target (not an example or bin, so neither
  `cargo llvm-cov` misses it nor `cargo install` installs it). The tests accept a build only if
  it is newer than every source in its dep-info file. The fake refuses to run outside a marked
  test root.
- `TestEnv` controls `PATH` by default (an empty directory) instead of auditing each call site.
- The neutral working directory is the store root, falling back to a fresh temporary directory;
  with `CLAUDE_CONFIG_DIR` set but empty it is always a fresh temporary directory, because
  Claude then writes `backups/` and reads a legacy `.config.json` relative to its working
  directory. No neutral directory fails with `harness-no-neutral-dir` (exit 1) before any call.
- The login-state capability applies to remote entries with an `oauth` object or without
  static headers, because OAuth without a client id renders no `oauth` object.
- Local-scope shadowing is reported as harness notices.
- Entries in the JSON document gain an `error` field and the outcomes `rolled-back` and `lost`;
  a harness with a failed entry has status `failed` and carries the most severe entry error.
- A rollback of an OAuth entry restored without a secret reports "client secret needed; the
  restored entry has none".
- The parsed Claude file has a names-only `Debug`, so foreign values cannot be printed.
- New error codes: `harness-cli-not-found` (4), `harness-cli-failed` (1),
  `harness-cli-timeout` (4), `harness-no-neutral-dir` (1). Claude overrides reuse
  `override-key` and `override-type`.

## Review findings

**Date:** 2026-10-28
**Reviewer:** effective-flow-rust-reviewer (Rust bucket, two rounds); effective-flow-code-validator (tooling and documentation buckets)

### Summary

| Status | Count |
|---|---:|
| Fixed | 11 |
| Open / Not implemented | 0 |

## Open points

- No open points.
