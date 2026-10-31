# WP5: Git sources, approval and audit

**Plan status:** Implemented
**Source:** effective-flow plan
**Recommended workflow:** Feature (`effective-flow build`)

## Requirement

Work package 5 of the concept `docs/concept/2026-10-26-d-mcp.md`. It gives users the same servers
on every machine from personal or team Git sources. It covers:

- adding and refreshing Git sources pinned to a commit, read without a checkout;
- priorities and trust status;
- source-qualified approval bound to the definition hash;
- a blocking audit for unpinned launchers, with risk acceptance bound to the definition hash and
  the findings;
- holding previously approved entries while a new version waits for approval, and removing them
  on `revoke`;
- lock-file updates;
- `select --all-from <source>` so that a second machine is one setup run plus one sync.

The workflow is Feature.

**Dependencies.** WP5 builds on the merged WP1–WP4 code (origin/main 03aa2a4):

- the source scanner, validator and pure resolver (`src/source.rs`, `src/validate.rs`,
  `src/resolver.rs`);
- the source-id rule, the store layout and the store lock (`src/config.rs`, `src/store.rs`);
- the process runner (`src/process.rs`);
- the sync planner and executor with the *Held* operation (`src/sync.rs`, `src/sync/executor.rs`);
- the redaction layer (`src/redact.rs`), status, doctor, inventory and adoption (WP4);
- the error-code table, problem codes and JSON documents in `docs/reference.md`.

WP6 depends on WP5: its skill shows the source, the command or URL and the audit findings from
WP5's JSON documents before every `approve`.

**Context:**

- **Verified (Dalo):**
  - `git` runs through a guarded runner. Dalo sets the protocol allow-list `https:ssh:git:file`;
    d-mcp deliberately drops `git:` because it is unauthenticated. Dalo also sets
    `GIT_PROTOCOL_FROM_USER=0` and `GIT_TERMINAL_PROMPT=0`, and uses `ssh -oBatchMode=yes` unless
    `core.sshCommand` is configured.
  - Remote URLs with user info are rejected.
  - Files are read at a commit through `git show <commit>:<path>` with validated hex ids, without
    a checkout.
  - Tests isolate git config (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`, and unset
    `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS`).
  - Approvals are source-qualified. Accepted risks carry a scope hash over audit inputs and
    findings, plus a reason.
- **Verified (merged code):**
  - `SourceKind` has only `Directory` (`directory`); `local` is the id of the built-in source, not
    a kind. `config.toml` requires an absolute canonical `path` and rejects paths inside the store
    (`source-path-in-store`).
  - `KNOWN_ENTRIES` in `src/store.rs` already reserves `lock.toml` and `approvals.toml`, but not
    `sources`.
  - `HoldReason` is `invalid-winner | source-unavailable`. `executor::desired_for` maps every other
    hold reason to `invalid-winner` and every non-resolved status to `held(not-offered)`. The
    planner plans nothing for a held server d-mcp does not own; it removes owned entries whose
    server is not in the desired map.
  - Six call sites run `source_specs → scan → validate → resolve`: `src/cli.rs` (resolve),
    `src/sync/executor.rs`, `src/status.rs`, `src/inventory.rs`, `src/doctor.rs` and
    `src/adopt/candidate.rs`.
  - The process runner drops stderr, has a 10 s default timeout and kills only the direct child.
  - `src/redact.rs` hides `command`, `args`, `cwd` and the URL path in every view.
  - WP1 already defines digest contexts (a 40- or 64-character hex run after `@`/`#`, and
    `sha256:`) and designates `definition_hash` (tag `d-mcp-definition-v1`) as the approval
    binding.
  - `adopt --expect <display hash>` (exit 3 `adopt-entry-changed`) is the existing pattern for
    binding an apply to what the preview showed.
  - `TestEnv` runs d-mcp with an empty `PATH` and an empty environment.

## Architecture decisions

- **Two source kinds.**
  - `directory` (existing): read as a directory, no approval. This includes the built-in `local`
    source.
  - `git` (new, `SourceKind::Git`): a remote URL, cloned into `<store>/sources/<id>/repo`, pinned
    to a commit, approval required.
- **`config.toml` schema for Git sources.** A Git `[[sources]]` entry has `id`, `kind = "git"`,
  `url`, `ref`, `priority` and `trusted` (default `false`), and no `path`. Its location is derived
  as `<store>/sources/<id>`, like `local`; `source-path-in-store` does not apply to it. The pin
  lives in `lock.toml`, not in `config.toml`. `schema_version` stays 1 (additive, CONTRIBUTING
  schema rule). The JSON source object gains `url`, `ref`, `commit` and `trusted` (`null` for
  directory sources).
- **Personal multi-machine flow (user decision: `select --all-from`).**
  - On the first machine, the user turns the built-in `local` directory into a Git repository and
    pushes it. Adopting into `local` (WP4) followed by a manual `git commit && git push` is the
    adoption path into the personal source; d-mcp never commits or pushes.
  - On every other machine, setup is one scripted sequence: `source add-git <url> --trusted`,
    `approve --all-from <id> --expect …`, `select --all-from <id>`, `sync`.
  - `select --all-from <source> [--dry-run] [--json]` selects every server the source currently
    offers that is not yet selected, for all harnesses. It never unselects.
  - Convergence: the second machine pins the current head of the ref. To match an exact commit,
    `source add-git` accepts `--commit <sha>`, which must be reachable from the ref. `--ref`
    defaults to the remote HEAD branch.
  - On machine 2, `adopt` of a name the personal Git source already offers is refused with the
    existing `not-winning` reason when the Git source wins.
- **Input validation.**
  - A password in user info (`user:pass@`) is rejected everywhere, and any user info is rejected
    on `https` and `file`. A user name without a password is allowed on `ssh://` and the scp-like
    form (`git@host:org/repo.git`). The hint points to a credential helper or SSH.
  - A query or fragment is rejected (reusing rule code `url-query`), and
    `secret_value_heuristic` runs on the path segments.
  - `ext::` and other transports are blocked by `GIT_ALLOW_PROTOCOL=https:ssh:file`.
  - A URL or ref starting with `-` is rejected, and `--` precedes positional arguments.
  - Refs are checked with `git check-ref-format`.
  - Commit ids are accepted only as 40- or 64-character hex.
  - Source ids follow WP1's name rule.
- **Git hardening.** Every git invocation passes `-c core.hooksPath=/dev/null
  -c core.fsmonitor=false -c transfer.fsckObjects=true -c gc.auto=0`, runs with `GIT_ASKPASS` and
  `SSH_ASKPASS` cleared, `GIT_TERMINAL_PROMPT=0`, `GIT_PROTOCOL_FROM_USER=0`, and, unless
  `core.sshCommand` is configured, `ssh -oBatchMode=yes -oConnectTimeout=15`.
- **Process runner extension.** `Invocation` gains an opt-in `classify_stderr` mode: stderr is read
  capped, matched in memory against fixed git patterns and mapped to stable codes
  (`git-auth-failed`, `git-host-unknown`, `git-unreachable`, `git-ref-not-found`, `git-failed`).
  stderr is never printed or persisted. Network git calls (clone, fetch, ls-remote) use their own
  120 s timeout. The runner still kills only the direct child; a surviving `ssh` or
  `git-remote-https` grandchild is a documented limit.
- **Clone and snapshot (user decision: snapshot at add/refresh).** Git runs only in mutating
  commands and dry runs, never at read time.
  1. Clone with `--no-checkout --no-recurse-submodules` (full clone, no partial-clone filter) into
     a temporary directory `sources/.tmp-<random>`.
  2. Verify the commit (`rev-parse --verify <commit>^{commit}`) and keep it reachable via
     `refs/d-mcp/pin`.
  3. List `ls-tree -z <commit>:servers` (non-recursive). Map entries with the directory scanner's
     notice and rule semantics: hidden entries, non-`.toml` files and subdirectories are notices;
     symlinks (`120000`) and gitlinks (`160000`) are `not-regular-file`; a stem failing the name
     rule is `invalid-stem`. Check each size with `cat-file -s` before reading; over 64 KiB is
     `file-too-large`. Read accepted blobs with `cat-file blob`; non-UTF-8 is `not-utf8`. A commit
     without `servers/` makes the source unavailable.
  4. Materialize the accepted blobs into a snapshot directory `snapshot/servers/` with a stamp file
     recording the commit, written to a temporary directory and renamed into place.
  5. Under the store lock: sweep stale `sources/.tmp-*`, replace an unregistered `sources/<id>`,
     rename into place, then write `lock.toml`, then `config.toml`. A registered source therefore
     always has a pin. On failure the temporary directory is deleted.
  - At read time, the existing directory `scan` reads `sources/<id>/snapshot/servers/` unchanged
    (with its `O_NOFOLLOW` reads, 64 KiB cap and rule codes). A missing snapshot, a missing or
    unreadable pin, or a stamp that differs from the pin makes the source `available = false`,
    which yields the existing `hold(source-unavailable)`. Only mutating commands re-clone a broken
    clone.
- **Shared resolution loader.** One function, e.g. `source::load_resolution(paths, config, lock,
  approvals)`, scans, validates, applies the approval and audit gate and resolves. All six call
  sites go through it, so `resolve`, `status`, `inventory`, `doctor`, `adopt` and `sync` agree.
- **Dry runs (user decision: temporary fetch outside the store).** `source add-git --dry-run` and
  `source refresh --dry-run` clone into a fresh directory under `TMPDIR`, which is deleted
  afterwards, and report from there. The store stays byte-identical, including the existing
  clone. A full re-download per dry run is accepted.
- **Pinning and refresh.**
  - `source refresh <id> --dry-run [--json]` shows the change set between the pinned and the new
    commit: added, changed and removed definitions, each with validation problems, audit findings
    and the new `definition_hash`, rendered through the approval view.
  - `source refresh <id>` fetches into the store clone outside the lock, then under the lock
    re-checks that the source still exists and its pin is unchanged (otherwise `source-changed`,
    exit 3), re-materializes the snapshot and writes the pin. There is no interactive prompt.
  - **Rewrites:** `git merge-base --is-ancestor <old> <new>` detects them. A moved tag counts as a
    rewrite. A rewrite is refused without `--allow-rewrite` (`source-rewrite`). A deleted ref is
    `source-ref-deleted`. `refs/d-mcp/pin` keeps the old commit reachable until the user decides.
  - A moved pin never approves anything.
- **Approval view (user decision: own approval view).** The refresh change set, `approve`,
  `accept-risk` and the pending-approval detail show, for definitions from Git sources, `command`,
  `args`, the URL path and `cwd` in addition to the WP4 view. These have already passed the WP1
  secret rules. Env values, header values and override values stay redacted. This is a documented
  exception to the default-deny contract, recorded in ADR 0006, with redaction canaries proving
  that env, header and override values never appear. Audit findings show the matched package spec
  through the same view.
- **Source-qualified approval.**
  - `approve <server> [--source <id>] --expect <definition_hash>` records the source id, the source
    URL, the server name and the `definition_hash`. Without `--dry-run`, `--expect` is required; a
    mismatch is `approval-changed` (exit 3). `--dry-run` shows the hash to pass.
  - Approvals keep a set of approved hashes per (source, name), so a rollback to an earlier
    approved commit needs no re-approval. An approval applies only while the source URL matches.
  - `approve --all-from <source>` approves the clean servers of a trusted source, prints the
    approved hashes and lists the blocked ones, with exit 1 if any are blocked. Its `--expect`
    takes the source's pinned commit.
  - `approve` on a server that needs none is `approval-not-needed`.
- **Resolve-time gate.** A Git winner resolves only if (source, name, `definition_hash`) is
  approved and either it has no blocking findings or an acceptance matches the `definition_hash`
  and the current findings hash. Otherwise:
  - not approved: `hold(pending-approval)`;
  - approved, but findings changed since the acceptance: `hold(risk-not-accepted)`;
  - revoked: status `revoked`.
  `HoldReason` and `HeldReason` gain `pending-approval` and `risk-not-accepted`; both catch-all
  arms in `executor::desired_for` become exhaustive matches. The resolver does not read
  `state.toml`: *Held* keeps an owned entry unchanged, and for an unowned one nothing is planned
  (existing behaviour). All three still shadow lower-priority definitions; there is no
  fall-through. `resolve --json` reports `definition_hash` and `findings` for these states.
- **Revoke (user decision: revoke removes).** `revoke <server> [--source <id>]` removes the
  approvals and records a revoked marker for (source, name). The resolver returns status
  `revoked`, which `desired_for` leaves out of the desired map, so the next sync removes an owned
  entry like an unselected one. A later `approve` clears the marker.
- **Audit: unpinned launchers.**
  - **Matching:** on the command basename, skipping launcher options before the package spec. The
    options covered:
    - `npx -y`, `--yes`, `-p` and `--package=`;
    - `uvx --from`;
    - `pipx run --spec`;
    - docker flags.
  - **Exact version:** a full SemVer or PEP 440 version. Ranges and dist-tags (`@^1`, `@1`,
    `@latest`, `@next`, `>=`) are unpinned.
  - **Scoped npm packages** need `@scope/pkg@x.y.z`.
  - **VCS and URL specs** (`github:`, `git+https://`, tarball URLs) are pinned only with a commit
    in a WP1 digest context.
  - **Covered runners:**
    - `npx`, `npm exec` and `npm x`;
    - `pnpm dlx` and `pnpx`;
    - `bunx` and `bun x`;
    - `yarn dlx`;
    - `uvx`, `uv tool run` and `pipx run`;
    - `docker run`, `docker container run`, `podman run` and `nerdctl run`.
  - **Containers (concept wording):** pinned means `@sha256:` or a tag other than `latest`. So
    `image:1.2` is pinned.
  - **Opaque wrappers:** `sh`, `bash` or `zsh` with `-c`, and `env` with a nested launcher, are
    classified as "opaque command", which needs risk acceptance. `env` is unwrapped one level.
  - **Overrides:** only the neutral definition is audited. A guard unit test fails if any adapter
    override allow-list gains `command`, `args` or another command-running key.
  - **Not audited:** arbitrary binaries and scripts, and remote URLs. This is documented in
    `docs/reference.md`.
  - **Directory sources:** findings are warnings and block nothing.
- **Risk acceptance.**
  - `accept-risk <server> --reason <text> --expect <definition_hash>` records the definition hash,
    a findings hash (rule ids plus matched arguments), the reason and a timestamp. It does not
    approve. `--expect` is required without `--dry-run`.
  - `--reason` is capped at 500 characters; `secret_value_heuristic` runs on it, and a match is
    rejected without echoing the text.
  - `approve` then succeeds only when the current findings hash matches. New audit rules
    therefore invalidate old acceptances (`hold(risk-not-accepted)` at resolve time).
- **Trust status.**
  - `source add-git ... [--trusted]` defaults to `untrusted`.
  - `source trust <id> --on|--off` changes it, with `--dry-run`.
  - In v1, trust only allows `approve --all-from`. It has no effect on directory sources.
- **Source removal.** `source remove <git-id>` deletes `sources/<id>` and the pin. Its approvals
  remain and are reported as orphaned (problem code `orphaned-approval`). Re-adding the same id
  with another URL does not reactivate them (URL binding).
- **Files.**
  - `lock.toml`: pinned commits per Git source.
  - `approvals.toml`: approvals, revoked markers and accepted risks. It is separate like Dalo's
    approval store, which deviates from the concept's data outline; the deviation is recorded in
    ADR 0006.
  - Both follow the `config.toml` rules: created on first write, atomically and under the store
    lock; strict parse; a corrupt file is `lock-corrupt` / `approvals-corrupt` (exit 3), never
    rewritten, and a doctor `error`.
  - `sources/` joins `KNOWN_ENTRIES`.
- **Uninstall (user decision: block purge of a local Git repository).**
  - `uninstall --purge` deletes `sources/` (re-creatable clones and snapshots).
  - If `<store>/local/.git` exists, `--purge` fails with `purge-blocked`, reason `local-is-git`,
    unless `--include-local-repo` is given.
  - The reference sentence becomes: directory sources live outside the store and are never
    touched; Git clones under `sources/` are deleted.
- **Status, doctor, inventory.**
  - `status` shows `held` with `pending-approval` / `risk-not-accepted` and `revoked` through the
    shared loader.
  - New doctor checks: `git` (warn when a Git source exists and `git` is missing), `approvals`
    (warn: orphaned; error: corrupt), `lock` (error: corrupt), `pending-approval` (warn).
- **Not in WP5:** d-mcp writing, committing or pushing into a Git source (concept v1.2,
  contributing back).

## JSON and codes

- **Documents** in `docs/reference.md` for `source add-git`, `source refresh`, `source trust`,
  `source remove` (Git), `select --all-from`, `approve`, `accept-risk` and `revoke`, plus the
  source-object extension and the `resolve` extension (`definition_hash` and `findings` for
  `hold(pending-approval)`, `hold(risk-not-accepted)` and `revoked`).
- **Problem codes:** `pending-approval`, `risk-not-accepted`, `audit-finding`,
  `orphaned-approval`.
- **Error codes and exit codes:**

  | Code | Exit |
  |---|---:|
  | `source-url-invalid` | 1 |
  | `source-ref-invalid` | 1 |
  | `source-commit-unreachable` | 1 |
  | `source-rewrite` | 1 |
  | `source-ref-deleted` | 1 |
  | `approval-blocked` | 1 |
  | `approval-not-needed` | 1 |
  | `approval-changed` | 3 |
  | `source-changed` | 3 |
  | `lock-corrupt` | 3 |
  | `approvals-corrupt` | 3 |
  | `git-auth-failed`, `git-host-unknown`, `git-unreachable`, `git-ref-not-found`, `git-failed` | 4 |
  | `git-not-found` | 4 |
  | `git-timeout` | 4 |

  Each code joins the `src/error.rs` table and the reference table, which a unit test keeps in
  sync. `purge-blocked` gains the reason `local-is-git`.
- **Tests:** a `tests/redaction.rs` case and a `tests/json_contract.rs` case per new command.

## Affected files

| File | Description |
|---|---|
| `src/git.rs` | Guarded `git` operations on the process runner: clone, fetch, ls-remote, rev-parse, ls-tree, cat-file, merge-base, check-ref-format, update-ref; URL and ref validation; hardening flags |
| `src/process.rs` | `classify_stderr` mode and per-invocation network timeout |
| `src/config.rs` | `SourceKind::Git`, Git source fields, derived location |
| `src/store.rs` | `sources` in `KNOWN_ENTRIES` and the layout |
| `src/source.rs` | Git snapshot materialization; shared `load_resolution` loader |
| `src/lockfile.rs` | `lock.toml` with pinned commits |
| `src/approval.rs` | Source-qualified approval sets with URL binding, revoked markers, accepted risks with findings hashes, orphan reporting |
| `src/audit.rs` | Table-driven launcher rules |
| `src/resolver.rs` | Approval and audit inputs; `pending-approval`, `risk-not-accepted`, `revoked`; blocked-winner shadowing |
| `src/sync.rs`, `src/sync/executor.rs` | New held reasons; exhaustive `desired_for`; `revoked` left out of the desired map |
| `src/redact.rs` | The approval view for Git-source definitions |
| `src/status.rs`, `src/inventory.rs`, `src/doctor.rs`, `src/adopt/candidate.rs`, `src/cli.rs` | Route through `load_resolution`; new doctor checks |
| `src/uninstall/purge.rs` | Purge `sources/`; `local-is-git` block |
| `src/error.rs` | New error codes |
| `src/cli.rs` | `source add-git`, `source refresh`, `source trust`, `select --all-from`, `approve`, `accept-risk`, `revoke` |
| `tests/common/mod.rs` | `TestEnv::with_git()`: only the resolved `git` binary symlinked into a controlled `PATH` directory; isolated git config for builders and d-mcp |
| `tests/git_sources.rs`, `tests/approval_audit.rs`, `tests/two_machines.rs` | Integration tests with local bare repositories |
| `tests/redaction.rs`, `tests/json_contract.rs` | Cases for every new command and the approval view |
| `tests/fixtures/launchers/` | A pinned and an unpinned fixture per rule, plus wrapper cases |
| `tests/fixtures/git-hostile/` | Repository builders for a symlinked definition, a submodule under `servers/`, an oversized blob, leading-dash and `ext::` inputs, and a URL with a password or a query token |
| `docs/reference.md` | Source kinds, layout, pinning, refresh, approval view, approval, holding, revoke, audit rules and limits, trust, uninstall, JSON documents and codes, the manual adoption flow |
| `docs/adr/0006-hash-bound-approval-and-launcher-audit.md` | ADR, including the separate `approvals.toml`, the snapshot model and the approval-view exception |

## Implementation details

### Approach

1. **Process runner extension and guarded `git` operations**, with input validation, hardening and
   test-time isolation (`TestEnv::with_git()`).
2. **Config, store layout, lock and approvals files** with strict parsing.
3. **`add-git`**: temporary clone, commit verification, pin ref, tree listing, snapshot
   materialization, locked rename and writes; dry run in `TMPDIR`.
4. **Shared resolution loader** and routing of all six call sites.
5. **Refresh**: fetch outside the lock, rewrite detection, change set with audit findings and the
   approval view, locked apply; dry run in `TMPDIR`.
6. **Audit rules** as a table, each tested on pinned, unpinned and wrapper fixtures.
7. **Approvals, risk acceptance, revoke, trust and `select --all-from`**, all with `--dry-run` and
   `--json`.
8. **Resolver gate and sync integration** (held reasons, `revoked`).
9. **Status, doctor, uninstall.**
10. **Documentation and ADR.**

### Component structure

`approval` and `audit` are pure. `git` is the only module that runs `git`, and only mutating
commands and dry runs call it. `resolver` stays pure, with approvals and audit results passed in
as inputs by `load_resolution`.

### State management

- **New files:** `lock.toml` and `approvals.toml`.
- **New directory:** `sources/<id>/repo` (clone) and `sources/<id>/snapshot/servers` (accepted
  definitions with a commit stamp).

### API integration

`git` only, through the guarded runner. No `gh` dependency in v1.

### Styling approach

Not relevant: a command-line tool.

### Accessibility

Plain-text output that does not depend on colour.

### Edge cases

- **Unreachable URL, failed authentication, or an unknown SSH host:** a classified error code
  (`git-unreachable`, `git-auth-failed`, `git-host-unknown`) and no prompt.
- **A password in the URL, user info on `https`/`file`, a query or fragment:** rejected without
  echoing the secret part.
- **`git@host:org/repo.git` and `ssh://git@host/...`:** accepted.
- **A leading-dash URL or ref, or `ext::`:** rejected.
- **A force-push or a moved tag:** refused without `--allow-rewrite`; the old pin stays reachable.
- **A deleted ref:** `source-ref-deleted`.
- **A clone that fails midway or is killed:** no half-registered source; leftover `sources/.tmp-*`
  are swept by the next mutating command.
- **A crash between writes:** the order clone → rename → `lock.toml` → `config.toml` ensures that
  a registered source always has a pin; an unregistered `sources/<id>` is replaced on `add-git`.
- **A corrupted clone or snapshot:** read-only commands report the source unavailable
  (`hold(source-unavailable)`); `refresh` re-clones. If the pin is unreachable after the re-clone,
  `source-commit-unreachable`.
- **Offline:** read-only commands work unchanged (no git at read time).
- **A refresh racing `source remove` or another refresh:** `source-changed` (exit 3).
- **A symlinked definition, a submodule or an oversized blob:** a per-file problem. Nothing outside
  the repository is read.
- **A definition removed upstream:** listed by refresh. Afterwards it is "selected but not
  offered" (*Held* keeps an owned entry).
- **A pending winner over an approved lower-priority definition:** held, no fall-through.
- **A formatting-only upstream change:** same `definition_hash`, no re-approval.
- **A rollback to an earlier approved commit:** approved (hash set).
- **New audit rules after an update of d-mcp:** the findings hash changes; the server becomes
  `hold(risk-not-accepted)` until the acceptance is renewed.
- **A refresh between preview and `approve`:** `approval-changed` (exit 3) via `--expect`.
- **`approve --all-from` on an untrusted source:** refused.
- **`uninstall --purge` with `local/.git`:** `purge-blocked` (`local-is-git`) without
  `--include-local-repo`.

## Acceptance criteria

- [x] `mise run check` passes, and coverage stays at or above `coverage-threshold`.
- [x] Two-machine test: two stores and one local bare repository. Machine 2 runs exactly the
      documented sequence (`source add-git --trusted [--commit]`, `approve --all-from --expect`,
      `select --all-from`, `sync`). Both stores then give identical `resolve --json` desired
      entries (name, harnesses, `definition_hash`) and identical rendered entries in isolated fake
      harness homes.
- [x] After an upstream change and refresh, the server is `hold(pending-approval)`. Sync leaves the
      existing harness entry byte-identical until approval, then updates it.
- [x] After `revoke`, the next sync removes the owned entry; a lower-priority definition does not
      take its place.
- [x] `resolve`, `status`, `inventory` and `sync --dry-run` agree on a pending, a revoked and a
      risk-not-accepted server.
- [x] With the clone's remote unreachable, `resolve`, `status`, `doctor` and `inventory` succeed and
      start no `git` process; the store stays byte-identical.
- [x] Every launcher rule has a pinned fixture that passes and an unpinned fixture that blocks.
      Wrappers need risk acceptance. An acceptance stops working when the findings hash changes.
- [x] `approve` and `accept-risk` without `--expect` fail; with a stale `--expect` they fail with
      `approval-changed`.
- [x] The approval view shows `command` and `args` of a Git-source definition; redaction canaries
      in env, header and override values never appear in any output.
- [x] Hostile-repository tests show that the following are rejected or neutralized:
      - a symlinked definition;
      - a submodule;
      - an oversized blob;
      - a leading-dash URL or ref;
      - `ext::` transport;
      - a password in the URL and a token in the query.
- [x] Canary `reference-transaction` and `post-checkout` hooks configured through the test's git
      config or template do not run during `add-git` or `refresh`.
- [x] Rewrite, moved-tag and deleted-ref cases behave as specified, and the old pin stays
      reachable after a refused rewrite.
- [x] A failure-injection test for a clone failing midway shows no half-registered source.
- [x] `--dry-run` on `add-git`, `refresh`, `trust`, `select --all-from`, `approve`, `accept-risk`
      and `revoke` leaves the store byte-identical.
- [x] Trust, `--all-from` and `revoke` behave as specified. Directory-source audit findings do not
      block.
- [x] `uninstall --purge` deletes `sources/` and is blocked by `local/.git` without
      `--include-local-repo`.
- [x] Every new error code is in the reference table; every new command has a JSON-contract and a
      redaction test.
- [x] No test uses the network. Git config is fully isolated, and only the `git` binary is on the
      test `PATH`.

## Validation plan

- Integration tests with local bare repositories and the hostile-repository builders, via
  `TestEnv::with_git()`.
- Table-driven unit tests for the audit and the URL validation.
- A guard unit test on the adapter override allow-lists.
- A manual check with a private GitHub repository as a personal source on two stores, run only
  with HOME, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `D_MCP_STORE` pointing to fresh temporary
  directories.

## Assumptions and open points

- Trust has only the batch-approval effect in v1.
- Further launchers are added as table rules without schema changes.
- Definition repositories are small, so a full clone and a full re-download per dry run are
  acceptable.
- The runner's grandchild limit (a surviving `ssh` after a timeout) is accepted and documented.

## Test results

**Date:** 2026-10-31

- `mise run check:full` (fmt, clippy `-D warnings`, all tests, coverage, `cargo deny`): passed;
  line coverage 97.17 % (threshold 85), region 96.47 %, function 96.20 %.
- MSRV `cargo +1.94 check --locked --all-targets --all-features`: passed.
- `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --locked`: passed.
- Full test suite with a symlinked `TMPDIR` (macOS `/var` → `/private/var` case): passed.
- Every acceptance criterion has a dedicated test. Main suites: `tests/git_sources.rs`,
  `tests/git_hardening.rs`, `tests/approval_audit.rs`, `tests/approval_hardening.rs`,
  `tests/two_machines.rs`, `tests/git_health.rs`, `tests/redaction.rs`, `tests/json_contract.rs`,
  `tests/uninstall.rs`, the audit table tests in `src/audit/tests.rs` and the launcher fixtures in
  `tests/fixtures/launchers/`. All Git tests use local bare repositories through
  `TestEnv::with_git()`; no test uses the network.

### Implementation deviations

Recorded during the build; all are reflected in `docs/reference.md` and ADR 0006.

- Rejected `servers/` entries are materialized as placeholders so the unchanged directory scanner
  reports the same rule codes at read time. Valid definitions and keyed placeholders are written
  first; on a case-insensitive file system a colliding keyless placeholder is skipped (its notice
  is lost — documented limit).
- `source add-git`: `--priority` is optional (one above the highest user source) and `--id`
  defaults to the last URL segment. Clones use `--no-local --template=` in addition; the followed
  ref is recorded fully qualified (`refs/heads/…` or `refs/tags/…`) as an additive `ref` key in
  `lock.toml`. A missing pin counts as a rewrite. `servers/` is capped at 1000 entries.
- SSH batch mode applies to every network call (also after an `insteadOf` rewrite) unless the user
  configured an SSH command. User git config stays honoured; ancestry, replace, shallow, graft,
  quarantine and trace variables are stripped and `GIT_NO_REPLACE_OBJECTS=1` is set.
- Staging clones are created as `.staging-*`, locked through an `.owner` file and then renamed to
  `.tmp-*`; the sweep skips locked directories. A re-clone moves the old source aside and restores
  it on failure.
- Additional error codes `expect-required` and `reason-invalid` (exit 1). All approval commands
  reject invalid server names before reading anything, and `approvals.toml` is never written in a
  form its parser would reject.
- Audit: additional blocking rules `unparsed-launcher` (unknown options or value-taking short
  clusters fail closed) and `execution-env` (literal values of execution-affecting env variables;
  the finding names the variable only). Commits pin only Git transports in ref position; plain
  archive URLs are always unpinned. Shell, `env` and pass-through wrappers (`timeout`, `nice`,
  `nohup`, `stdbuf`, `setsid`, `corepack`) are unwrapped one level; anything unclear is
  `opaque-command`.
- Revoked markers store the source URL for the orphan report but block under any URL, and a marker
  whose source is no longer a registered Git source still revokes a server nothing else offers.
- `approve --all-from` also blocks invalid and revoked servers. `refresh` and `trust` on a
  directory source warn and exit 0. `accept-risk` acts on the winning source (no `--source`).
- Doctor's `pending-approval` check also covers `risk-not-accepted`; `inventory` keeps held and
  revoked owned entries as `managed`; `status` lists selected revoked servers without a record.
- `uninstall` loads `lock.toml` and `approvals.toml` before any write; a symlinked `local` is only
  unlinked and never blocks the purge.

## Review findings

**Date:** 2026-10-31
**Reviewer:** effective-flow-rust-reviewer (git and sources; approval and audit; commands, tests
and docs), plus one re-review of the fixes

### Summary

| Status | Count |
|---|---:|
| Fixed | 34 |
| Open / Not implemented | 0 |

One further note (non-Unix snapshot path handling) was closed as out of scope: Windows is not a
v1 target.

## Plan review

**Result:** Approved

Second round (2026-10-31), the re-planning gate against the merged WP1–WP4 code (origin/main
03aa2a4). The first round (2026-10-26, before any code existed) found 2 critical, 14 important and
7 notes, all incorporated; its decisions that still hold are kept above.

### Summary

| Area | Critical | Important | Note |
|---|---:|---:|---:|
| Architecture | 1 | 3 | 1 |
| Security | 1 | 5 | 1 |
| Data protection | 0 | 1 | 1 |
| Error cases | 0 | 4 | 0 |
| Testability | 0 | 2 | 0 |
| Scope | 0 | 1 | 2 |
| Maintainability | 0 | 1 | 2 |

### Findings

- **Security / Data protection, Critical.** The redaction layer hides `command` and `args`, so
  approval would have been blind. Decided: an own approval view for Git-source definitions, a
  documented exception in ADR 0006 with canaries.
- **Architecture / Error cases, Critical.** A partial clone read at resolve time would have made
  read-only commands fetch over the network and write into the store. Decided: a snapshot
  materialized at add/refresh; no git at read time.
- **Architecture, Important.** The resolver would have needed `state.toml` to choose between hold
  and not-desired. Adjusted: always `hold(pending-approval)`; the existing planner behaviour covers
  unowned servers; exhaustive `desired_for`.
- **Architecture, Important.** The source kind was named `local` and the Git `config.toml` schema
  was missing. Adjusted: `SourceKind::Git`, fields, derived location, source-object extension.
- **Architecture / Error cases, Important.** The store layout, `init` and `--purge` did not know
  `sources/`. Adjusted.
- **Maintainability / Architecture, Important.** Six copies of the resolution pipeline. Adjusted:
  one shared loader plus an agreement test.
- **Error cases / Architecture, Important.** The runner drops stderr and its timeout does not fit
  git. Adjusted: `classify_stderr`, network timeout, documented grandchild limit.
- **Testability, Important.** `TestEnv` has an empty `PATH`. Adjusted: `TestEnv::with_git()`.
- **Testability / Maintainability, Important.** Codes, exit codes and JSON documents were
  unspecified. Adjusted: "JSON and codes".
- **Security / Error cases, Important.** No resolve-time gate for changed audit findings.
  Adjusted: `hold(risk-not-accepted)`.
- **Security, Important.** Under hold semantics `revoke` left the server running. Decided: revoke
  removes.
- **Security, Important.** Approval was not bound to the previewed hash. Adjusted: `--expect`,
  `approval-changed`.
- **Error cases / Testability, Important.** Dry runs would have written into the store clone.
  Decided: temporary fetch under `TMPDIR`.
- **Error cases, Important.** A rewrite could garbage-collect the pin, and refresh could race
  removal. Adjusted: `refs/d-mcp/pin`, `gc.auto=0`, `source-changed`.
- **Error cases, Important.** Multi-file atomicity and corrupt files were unspecified. Adjusted:
  write order, sweep, strict parse.
- **Security / Error cases, Important.** The user-info rule rejected standard SSH remotes and
  ignored query tokens. Adjusted.
- **Security / Testability, Important.** Hook hardening covered only the clone, and the hook test
  proved nothing. Adjusted: hardening on every call, canary hooks via git config.
- **Scope, Important.** The concept's "one setup run, one sync" was unreachable. Decided:
  `select --all-from`.
- **Data protection, Important.** `--purge` would delete a `local` Git working copy. Decided:
  `purge-blocked` (`local-is-git`) without `--include-local-repo`.
- **Security, Note.** The context contradicted the protocol allow-list. Adjusted.
- **Maintainability / Error cases, Note.** The tree listing diverged from the directory scanner.
  Adjusted: non-recursive listing with the scanner's semantics.
- **Maintainability, Note.** Auditing each adapter's rendered input was redundant. Adjusted: guard
  test instead.
- **Scope, Note.** Adoption into a personal Git source is already covered by WP4 plus a manual
  push. Adjusted: documented flow, nothing new in WP5.
- **Data protection, Note.** The free-text reason could carry secrets. Adjusted.
- **Architecture, Note.** Approval cardinality and orphan reactivation. Adjusted: hash set, URL
  binding.
- **Scope / Maintainability, Note.** Doctor lacked git, approvals and lock checks; the re-planning
  gate is now closed. Adjusted.

## Open points

- No open points.
