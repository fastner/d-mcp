# ADR 0006: Hash-Bound Approval and Launcher Audit

- Status: Accepted
- Date: 2026-10-31
- Source: concept `docs/concept/2026-10-26-d-mcp.md` (decision "Hash-bound approval with a
  blocking audit for unpinned launchers"), WP5 plan "Git sources, approval and audit"

## Context

Git sources bring definitions from repositories the user does not fully control: a team
repository, or their own personal repository on a second machine. A definition is a command d-mcp
writes into a harness, which then runs it with the user's credentials. A changed upstream
definition must therefore never reach a harness without the user seeing and approving it, and the
common supply-chain hole of MCP setups, a launcher that installs whatever version is current
(`npx pkg`, `uvx pkg`, `docker run image:latest`), must be visible and blocking.

At the same time d-mcp must stay usable offline and safe to run often: `resolve`, `status`,
`inventory`, `doctor` and `sync` run constantly and must not reach the network, run repository
hooks or write into the store. And everything d-mcp prints goes through a redaction layer with
default deny, which hides `command` and `args`, the very fields an approval is about.

## Decision

**Snapshots, no `git` at read time.** A Git source is cloned (full clone, no checkout) into
`<store>/sources/<id>/repo` and pinned to a commit in `lock.toml`, next to the followed ref
recorded fully qualified (`refs/heads/<ref>` or `refs/tags/<ref>`), so a refresh never takes a
same-named tag for a followed branch or the other way round. The definitions under
`servers/` at that commit are materialized into `sources/<id>/snapshot/servers/` with a stamp
file holding the commit, keeping the directory scanner's per-file rules. `git` runs only in
`source add-git` and `source refresh` (and their dry runs, which clone into `TMPDIR`); read-only
commands and `sync` scan the snapshot like a directory source and treat a missing snapshot, a
missing pin or a stamp that differs from the pin as an unavailable source. Every `git` call is
hardened (no hooks, no fsmonitor, `transfer.fsckObjects`, no automatic gc, an allow-list of
`https:ssh:file`, no prompts, SSH in batch mode on every network call unless the user configured
an SSH command, since an `insteadOf` rewrite can turn any URL into an SSH one, and no inherited
variable that redirects, rewrites or traces git) and its output is never shown. The user's git
configuration (proxies, CA bundles, credential helpers, `insteadOf`) is honoured on purpose; the
hardening options are applied after it and win. The process runner kills only the
direct child on a timeout; a surviving `ssh` or `git-remote-https` grandchild is an accepted,
documented limit.

**Approval is bound to the `definition_hash` and the source.** An approval records the source id,
the source URL, the server name and a *set* of approved `definition_hash` values (tag
`d-mcp-definition-v1`, from WP1). A Git-source winner resolves only when its current hash is in
that set under the current URL; otherwise it is `hold(pending-approval)`. The set makes a
rollback to an earlier approved commit free; the URL binding means that re-adding a source id for
another repository never inherits old approvals (an approval under the new URL replaces the old
one and drops the risks accepted under the old URL), and approvals of a removed source stay
inactive and are reported as `orphaned-approval`. A moved pin never approves anything. Stored URLs
are parsed with the syntax and transport checks only; the secret rules run when a URL is entered,
so a later, stricter heuristic never turns `approvals.toml` or `config.toml` corrupt. A held winner
shadows lower-priority definitions, so nothing falls through to a different definition while the
user decides.

**Apply what was previewed (`--expect`).** `approve` and `accept-risk` require `--expect` with
the `definition_hash` shown by their dry run (`approve --all-from` with the pinned commit), as
`adopt --expect` does for adoption. A refresh between preview and apply fails with
`approval-changed` (exit 3) and writes nothing.

**An approval view as the one exception to default deny.** For definitions from Git sources, the
refresh change set, `approve`, `accept-risk` and `resolve`'s findings show `command`, `args`, the
URL path and `harness.codex.cwd` on top of the redacted view. These fields have passed the WP1
secret rules, which reject likely secrets in exactly these positions. Env values, header values,
all other override values, the description, the client id and scopes stay redacted. A canary
suite proves that the redacted parts never reach any output. Without this exception approval
would be blind; with a broader one, literal values would leak into logs and chats.

**A table-driven, blocking audit of launchers.** Each valid definition's neutral `command` and
`args` are matched against a rule table for package runners (`npx`, `npm exec`, `pnpm dlx`,
`bunx`, `yarn dlx`, `uvx`, `uv tool run`, `pipx run`), container runners (`docker`, `podman`,
`nerdctl` `run` and `container run`), VCS and URL specs (pinned only by a commit of a Git
transport in ref position, never by a hex run elsewhere in a URL), and opaque commands (a shell's
`-c` after any leading options, `env -S`, a wrapper inside a wrapper). `env` and a small list of
pass-through wrappers (`timeout`, `nice`, `nohup`, `stdbuf`, `setsid`, `corepack`) are unwrapped
one level. The parser fails closed: an option its per-family tables do not know, a short option
cluster with a letter that takes a value, or an unknown global option that may have taken the
subcommand's place is `unparsed-launcher`, never guessed as a flag, because a guessed arity lets a
value pose as the package spec and hide the real one. A finding blocks approval of a Git-source definition until `accept-risk` records a reason and a
findings hash (rule ids and matched arguments) bound to the `definition_hash` and the URL. New
rules in a later d-mcp version change the findings hash and turn an approved server into
`hold(risk-not-accepted)` until the acceptance is renewed. For directory sources findings are
warnings only. Override tables are not audited; a guard test fails if an adapter's allow-list ever
gains a key that runs a command.

**Execution-changing env values are a finding, not a view exception.** Env values stay redacted in
the approval view, so a literal `NODE_OPTIONS`, `LD_PRELOAD`, `DYLD_*`, `PYTHONPATH`,
`NPM_CONFIG_*` or package index variable could change an approved, pinned command unseen. Instead
of showing env values (which would leak literals into logs and chats), a literal value for a
deny-listed name (or an `env` wrapper assignment) is the blocking rule `execution-env`. The finding
names the variable, never the value; the value is bound through the `definition_hash`, so a
changed value needs a new approval and acceptance. A forwarded variable carries the user's own
value and is not a finding.

**Revoke removes.** `revoke` drops the approvals and accepted risks of a (source, server) and
records a revoked marker. The server resolves to `revoked`, which sync treats like an unselected
server: owned entries are removed, and no lower-priority definition takes over. The marker wins
over every hold, so what was revoked is removed even when its definition turns invalid upstream,
its source becomes unavailable, or it (or its whole source, via `source remove`) is removed while
nothing else offers the name. The marker records the source URL, but, unlike an approval, keeps
blocking under any URL (fail closed: a revoke is never undone by re-adding the source id); the URL
only decides whether the marker is reported as `orphaned-approval`. A later `approve` clears the
marker.

**A separate `approvals.toml`.** The concept's data outline keeps the approved hashes in the lock
file. d-mcp instead keeps `lock.toml` for pins only (the commit and the followed ref of each Git
source) and stores approvals, revoked markers and
accepted risks in `approvals.toml`, like Dalo's separate approval store. Pins change on every
refresh, a mechanical operation; approvals change only on a user decision, carry a free-text
reason, and must survive a source being removed and re-added. Separate files keep the two
lifecycles and their corruption cases apart (`lock-corrupt`, `approvals-corrupt`); both follow the
`config.toml` rules: strict parse, atomic writes under the store lock, never rewritten when
corrupt.

**Trust is narrow.** A Git source is untrusted by default. In v1, trust only allows
`approve --all-from <source> --expect <commit>`, which approves the clean servers of the pinned
commit at once; together with `select --all-from` it makes a second machine one setup run plus
one sync. Trust never skips the audit and never approves a future commit.

## Consequences

- Read-only commands work offline and never start `git`; a corrupt clone or snapshot only makes
  its source unavailable until a refresh. Dry runs pay a full re-download, accepted because
  definition repositories are small.
- Every upstream change of a definition needs a new approval; formatting-only changes keep the
  `definition_hash` and do not.
- The approval view prints commands and arguments of Git-source definitions. The secret rules are
  the guard for these fields; a secret they miss (a short password in an argument) can appear
  there. This is the accepted price of a non-blind approval.
- The audit covers common launchers, not arbitrary binaries, scripts or remote servers, and not
  what a pinned package does. It raises the bar; it is not a sandbox. Failing closed on unknown
  options means an unusual but harmless launcher needs `accept-risk`; the tables grow with the
  launchers' option lists. The `execution-env` deny-list is a list, not a proof: an execution-
  changing variable it does not name, or a configuration file a launcher reads, is not seen.
- Approvals and revoked markers of removed sources accumulate in `approvals.toml` until removed by
  hand (or, for a marker, cleared by `approve`); `doctor` and `resolve` report them.
- d-mcp never writes, commits or pushes into a Git source in v1. Adopting into the personal source
  is adoption into `local` followed by a manual `git commit` and `git push`, and a `local/` that
  is a Git repository blocks `uninstall --purge` unless `--include-local-repo` is given (a
  symlinked `local` is only unlinked and never blocks).
