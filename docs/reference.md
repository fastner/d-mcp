# d-mcp reference

This is the reference for d-mcp: the store, the definition format, the secret rules, sources,
selections, resolution, override validation, sync with the Codex and Claude Code adapters, the
write state, exit and error codes, and the JSON documents. It also covers the discovery of every
harness scope, the inventory, the redacted view, display hashes, adoption, status, doctor and
uninstall. Later work packages (approvals) build on it.

## Commands

| Command | Kind | Description |
|---|---|---|
| `d-mcp init` | mutating | Create the store, or complete a partial one. Idempotent. |
| `d-mcp source add <path> --id <id> --priority <n>` | mutating | Register a local directory as a source. |
| `d-mcp source list` | read-only | List all sources, including the built-in `local` source. |
| `d-mcp source remove <id>` | mutating | Unregister a source. |
| `d-mcp select <server> [--harness claude\|codex]...` | mutating | Select a server for harnesses. |
| `d-mcp unselect <server>` | mutating | Remove a selection. |
| `d-mcp resolve` | read-only | Resolve every selection against the sources. |
| `d-mcp inventory [--harness claude\|codex]...` | read-only | List every entry of every harness scope d-mcp can read, classified, with duplicates and conflicts (see [Inventory](#inventory)). |
| `d-mcp adopt <harness>:<scope>:<name>... [flags]` | read-only; mutating with `--apply` | Take existing harness entries over as definitions of the `local` source, preview first (see [Adoption](#adoption)). |
| `d-mcp sync [--harness claude\|codex]... [--take-over <server>]... [--relocate]` | mutating | Write the selected servers into the harnesses (see [Sync](#sync)). |
| `d-mcp unmanage <server> [--harness claude\|codex]...` | mutating | Stop managing a server's harness entries (see [Unmanage](#unmanage)). |
| `d-mcp status [--auth] [--harness claude\|codex]... [<server>...]` | read-only | Show the desired, recorded and actual state, the next sync operation, missing variables and, with `--auth`, the login state (see [Status](#status)). |
| `d-mcp doctor [--auth]` | read-only | Check the store, the harnesses and the managed entries; exit 1 on an `error` check (see [Doctor](#doctor)). |
| `d-mcp uninstall [--apply] [--purge [--follow-store-link]]` | read-only; mutating with `--apply` | Remove the entries d-mcp created, release adopted ones, and optionally delete the store (see [Uninstall](#uninstall)). |

Global options, accepted before or after the command:

- `--store <path>`: the store directory (see [Store location](#store-location)).
- `--json`: print exactly one JSON document on stdout (see [JSON documents](#json-documents)).
- `--dry-run`: show what a mutating command would do. A dry run takes no lock and creates, changes
  or deletes no file or directory, `.lock` included. On a read-only command, `--dry-run` has no
  effect and prints a warning on stderr; stdout stays valid JSON.

Mutating commands take the store lock, then read `config.toml`, then write. A mutating command on
an uninitialized store fails before it tries to lock. Every mutating command (not a dry run)
creates a missing `.hash-key` under the lock (see [Display hashes](#display-hashes)). Read-only
commands take no lock, create nothing, and succeed while another process holds it.

## Store

### Store location

The store path comes from the first of these that is set:

1. the `--store` flag;
2. the `D_MCP_STORE` environment variable (an empty value counts as unset);
3. `$HOME/.d-mcp`.

A leading `~` (alone or followed by `/`) is expanded with `HOME` in the first two; `~user` is not
expanded. Relative paths are resolved against the current directory. d-mcp reads `HOME` directly
and does not fall back to the password database. If `HOME` is unset or empty and is needed, the
command fails with `home-unset` (exit 4).

### Layout

```text
<store>/
├── config.toml        schema version, user sources, selections
├── state.toml         write state of `sync` (created on first write)
├── .hash-key          key of the display hashes (created by `init` or any mutating command)
├── .lock              advisory lock file
└── local/             root of the built-in local source
    └── servers/       definitions of the built-in local source
```

Directories are created with mode `0700`, files with mode `0600`. `sync` creates `state.toml` on
its first write; later work packages create `lock.toml` and `approvals.toml`. Every persisted file carries
`schema_version = 1`; another version fails with `schema-unsupported` (exit 3). `uninstall --apply --purge` deletes exactly
these entries (see [Uninstall](#uninstall)).

`init`:

- creates the missing parts of the layout and reports each created path, in creation order;
- is idempotent: on a complete store it creates nothing and succeeds;
- creates `.hash-key` with a fresh random key when it is missing, and never replaces an existing
  one (a corrupt one fails with `hash-key-corrupt`, exit 3);
- completes a partial store: a directory that holds only known store entries (`config.toml`,
  `local`, `.lock`, `state.toml`, `.hash-key`, `lock.toml`, `approvals.toml`);
- refuses a non-empty directory that is not a store with `foreign-directory` (exit 1);
- never rewrites an existing `config.toml`; a corrupt one fails with exit 3;
- follows a store root that is a symlink to a directory, as every other command does, so a
  dotfile-managed `~/.d-mcp` works; a dangling symlink or a link to a non-directory fails with
  `store-path-invalid` (exit 1);
- warns on stderr when an existing store root is accessible by group or others (mode looser than
  `0700`) and leaves its mode unchanged;
- with `--dry-run`, reports what it would create and creates nothing, not even on a missing store.

A store directory without `config.toml` is *not initialized*: every command except `init` fails
with `store-not-initialized` (exit 1).

### `config.toml`

```toml
schema_version = 1

[[sources]]
id = "team"
kind = "directory"
path = "/home/me/team-mcp"
priority = 10

[[selections]]
server = "github"
harnesses = ["all"]

[[selections]]
server = "linear"
harnesses = ["claude", "codex"]
```

- `sources`: user sources with `id` (name rule, not `local`), `kind` (`directory`), `path` (absolute
  and canonical) and `priority` (an integer of at least 1). Ids, paths and priorities are unique.
- `selections`: `server` (name rule, unique) and `harnesses`, either `["all"]` or a non-empty,
  duplicate-free list of `claude` and `codex`. The scope is always `user`.
- d-mcp writes the file atomically and in canonical form: sources sorted by id, selections sorted
  by server.
- Any deviation (syntax, unknown keys, wrong types, invalid or duplicate values) makes the file
  corrupt: `config-corrupt` (exit 3), reported by line, column, field path and rule code. d-mcp
  never rewrites a corrupt file; fix it by hand.

### Built-in local source

The built-in source has the id `local`, the root `<store>/local` and priority 0. It is implicit:
it is not persisted in `config.toml`, and its path is always derived from the active store. It
cannot be added, removed or re-prioritized (`source-id-reserved`).

### Locking and atomic writes

- The lock is an advisory `flock`-style lock on `<store>/.lock`, taken with `File::try_lock`. A
  held lock is retried after 0, 25, 50, 100, 200, 400 and 800 ms (about 1.6 seconds in total),
  then the command fails with `store-locked` (exit 3). On Unix, `.lock` is opened with
  `O_NOFOLLOW`: a `.lock` symlink is refused with `io-error` (exit 4) instead of being followed.
- Every write goes to a temporary file in the same directory (mode `0600`), is flushed and synced,
  is persisted over the target with a rename, and is followed by an fsync of the parent
  directory. An interrupted write leaves the old file intact. If only the final fsync of the
  parent directory fails, the new content is already in place: the command succeeds and prints a
  warning on stderr that the change may not survive a crash.
- A rename-based replace creates a new file: it keeps the permission bits d-mcp sets, but not the
  owner, group, extended attributes or ACLs of the file it replaces. When an existing harness file
  is owned by another user or group than the d-mcp process, the write still succeeds and prints a
  `warning:` line that the owner and group changed.

## Definition format

A source root contains `servers/<name>.toml`, one server per file.

### Normative examples

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

### Fields

| Field | Required | Description |
|---|---|---|
| `name` | yes | The neutral name. Must equal the file stem. |
| `description` | no | Human-readable description. |
| `transport.type` | yes | `stdio` or `http`. |
| `transport.command` | stdio | A bare program name or an absolute path. |
| `transport.args` | no | Array of strings. |
| `transport.url` | http | The server URL. |
| `env` | no | stdio only. Table of variable name to `{ forward = true }` or `{ value = "..." }`. |
| `auth` | no | http only. The auth method. |
| `harness.codex`, `harness.claude` | no | Per-harness override tables; each is checked against its adapter's allow-list (see [Override validation](#override-validation)). |

The `auth` forms:

- `type = "oauth"` with optional `client_id`, `scopes` (array of strings) and `client_secret_env`
  (a variable name; requires `client_id`). Tokens never leave the harness.
- `type = "bearer_env"` with `var`, and optionally `oauth_supported = true`.
- `type = "header_env"` with `header` and `var`.
- `type = "none"`.

### Defaults

When omitted:

- `args` is `[]` and `env` is `{}`;
- `auth` is `{ type = "oauth" }`;
- `scopes` is `[]`;
- `oauth_supported` is `false`;
- empty override tables are dropped.

Writing one of these defaults explicitly is equivalent to omitting it; both hashes stay the same.

### Field rules

- `env` is valid only for `stdio`; `auth` and its fields only for `http` (`transport-mismatch`).
- Unknown keys in any core table are an error, reported with the field path (`unknown-key`).
  Only `harness.codex` and `harness.claude` are opaque; any other key under `harness` is unknown.
- The stdio working directory exists only as `harness.codex.cwd`, because Claude Code has no
  equivalent.
- Variable names (env keys, `var`, `client_secret_env`) match `^[A-Za-z_][A-Za-z0-9_]*$`
  (`variable-name`). Header names are RFC 9110 tokens (`header-name`).
- `client_secret_env` requires `client_id` (`client-secret-without-client-id`).
- `command` must be non-empty and either a bare program name or an absolute path; `~` and relative
  paths are rejected (`command-rule`).
- An env entry is exactly one of `{ forward = true }` and `{ value = "<string>" }`
  (`invalid-env-entry`). Renamed forwards are not modelled.
- Floats are allowed only in override tables; NaN and infinity are rejected (`non-finite-float`).

### Name rule

Server names and source ids use lowercase letters, digits, `-` and `_`. A name starts with a letter
or digit, does not end with `-` or `_`, contains no `__`, and has at most 32 characters
(`name-rule`). The limit follows from a 64-character model tool-name limit minus `mcp__` and `__`,
which leaves at least 25 characters for tool names. Claude Code 2.1.310 validates tool names
against `^[a-zA-Z0-9_-]{1,64}$` (found in the binary; not observed through a model call).
Reserved names are adapter-specific (see [Claude Code mapping](#claude-code-mapping)).

### URL rules

- The `url` field must be URL-shaped, `scheme://host...` (`url-invalid`). It must not contain
  whitespace or control characters, a `[` that opens an IPv6 host must be closed by `]` (followed
  only by an optional port), and a port must be a number from 0 to 65535; an empty port
  (`host:`) is allowed, as in RFC 3986.
- The scheme must be `https`. `http` is allowed only for the loopback hosts `localhost`,
  `127.0.0.0/8` and `::1` (`url-scheme`).
- The `url` field must not carry a query (`url-query`).
- User info (`user:password@`) is rejected in any URL-shaped value, in every checked position
  (`url-userinfo`). A token carries user info when it contains `scheme://` anywhere, also after a
  prefix such as `DATABASE_URL=` or `-dsn=`, and either the authority (the text after `://` up to
  the first `/`, `?` or `#`) contains `@`, or the authority contains a `:` whose following text up
  to that delimiter is not a port and an `@` follows later. A port is digits only and at most
  65535; an empty port counts as a port, and the colons of a bracketed IPv6 host are skipped. The
  second form catches passwords that contain `/`, `?` or `#` (`postgres://u:p/ss@db/app`,
  `postgres://u:pa#ss@db/app`), while a port followed by an `@` in the path
  (`https://mastodon.example.com:443/@me/mcp`, `http://localhost:8080/pkg@1.0`) and colons in a
  query (`https://h.example.com/x?t=10:00&e=a@b`) pass. The Go DSN form
  `user:password@tcp(host:3306)/db`, an `@` preceded by a `:`, not preceded by any `/`, and
  followed by a protocol name and `(`, counts as user info too. This covers database connection
  strings in URL and Go DSN form. Key-value connection strings (libpq `host=db password=x`) are
  caught by the name heuristic instead. Known false negative: an all-digit password followed by
  `/`, `?` or `#` (`postgres://user:1234/x@db/app`) reads as a port and is not caught. In the
  `url` field, user info is checked before the shape, so a password that breaks the shape is
  still reported as `url-userinfo`.

### Scanning and conflict keys

- Only regular files are accepted; symlinks are not followed, and FIFOs and other special files are
  rejected (`not-regular-file`). On Unix, definition files are opened with
  `O_NOFOLLOW | O_NONBLOCK` and re-checked through the open handle, so a file swapped for a
  symlink or a FIFO after the first check is neither followed nor waited on.
- Files over 64 KiB are rejected (`file-too-large`). Files must be UTF-8 (`not-utf8`).
- Hidden files (`hidden-file`), non-`.toml` files (`non-toml-file`) and subdirectories
  (`subdirectory`) are ignored and reported as notices.
- The conflict key is the file stem, as long as the stem passes the name rule. A malformed
  `servers/foo.toml` therefore still shadows `foo`. A stem that breaks the name rule is a per-file
  problem (`invalid-stem`) and shadows nothing.

## Secret rules

Definitions hold secret *references* only: env forwards and the auth variables. Literal secrets
are rejected. The heuristics produce false positives and false negatives. Only an `env` literal
has a remedy for a false positive: turning it into a forward. In `args`, `transport.url` and
override strings a false positive is a hard rejection; there is deliberately no "allow literal"
escape.

### Variable syntax

`$VAR`, `${VAR}`, `${env:VAR}` and `{env:VAR}` are rejected (`variable-syntax`) in `command`,
`args`, `url`, env literal values and override-table strings.

### Name heuristic (`secret_name_heuristic`)

A name is split at `_` and `-`. It matches when one segment equals, case-insensitively, `TOKEN`,
`SECRET`, `PASSWORD`, `PASSWD`, `PASS`, `APIKEY`, `CREDENTIAL`, `CREDENTIALS`, `COOKIE` or `AUTH`,
or when the segment pair `API` `KEY` occurs. It runs on (`secret-name`):

- env literal names (forwards may have any valid variable name);
- `--flag` names in `args` (`--api-key short` and `--token=x` are both rejected);
- override-table keys whose value is a string (`[harness.codex.env]` `GITHUB_TOKEN = "x"`),
  except env-reference keys (see below);
- URL query keys of URL-shaped values, including URLs embedded in a token
  (`jdbc:postgresql://db/app?password=x`);
- the left side of a `NAME=value` token in any position the value heuristic checks, with leading
  `-` removed. For a flag (`-password=x`, `--token=x`) it always runs. For a bare `NAME=value`
  it runs only when the value is not a plain word `[A-Za-z0-9_.-]{1,31}`, so `AUTH_MODE=oauth`
  passes and `GITHUB_TOKEN=hunter2!` is rejected. A bare name that is, case-insensitively, exactly
  `password`, `passwd`, `pwd`, `secret` or `token` is rejected whatever the value (libpq
  `password=hunter2`). A plain-word value is still checked by the value heuristic, so
  `GITHUB_TOKEN=ghp_x` is rejected as `secret-prefix`.

Env-reference keys in override tables skip the name heuristic. A key is an env reference when its
string value is a variable name, `[A-Za-z_][A-Za-z0-9_]*`, and either the key's last `_` or `-`
segment is, case-insensitively, `env` or `var` (`bearer_token_env_var`, `client_secret_env`,
`token_env`), or its parent table is one of the env-reference tables, currently only
`env_http_headers` (`[harness.codex.env_http_headers]` `X-Auth-Token = "MY_TOKEN_VAR"`). Keys in
an `env` table (`[harness.codex.env]`) hold literals and are never exempt, at any depth: tables
nested in it (`[harness.codex.env.sub]`) and tables inside an `env` array
(`[[harness.codex.env]]`, `env = [{ token_env = "x" }]`) are covered too. The value heuristic
still runs on an exempt value, so `bearer_token_env_var = "ghp_x"` is rejected as
`secret-prefix`. Known false positives: plain words that contain a secret segment, such as
`pass_through = "x"` or `token_mode = "x"`, are rejected.

### Value heuristic (`secret_value_heuristic`)

A value is split at whitespace and every token is checked. A token matches when it meets any of:

| Rule | Code |
|---|---|
| Starts with a known prefix followed by at least one character: `ghp_`, `gho_`, `ghu_`, `ghs_`, `github_pat_`, `glpat-`, `sk-`, `sk-ant-`, `xoxa-`, `xoxb-`, `xoxp-`, `xoxr-`, `xoxs-`, `AKIA`, `AIza` | `secret-prefix` |
| A JWT: starts with `eyJ` and contains at least two `.` separators | `secret-jwt` |
| A run of at least 32 hex characters, outside digest contexts | `secret-hex` |
| At least 32 characters with Shannon entropy of at least 4.0 bits per Unicode scalar | `secret-entropy` |

Digest contexts, which WP5 allows for pinning, are a hex run directly after `sha256:` (including
`@sha256:`), and a 40- or 64-character hex run directly after `@` or `#` (the commit of a VCS spec
such as `git+https://host/repo@<commit>` or `github:org/repo#<commit>`). Exempt digests are also
left out of the entropy input.

Every token is first checked for user info (see [URL rules](#url-rules)). Paths and URLs are
exempt from the entropy rule only as a whole:

- A URL-shaped token has its path segments, query values and fragment checked one by one, and its
  query keys with the name heuristic.
- A token that is not URL-shaped but contains `scheme://` (`jdbc:postgresql://db/app?...`,
  `--url=https://...`) gets the same URL checks on the substring starting at the inner scheme, in
  addition to the rules below.
- A compound token containing `=`, `;` or `,` is split there: the left side of `NAME=value` goes
  to the name heuristic (as described above), the right side is checked as a token of its own, and every piece is
  checked for the known prefixes and the JWT shape (`key=ghp_x`, `a,ghp_x` and `a;xoxb-1` are
  rejected).
- A token containing `/` is checked as a whole only when it is standard base64,
  `[A-Za-z0-9+/]+` with up to two trailing `=`, contains at least one digit or `+`, and does not
  start with `/`, `./`, `../`, `~/` or `@`. So a secret such as
  `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEYxxAB` is not split into harmless segments. Any other
  token containing `/` is path-like and checked segment by segment
  (`GoogleCloudPlatform/kubernetes-engine-samples`, `@scope/name@1.2.3`). Known false positive:
  a single path segment of at least 32 characters can reach the entropy threshold on its own
  (`src/ModelContextProtocolServerGithubReferenceImpl/index`).

The value heuristic runs on these positions:

- env literal values;
- every `args` element, plus the right side of `--flag=value`;
- URL path segments, and the path segments and query values of URL-shaped arguments;
- `description`, `client_id` and `scopes`;
- every string in the override tables.

### Errors never echo values

Parsing runs in two phases: into an untyped, spanned TOML document (`toml_edit`'s immutable
document), then validation by d-mcp's own walker, which tracks the field path and maps every
failure to a rule code. Errors show only file, line, column, field path and rule code; no parser
message text is ever printed. A key that itself looks like a secret is shown as `<redacted>` in the
field path. The walker stops at the first broken rule, so every file reports exactly one rule.

## Hashes

Both hashes are SHA-256 over a domain tag, a newline, and the canonical JSON of the model after
defaults, as 64 lowercase hex characters.

- `definition_hash`: tag `d-mcp-definition-v1`, the full model including `description` and the
  override tables. WP5 binds approvals to it.
- `core_hash`: tag `d-mcp-core-v1`, the model without `description` and without the override
  tables. `inventory` and `adopt` use it to detect duplicates across harnesses.

Canonical JSON has keys sorted byte-wise (d-mcp sorts them itself, independent of `serde_json`
features) and no whitespace. Its shape mirrors the TOML:
`name`, optional `description`, `transport` (`type`, `command`, `args` or `type`, `url`), `env` for
stdio, `auth` for http (always with its defaults; `client_id` and `client_secret_env` only when
set), and `harness` when an override table is non-empty. Value encoding:

- `scopes` is sorted and deduplicated;
- TOML datetimes become RFC 3339 strings; a `+00:00` offset is written as `Z`, while `-00:00`
  stays distinct because RFC 3339 gives it its own meaning (an unknown local offset);
- integers and floats stay distinct; floats use the shortest round-trip form.

Formatting, comments, key order, inline versus standard tables, `scopes` order and explicitly
written defaults do not change either hash.

### Entry hashes

Harness entries have their own hashes, used by the write state: the *sent* hash of an entry as
rendered and the *read-back* hash of the entry as read back from the harness. For the Codex adapter
both use the tag `d-mcp-codex-entry-v1` over the canonical JSON of the full `mcp_servers.<name>`
subtree, sub-tables included, so the two are comparable. Before hashing, values are canonicalized:

- keys with the value Codex treats as its default are removed, because Codex drops them when it
  re-saves its file (verified against Codex 0.165.0): `enabled = true`, `required = false`, and an
  empty `args`, `env_vars`, `env`, `env_http_headers`, `scopes` or `disabled_tools`. An empty
  `enabled_tools` is not a default (it allows no tool) and is kept;
- `env_vars` strings become `{ name = "...", source = "local" }`;
- `startup_timeout_ms` becomes `startup_timeout_sec` (when `startup_timeout_sec` is absent);
- integers and floats get one number form. This deliberately differs from the definition hashes,
  where they stay distinct, because Codex rewrites `30` as `30.0` when it saves its file;
- datetimes keep the `+00:00` to `Z` rule; keys are sorted.

The preserved keys `oauth.client_secret`, `oauth.callback_url` and `oauth.callback_port` are
excluded (and an `oauth` table that only held them). They are skipped while the entry is read, so
their values never enter any model, hash or output.

For the Claude Code adapter both hashes use the tag `d-mcp-claude-entry-v1` over the canonical
JSON of the whole `mcpServers.<name>` value, every key included, so they are comparable as well.
The same canonical form is applied to the rendering and to the entry read back, and it absorbs the
normalization Claude Code applies when it writes an entry (verified against Claude Code 2.1.310):

- `type = "stdio"` is Claude Code's default and is removed;
- an empty `args` (which Claude Code adds to every stdio entry), `env` or `headers` is removed;
- integral numbers get the integer form (`30000.0` becomes `30000`); keys are sorted.

Claude Code also drops keys it does not know; d-mcp never renders such keys. A normalization the
canonical form does not absorb makes the read-back verification fail (`harness-verify-failed`)
instead of silently losing ownership.

### Display hashes

The entry hash of an entry d-mcp does not own could be used to guess a low-entropy secret in it by
hashing candidates. Such hashes are therefore shown only *keyed*, as a display hash:
HMAC-SHA-256 under a random 32-byte key over the tag `d-mcp-display-v1`, a newline and the
adapter's entry hash, as 64 lowercase hex characters.

- The key lives in `<store>/.hash-key` (mode `0600`) as 64 lowercase hex characters and a newline.
  `init` creates it, and every mutating command creates it under the store lock when it is
  missing. Read-only commands never create it: without a key they show no display hashes and say
  to run `d-mcp init`. A key that is not a regular file or not 64 hex characters is
  `hash-key-corrupt` (exit 3) for mutating commands and is never replaced; read-only commands
  warn and show no display hashes.
- A plain entry hash (`hash` in JSON) is shown only for an owned record that d-mcp wrote itself
  (`written`). A record created by `--take-over` holds the foreign entry's hash and is shown keyed
  (`display_hash`), like every unmanaged and project-scope entry. A drifted entry shows its
  recorded `hash` and the `display_hash` of its changed content.
- A display hash changes when the key changes; it is meant to compare entries and to bind a later
  apply to a preview, never to be stored elsewhere.

## Sources and selections

- `source add <path> --id <id> --priority <n>` registers a directory. `~` and relative paths are
  resolved and the canonical absolute path is stored. `--priority` is required, an unsigned integer
  of at least 1, and must be unique. Rejections (exit 1): `source-id-reserved`, `invalid-name`,
  `priority-invalid`, `source-path-not-found`, `source-path-not-directory`, `source-path-not-utf8`,
  `source-path-in-store`, `source-id-taken`, `priority-taken`, `source-path-registered`. A
  non-integer or negative priority is a usage error (exit 2).
- A user source is `unavailable` when its root or its `servers/` directory is missing or
  unreadable. `source add` of such a directory succeeds with a warning.
- `source remove <id>` reports, in `no_longer_offered` and as warnings, the selected servers that
  the removed source offered and no remaining source offers. The selections themselves are kept.
- `select <server>` without `--harness` stores `all`, so future adapters are included.
  `--harness claude|codex` can be repeated; duplicates are collapsed. Re-selecting a server
  replaces its harness list. Selecting a name that no source offers is allowed, with a warning.
- `unselect <server>` removes a selection; an unknown one fails with `not-selected` (exit 1).

## Resolution

Resolution is a pure, deterministic function of the source snapshots and the selections. Sources
are ordered by descending priority. For every selected server it returns exactly one status:

| Status | Reason | When |
|---|---|---|
| `resolved` | | The highest-priority file for the name is valid. |
| `hold` | `source-unavailable` | An unavailable source has a higher priority than the current winner, or no winner exists while a source is unavailable. |
| `hold` | `invalid-winner` | The highest-priority file for the name is invalid (fail closed; a valid lower copy is not used). |
| `not-offered` | | No source offers the name and no source is unavailable. |

`source-unavailable` is checked before `invalid-winner`. `config.toml` keeps selections unique; if
the resolver is given duplicate selections of a server anyway, it keeps the one whose harness list
sorts first, independent of the input order. `sync` turns `hold` into its *Held* operation (keep the
existing managed entry unchanged), and WP5 adds the reason `pending-approval`.

## Override validation

`resolve` and `sync` run a `validate` step between the source scan and the resolver. Parsing stays
adapter-agnostic; `validate` checks each valid definition's `harness.<id>` table against that
adapter's allow-list, per-key types and ranges, and per-key transport restrictions. A definition
that breaks it becomes invalid with the rule `override-key` (a key that is not allowed),
`override-type` (a wrong type or a value outside its range) or `override-transport` (a key that is
not valid for the definition's transport), reported by line, column and field path. The resolver
then reports `invalid-definition`, and `invalid-winner` for a selected server: the server is held
for every harness, Claude Code included (fail closed). Because `resolve` and `sync` run the same
step, they agree, and d-mcp never writes a value that makes Codex refuse to load its whole
configuration.

The type check reads the TOML kind as written, so a datetime is never accepted as a string. A key
that is not on the allow-list is not echoed in the field path unless it is one of the deliberately
excluded keys.

The `harness.codex` allow-list:

| Key | Type | Transport |
|---|---|---|
| `cwd` | absolute path string (starts with `/`; no `~`) | stdio only |
| `enabled`, `required` | boolean | any |
| `startup_timeout_sec`, `tool_timeout_sec` | non-negative integer or float of seconds, at most `31536000` (365 days) | any |
| `enabled_tools` | non-empty array of strings | any |
| `disabled_tools` | array of strings | any |

These restrictions follow Codex 0.165.0, which refuses to load a configuration with `cwd` on a
streamable HTTP entry, or with a timeout it cannot convert into a duration (such as `1e300`). An
empty `enabled_tools` is rejected because Codex drops it when it re-saves its file, which would
turn "allow no tool" into "allow every tool"; to disable a server, use `enabled = false`.

Deliberately excluded in v1 for security reasons: `default_tools_approval_mode`, `tools`
(`tools.<tool>.approval_mode`), `http_headers_helper` (it runs a local command) and
`experimental_environment`. An owned entry that contains them counts as drift.

The `harness.claude` allow-list:

| Key | Type | Transport |
|---|---|---|
| `timeout` | integer milliseconds, `1000` to `31536000000` (365 days); Claude Code ignores values below one second | any |
| `oauth_callback_port` | integer port, `1024` to `65535` | any (used only with `auth.type = "oauth"`) |

Deliberately excluded: `headersHelper` and `headers_helper` (it runs a local command, like Codex's
`http_headers_helper`) and `env` (environment entries belong to the neutral definition). Any other
key is invalid. Since the Claude Code adapter exists, a definition with an unknown `harness.claude`
key, which was opaque before, is invalid too, and the server is held for every harness, Codex
included.

## Sync

`sync` writes every selected, resolved server into each harness that has an adapter (Codex and
Claude Code), at user scope. `--harness` limits it to the named harnesses (`claude`, `codex`); any
other value is a usage error (exit 2).

### Operations

For each harness, `sync` compares three things per entry name: what the resolver wants (the
*desired* entry, rendered), what the write state records, and what the harness file actually
contains.

| Operation | When | Effect |
|---|---|---|
| `add` | desired, not recorded, absent | Written. |
| `update` | the new rendering's hash differs from the recorded sent hash, and the actual hash equals the recorded read-back hash | Rewritten. |
| `remove` | not desired, recorded, the actual hash equals the recorded read-back hash | Deleted. |
| `drift` | recorded, the actual hash differs from the recorded read-back hash | Reported only (exit 1). |
| `collision` | desired, not recorded, a same-name entry exists | Reported only (exit 1). |
| `identical-collision` | a collision whose content equals the rendering | Reported only (exit 1); `--take-over` resolves it without a write. |
| `vanished` | recorded, desired, absent | Re-added, with the notice "removed outside d-mcp; run `d-mcp unmanage <server> --harness <harness>` to keep it removed". |
| `held` | the resolver holds the server (`invalid-winner`, `source-unavailable`), or a recorded, still-selected server is `not-offered` | The entry is left unchanged; `reason` names why. A held server without a record plans nothing. |
| `orphaned` | recorded, not desired, absent | The record is dropped. |
| `location-changed` | the recorded file differs from the resolved one (for example after `CODEX_HOME` or `CLAUDE_CONFIG_DIR` changed); the notice names the variable | Reported (exit 1); nothing is planned for that harness until `sync --relocate`. |
| `blocked` | the rendering has an `important` gap | Not written (exit 1). |
| `unsupported` | an owned entry was rewritten in a style d-mcp does not edit (Codex: an inline or dotted-key table; Claude Code: not a JSON object) | Left untouched (exit 1). |
| `unchanged` | nothing to do | A changed definition hash with an identical rendering only refreshes the record. |

The planner never compares a rendered entry with the read-back entry directly, except to detect an
identical collision in adapters whose two hashes are comparable (Codex and Claude Code: yes).
Without `--take-over`, a drifted entry is never modified.

- A drifted entry that is still selected suggests `sync --take-over`. A drifted entry that is no
  longer selected suggests `unmanage` (or selecting it again) instead, because a take-over would
  plan its removal.
- `--take-over <server>` accepts the actual entry as owned (its sent and read-back hashes become
  the actual hash), then plans normally in the same run: an identical collision becomes
  `unchanged` without a write; any other content becomes an `update`. It applies to drift,
  collision and identical collision. A name that is neither selected nor recorded fails with
  `unknown-take-over` (exit 1). Adoption reuses this operation, bound to the entry it
  previewed (see [Apply](#apply)).
- `--relocate` rewrites the recorded paths of a harness to the newly resolved file, then plans
  normally, which usually re-adds the entries. It reports that the old file still contains the
  formerly managed tables and that d-mcp no longer owns them; it never edits the old file.

### Gaps

When a definition carries something a harness cannot express, or needs a manual step, rendering
reports a gap with `field`, `severity` and `reason`. A `note` gap is informational; an `important`
gap blocks the entry. Codex `note` gaps: `auth = none` (Codex falls back to OAuth discovery), an
`oauth_supported` flag on `bearer_env` (Codex ignores it), and `client_secret_env` (a manual step,
see [Codex mapping](#codex-mapping); it is not reported once the entry has an
`oauth.client_secret`). Keys of another harness's override block that a harness cannot enforce are
reported as `important` gaps by that harness's adapter; the Claude Code adapter does this for
`harness.codex.disabled_tools` and `enabled_tools`, so such a definition is not written to Claude
Code (Codex proceeds). For the Claude Code gaps, see [Claude Code mapping](#claude-code-mapping).
A reserved name is an `important` gap on `name` for that harness.

### Write state and recovery

Before a harness file is persisted, `sync` writes a `pending` record per entry with the expected
hashes and the previous owned hashes, under the store lock. The previous hashes are the ones the
entry was planned from: after `--take-over` or `--relocate` these are the taken-over or relocated
hashes, so a failed or interrupted write never loses a take-over. After verification the records
are promoted to `owned`. If the persist or the verification fails, an `update` or `remove` reverts
to the previous owned record, and an `add` of a new entry drops its record. A `pending` record left
by a crash is settled by the next sync before planning: it is promoted when the actual hash equals
the expected read-back hash, otherwise reverted to the previous hashes; an `add` of a new entry
without previous hashes is dropped, so a same-name entry with other content is reported as a
`collision`, never owned. A `pending` record for another file than the one just read (for example
after a crash and a changed `CODEX_HOME`) is left pending; it is reported as `location-changed` and
settled once its file is read again. Ownership is never lost.

d-mcp checks its own edit before anything is journaled or written: the spliced text must parse,
keep the `mcp_servers` root editable, keep every entry outside the plan with its hash, and contain
exactly the planned entries. Otherwise the harness fails with `harness-edit-failed` (exit 3) and
nothing is written.

The Codex adapter journals all operations at once before its single persist. The Claude Code
adapter writes through the `claude` CLI one operation at a time, journals each intent right before
its call, and reports a separate outcome per entry: `applied` (the record is promoted),
`failed` (the pending record is settled against the file: reverted, dropped or, when the write did
take effect, promoted), `rolled-back` or `lost` (see
[Claude Code mapping](#claude-code-mapping)), or a `drift` found right before a removal. A harness
with a failed entry has the status `failed` and carries the most severe entry error; the other
entries keep their own outcomes.

Every record carries a provenance marker (`written` in `state.toml`): it is set when d-mcp's own
write is promoted and cleared by `--take-over` (and by adoption). The Claude Code adapter rolls
back only entries with the marker, so foreign content never reaches a command line.

A record also keeps what outlives its hashes: the adoption provenance (`adopted_from`,
`replaced_fields`) and the vanish history (`vanish_count`, `last_vanish`). Take-overs, refreshes,
writes and settlements carry them over; only a dropped record loses them. `vanish_count` grows by
one whenever a `vanished` re-add is promoted (right after the write, or when a `pending` re-add is
settled) and is never reset; doctor warns from 2 on.

### Dry run and lock

`sync` and `unmanage` fail on an uninitialized store before locking. `sync` holds the store lock
for the whole run. `sync --dry-run` takes no lock, writes neither `state.toml` nor a harness file,
creates no file or directory, and treats a settled `pending` record as owned for planning only.
`unmanage --dry-run` only reports.

### Output rules

Sync output (text, JSON and errors) names only the harness, scope, file path, entry name,
operation, outcome, hold reason, gaps and positions, plus the hashes of owned entries: plain
(`hash`) only for an entry d-mcp wrote itself (`written`), keyed (`display_hash`) for a taken-over
one (see [Display hashes](#display-hashes)). It never contains field values, plain hashes of
foreign or unowned entries, child process output, or `toml_edit` or `serde_json` error text.
Shadowing notices name the entry and the project path only.

### Sync exit codes

| Exit | When |
|---:|---|
| 0 | Everything was applied, unchanged, held or re-added after `vanished`, or the harness is not present. `note` gaps do not change the code. |
| 1 | `sync-blocked`: at least one entry is blocked by drift, a collision, a location change, an unsupported entry or root style, or an `important` gap; `harness-cli-failed`: a harness CLI call of an entry failed; `harness-no-neutral-dir`: no neutral working directory for a harness CLI, so nothing was run for that harness. |
| 3 | `harness-file-changed`, `harness-config-invalid`, `harness-edit-failed`, `harness-verify-failed`; also `state-corrupt` and `schema-unsupported` for `state.toml`; `adopt-entry-changed` in the scoped sync of an adoption (an entry changed after adopt checked it). |
| 4 | An environment problem (`io-error`, `home-unset`, `harness-path-not-utf8`, `harness-cli-not-found`, `harness-cli-timeout`). |

Across harnesses the highest-priority code wins, in the order 3, 4, 1, 0. Global errors
(`store-locked`, `config-corrupt`, `state-corrupt`, an uninitialized store, `unknown-take-over`)
abort before any harness is touched; they carry no `data`.

## Unmanage

`unmanage <server> [--harness claude|codex]` removes the server's write-state records (without
`--harness`: for every harness with a record) and drops those harnesses from the server's
selection. The harness entries are left as they are, so the next sync reports no collision.

- A selection of `all` becomes an explicit list of the known harnesses (`claude`, `codex`) minus
  the unmanaged ones; the conversion is reported. An empty result removes the selection.
- The record is removed before the selection is written, so a crash in between can only cause a
  collision on the next sync, never the removal of the entry.
- A server without a record fails with `not-managed` (exit 1). An unknown `--harness` value is a
  usage error (exit 2).

## Discovery

d-mcp writes user scope only, but it reads every scope a harness loads, so it can show what is
already configured. Discovery is read-only: it opens no file for writing, runs no harness CLI and
creates nothing. Each scope is read on its own; a file that cannot be read is that scope's error
(by path and position only) and never hides the other scopes.

- **Current repository**: the nearest ancestor of the working directory (itself included) that
  holds `.git` (a directory, or a file for worktrees and submodules), otherwise the working
  directory. No `git` process is run.
- **Repository files** (`.mcp.json`, `.codex/config.toml`, Claude Code settings files) are read
  through the same size-capped, no-follow open as definition files: a symlink, a non-regular file
  or a file over 1 MiB is that scope's `harness-config-invalid` error.
- **Absent** means not found (or a parent that is not a directory). Any other error while looking
  for a file, such as a directory that cannot be searched, is not absence: for a harness or
  repository file it is that scope's `io-error`, for a settings file the approval is `unknown`,
  and a local-scope project path is not reported as orphaned.

| Harness | Scope | File | Read |
|---|---|---|---|
| Codex | `user` | `$CODEX_HOME/config.toml` | every entry, as for sync |
| Codex | `project` | `.codex/config.toml` of every directory from the repository root down to the working directory, root first | every entry, with the root's trust |
| Claude Code | `user` | the configuration file (see [Location and presence](#location-and-presence-1)) | every entry, as for sync |
| Claude Code | `local` | `projects.<path>.mcpServers` of the configuration file, one scope per project path | entry names and the project path only; never values (ADR 0005). A path that no longer exists is *orphaned* |
| Claude Code | `project` | `<repository>/.mcp.json` | every entry, with its approval |

**Codex project trust** (verified against Codex 0.165.0 with isolated `HOME` and `CODEX_HOME`,
through `codex mcp list --json`): Codex loads the `.codex/config.toml` layers only when the user
configuration trusts the repository root, `[projects."<root>"] trust_level = "trusted"`, keyed by
the root, not by a subdirectory. Without the key, or with `"untrusted"`, they are ignored. A
project entry replaces a user entry of the same name, and one unparsable layer makes Codex fail as
a whole. d-mcp reports the trust as `trusted`, `untrusted` (also for an absent key) or
`unknown-trust` (any other value, or an unreadable user configuration).

**Claude Code project approval** (verified against Claude Code 2.1.312 with isolated `HOME` and
`CLAUDE_CONFIG_DIR`, through `claude mcp list` in a temporary project with a `.mcp.json` stdio
server; no login, no network server; this covers the approval rules only, the version the
adapter writes against stays `2.1.310`, see [Doctor](#doctor)): an `.mcp.json` entry is loaded
only when approved. The keys
are `enabledMcpjsonServers`, `disabledMcpjsonServers` (arrays of names) and
`enableAllProjectMcpServers` (a boolean). They count from the user settings
(`$CLAUDE_CONFIG_DIR/settings.json`, else `$HOME/.claude/settings.json`) always, and from the
project's `.claude/settings.json`, `.claude/settings.local.json` and the legacy keys of the project
entry in the configuration file only once the workspace is trusted (`hasTrustDialogAccepted =
true` in `projects.<path>`); Claude Code moves the legacy keys into `.claude/settings.local.json`
on its next run. The project is Claude Code's working directory, so d-mcp uses its own. A
disabled name wins; an entry that is neither enabled nor disabled is pending. d-mcp reports each
entry's `approval` as `approved`, `disabled`, `pending` or `unknown` (a settings file that cannot
be read, an empty `CLAUDE_CONFIG_DIR`, or an unreadable configuration file), and the scope's trust
as `trusted`, `untrusted` or `unknown-trust`. Only these keys are deserialized; every other key of
a settings file, such as `env`, is skipped unread. Managed settings are not read; they can change
the outcome without d-mcp seeing it.

### Import

Every value-read entry is also imported into a neutral candidate definition, the inverse of the
rendering ([Codex mapping](#codex-mapping), [Claude Code mapping](#claude-code-mapping)). The
candidate is not validated yet; [adoption](#adoption) validates it.

- Every env and header literal becomes a variable reference: an env literal a forward of the same
  key name; a literal `Authorization: Bearer <token>` header `bearer_env`, any other literal header
  `header_env`, each with a proposed variable (`<SERVER>_TOKEN`, `<SERVER>_<HEADER>`, upper case,
  other characters turned into `_`). Each is listed as a *replaced* field (field, kind, variable),
  never with its value. The variable is shown as `<redacted-key>` when it does not look like a
  name (see [Redaction](#redaction)).
- Any name the definition would carry passes one name check, whether it comes from a literal, a
  forward or a reference: every env key (a replaced literal's, a Claude Code `${NAME}` forward's, a
  Codex `env_vars` name), every header name (literal or referenced, Codex `env_http_headers`
  included), and every variable a header or bearer token reads (Claude Code `${VAR}` in a header,
  Codex `env_http_headers` values and `bearer_token_env_var`). A name that does not look like a
  name, and an env key or variable that is not a valid variable name, is not imported: the field
  (shown as, for example, `env.<redacted-key>`, `headers.<redacted-key>`, `env_vars[0]`,
  `bearer_token_env_var`) is an `invalid-name` gap, because the definition would carry the name.
- A Claude Code value `${NAME}` or `${NAME:-}` under the env key `NAME` is a forward. Declared
  *normalizations*: `empty-default` (`${NAME}` is rendered as `${NAME:-}`), `default-dropped` (a
  header `${VAR:-}` is rendered as `${VAR}`), `scopes-sorted` (OAuth scopes are rendered sorted and
  deduplicated), `env-vars-sorted` (Codex `env_vars` are rendered as names, sorted and
  deduplicated).
- Codex's preserved keys (`oauth.client_secret`, `oauth.callback_url`, `oauth.callback_port`) are
  not literals to replace; they stay harness-local.
- *Import gaps* name a field and a reason, never a value: `local-scope`, `scope-unreadable`,
  `not-found`, `not-an-object`, `unsupported-style` (an inline or dotted Codex entry),
  `unknown-key`, `excluded-key` (`headersHelper`, `http_headers_helper`, approval modes,
  `experimental_environment`), `invalid-value`, `missing-transport`, `sse-transport`,
  `multiple-headers`, `header-with-oauth`, `renamed-forward` (`${OTHER}` under `NAME`),
  `forward-default` (`${VAR:-default}`), `embedded-variable` (variable syntax inside a longer
  value, also in `command`, `args` and `url`), `env-source` (a Codex `env_vars` entry whose
  source is not `local`) and `invalid-name` (see above). A key name that does not look like a name
  is shown as `<redacted-key>`.

## Inventory

`d-mcp inventory [--harness claude|codex]...` lists every discovered scope of every harness (or the
given ones) with its entries. It is read-only (no lock, no writes, no `.hash-key`, no harness CLI)
and exits 0, also when a scope cannot be read; global errors (a corrupt `config.toml` or
`state.toml`) fail as usual. On a store that is not initialized everything is `unmanaged`, with a
notice.

**Classes.** User-scope entries are classified by a read-only run of the sync planner against the
write state (held servers are left out, so their records classify by content):

| Class | Meaning |
|---|---|
| `managed` | Owned by d-mcp and unchanged since it was recorded. |
| `drifted` | Owned, but changed outside d-mcp. |
| `unmanaged` | Not owned by d-mcp; every project-scope entry. |
| `vanished` | Owned, but no longer in the harness file. |
| `pending` | A write of d-mcp was interrupted; the next sync settles it. |
| `location-changed` | Owned at another file (after `CODEX_HOME` or `CLAUDE_CONFIG_DIR` changed). |
| `local` | A Claude Code local-scope entry, by name only: never hashed, compared or adopted. |

**`shadowed_in`** lists, for a user-scope entry, the narrower scopes with the same name that win in
their project: a Claude Code local-scope entry (`local`, with the project path), an `.mcp.json`
entry that is not disabled and a Codex project layer that is not untrusted (`project`, with the
repository root).

**Same name in several scopes.** A name found in more than one value-read scope (user and project
scopes of both harnesses; local scope is names only) gets a verdict: `duplicate` when every import
has an equal `core_hash` and the same replaced literals, `conflict` when they differ, and
`undetermined` when one cannot be imported (within one harness, equal entry hashes still make a
`duplicate`). The `core_hash` and the literals are compared in memory and never output.

**Hashes** follow [Display hashes](#display-hashes).

## Adoption

`d-mcp adopt <harness>:<scope>:<name>...` takes existing harness entries over as d-mcp
definitions. Entries with the same name become one definition, written to
`local/servers/<name>.toml` of the built-in `local` source and selected for exactly the harnesses
whose entries are adopted. The scope is `user` or `project` (`local` is always refused). Several
names can be adopted in one run.

| Flag | Meaning |
|---|---|
| `--apply` | Act instead of previewing; needs `--expect` for every entry. |
| `--expect <harness>:<scope>:<name>=<display-hash>` | Binds an `--apply` to the preview (64 hex characters). |
| `--keep-literal <NAME>` | Keep an env literal as a value (see [Literals and variables](#literals-and-variables)). |
| `--var <field>=<NAME>` | Choose the variable of a replaced header literal. |
| `--to-user-scope` | Adopt a project-scope entry into the user scope (see [Scope widening](#scope-widening)). |
| `--from <harness>:<scope>` | Choose the origin whose content wins a conflict (see [Several origins](#several-origins)). |
| `--prefer-oauth`, `--oauth-supported` | Choose OAuth for a static token (see [OAuth](#oauth)). |
| `--accept-relogin`, `--client-secret-env <NAME>` | Accept the re-login of an OAuth-capable Claude Code entry and pass its client secret (see [OAuth](#oauth)). |
| `--allow-missing-env`, `--allow-env-mismatch` | Accept a failed variable check (see [Apply](#apply)). |

A malformed entry, `--expect`, `--var` or `--from` argument, and the combination of `--prefer-oauth`
with `--oauth-supported`, is a usage error (exit 2).

### Preview

Without `--apply` (or with `--apply --dry-run`) `adopt` is read-only: no lock, no write, no
`.hash-key`. It shows, per server:

- the definition d-mcp would write, as a redacted view (see [Redaction](#redaction));
- per origin entry: its `display_hash`, `write` (`none` when the rendering equals the entry, which
  is then taken over without a write; `update`; `add` for a project-scope origin, which adds a new
  user entry), a redacted `before` view of the entry, a redacted `after` view of the rendering, the
  native fields that differ (`changed`, names only) and the rendering's `note` gaps;
- the replaced literals (origin, field, kind, variable), the literals kept with `--keep-literal`,
  the declared normalizations, the variable gate and every refusal;
- notices: a project-scope origin that keeps its literals and shadows the adopted entry, an
  accepted variable override, a Claude Code re-login.

The text form ends with the `--expect` flags that bind an `--apply` to this preview.

### Literals and variables

- Every env and header literal becomes a variable reference (see [Import](#import)): an env literal
  a forward of the same key name; a literal `Authorization: Bearer <token>` header `bearer_env`, any
  other single literal header `header_env`.
- `--keep-literal <NAME>` keeps an env literal as `{ value = "..." }`, only for env entries and only
  when the value passes `secret_value_heuristic` (`keep-literal-secret` otherwise). Header values
  are always references.
- Env literals keep their key name. A header gets a proposed variable, `<SERVER>_TOKEN` for a bearer
  token and `<SERVER>_<HEADER>` otherwise. A proposal must be a valid variable name that looks like
  a name (it is not shown as `<redacted-key>`, see [Redaction](#redaction)) and must not be a
  [credential name](#credential-names), so it works for both adapters and can be shown; otherwise
  it is refused (`proposal-invalid`, `proposal-credential-name`) and `--var <field>=<NAME>` is
  needed. `--var`
  names the native field (`headers.Authorization` for Claude Code, `http_headers.<Header>` for
  Codex), overrides any proposal for every origin of the server, and is checked the same way
  (`variable-name`, `credential-name`).
- Codex's preserved keys (`oauth.client_secret`, `oauth.callback_url`, `oauth.callback_port`) are
  not literals; they stay harness-local.

### Several origins

- `duplicate`: the origins have the same `core_hash` and the same replaced literals. Their override
  tables (each origin's own harness table) are merged.
- `conflict`: they differ. `--from <harness>:<scope>` chooses the origin whose core the definition
  takes; the other harness's entry is rewritten to that core, as the preview shows. Override tables
  are merged as for duplicates. Without `--from` the server is refused (`conflict`). The literals of
  the other origins pass the variable gate too (see [Apply](#apply)); a literal of another origin
  that the chosen core does not carry is named in a notice, because it is unrecoverable after
  apply, and is not recorded in that origin's `replaced_fields`.
- At most one origin per harness (`duplicate-origin`). Every origin must import losslessly, also a
  non-chosen one.

### Scope widening

A project-scope origin needs `--to-user-scope` (`project-origin` otherwise). Its entry is left
where it is, because d-mcp writes user scope only; the adopted entry is added to the harness's
user scope (`write: add`), and the notice says that the project entry keeps its literals and
shadows the user entry in that project. Arguments that are relative paths (`./`, `../`) are
refused (`relative-path`), and so is a user entry of the same name in that harness
(`user-entry-exists`; adopt the user entry instead).

### OAuth

- `--prefer-oauth` replaces `bearer_env` or `header_env` with `auth = oauth`; the header and its
  variable are dropped. `--oauth-supported` keeps `bearer_env` and sets `oauth_supported = true`.
  Each is refused for other auth (`prefer-oauth-invalid`, `oauth-supported-invalid`); the two
  cannot be combined (usage error).
- Taking over an OAuth-capable Claude Code entry (a remote entry with `oauth` or without headers)
  with `write: update` removes and re-adds it, which clears Claude Code's stored tokens and client
  secret. It is refused unless `--accept-relogin` is given (`relogin-required`). When the entry has
  `oauth.clientId` and the definition keeps OAuth with a client id, `--client-secret-env <NAME>` is
  also required (`client-secret-env-required`); the variable must be set and non-empty, and d-mcp
  passes it to `claude mcp add-json --client-secret`. d-mcp never proposes a `client_secret_env`
  itself. `--client-secret-env` on a definition without an OAuth client id, or with a name that is
  not a valid variable name or does not look like a name, is `client-secret-env-invalid`.

### Refusals

Any refusal blocks the whole run with `adopt-refused` (exit 1); the document lists every refusal
with the `entry` (`<harness>:<scope>:<name>`, or `null` for the server or a flag), the `field` and
the `reason`, never a value. Reasons:

- every [import gap](#import) reason, among them `local-scope`, `not-found`, `unsupported-style`
  (`supported_style = false`), `unknown-key`, `excluded-key`, `multiple-headers`,
  `header-with-oauth`, `renamed-forward`, `forward-default`, `embedded-variable` and
  `sse-transport`;
- every [rule code](#rule-codes) of the candidate: it is serialized to TOML and must pass
  `parse_definition` and `validate_definition` and parse back unchanged. This covers
  `variable-syntax`, `url-scheme`, `url-query`, `url-userinfo`, `command-rule`, the secret rules on
  `args`, the URL and override strings such as Codex's `cwd` (`secret-name`, `secret-prefix`, ...),
  and the override rules;
- `name-rule`: the native name is not a neutral name (there is no renaming);
- `round-trip`: the import is not lossless. The adapter's entry hash of the rendered candidate must
  equal the entry hash of the native entry after the replaced literals and the declared
  normalizations (`empty-default`, `default-dropped`, `scopes-sorted`, `env-vars-sorted`);
- `render-gap`: the rendering for a target harness has an `important` gap (a credential name, a
  reserved name, Codex's `enabled_tools` or `disabled_tools` for Claude Code);
- `relative-path`, `project-origin`, `user-entry-exists`, `harness-not-present` (no user scope to
  add a project-scope origin to), `duplicate-origin`, `conflict`, `from-unknown`;
- `local-exists` (`local/servers/<name>.toml` exists), `local-unavailable`, `not-winning` (with the
  candidate in memory, another source wins the name or the resolution holds it),
  `selection-differs` (the server is selected for another harness set, `all` included),
  `already-managed` (d-mcp already has a record for the entry);
- `keep-literal-secret`, `keep-literal-unknown`, `var-unknown-field`, `proposal-invalid`,
  `proposal-credential-name`, `variable-name`, `credential-name`, `prefer-oauth-invalid`,
  `oauth-supported-invalid`, `relogin-required`, `client-secret-env-required`,
  `client-secret-env-invalid`;
- `variable-missing`, `variable-mismatch` (see the variable gate below);
- with `--apply`: `expect-missing` (an entry without `--expect`), `expect-unknown` (an `--expect`
  for no adopted entry).

### Apply

`--apply` requires `--expect <harness>:<scope>:<name>=<display-hash>` for every origin entry, with
the display hash of the preview. Under the store lock, in this order:

1. the harnesses are read again, and every check above runs against that fresh read, with the
   candidate in an in-memory snapshot of the sources. A display hash that differs from its
   `--expect` is `adopt-entry-changed` (exit 3); a harness CLI that a write needs and that is not on
   `PATH` is `harness-cli-not-found` (exit 4). Nothing is written then;
2. the definition is written to `local/servers/<name>.toml` (atomically, mode `0600`), then the
   selection to `config.toml`;
3. the entries are taken over by a scoped sync: limited to the adopted servers (other servers are
   not planned or reported, and their records stay unchanged), with each take-over bound to the
   entry hash just read (an entry that changed in between fails its harness with
   `adopt-entry-changed`, before anything is journaled).

A failure in step 3 leaves the definition and the selection in place and is reported per entry
with sync's outcomes (`outcome`, `error`). A Claude Code entry that was removed and could not be
added again (`lost`) is named with the variables to restore; the original literals were never
stored, so there is no rollback. The next sync re-adds it in reference form as `vanished`. A
take-over record of a failed write stays, so the next `sync` completes the adoption.

The adopted records carry `adopted_from` and `replaced_fields` (see [`state.toml`](#statetoml)).

**Variable gate.** Every variable that replaces a literal must be set, non-empty and equal to the
replaced literal in d-mcp's environment (the current shell), so the harness keeps working. This
holds for the literals of every origin, not only the `--from` one: with a conflict, a variable
that replaces different literals in two origins fails for one of them. The comparison happens in
memory; neither value is ever printed. A failed check is refused (`variable-missing`,
`variable-mismatch`, naming the origin entry and the field) unless `--allow-missing-env` or
`--allow-env-mismatch` accepts it; the output then says that the original literal of that origin
is unrecoverable after apply. The
client secret variable of `--client-secret-env` must be set and non-empty.

**Exit codes.** 0 complete; 1 `adopt-refused` or an entry failure of the scoped sync (such as
`harness-cli-failed`); 3 `adopt-entry-changed` (or another unsafe state); 4
`harness-cli-not-found` (or another environment problem). The precedence is 3, 4, 1, 0. `data` is
present on every exit after planning.

## Status

`d-mcp status [--auth] [--harness claude|codex]... [<server>...]` shows, per harness location
(user scope) and server, what d-mcp wants, what it recorded and what is there. It is read-only (no
lock, no writes, no `.hash-key`) and exits 0, also when a harness file cannot be read (that
harness's `status` is `failed`, with its error). Global errors (a corrupt `config.toml` or
`state.toml`) fail as usual; on a store that is not initialized nothing is selected or managed,
with a notice.

- **Servers**: every server selected for the harness and every recorded one, or only the named
  ones (an unknown name is a notice).
- **Desired**: `resolved` (with its `source`), `held` (with the hold `reason`, as in
  [Operations](#operations)) or `not-selected` (a record only). **Recorded**: `owned`, `pending`
  (an interrupted write) or `none`, with `adopted_from` and `vanish_count`. **Actual**: `present`,
  `absent`, or `unknown` when the harness could not be read or is not present.
- **Operation**: what the next sync would do for the entry, from the same read-only plan a dry-run
  sync makes (pending records settled in memory); see [Operations](#operations). `null` when the
  harness was not planned.
- **Hashes** follow [Display hashes](#display-hashes): `hash` only for a record d-mcp wrote itself.
- **Missing variables**: the variables a resolved definition reads (`Definition::forwarded_env`:
  stdio forwards, `bearer_env`, `header_env` and `client_secret_env`) that are unset or empty in
  the current shell; a set but empty variable counts as missing. Only names are shown. The output
  always notes that a harness launched from a GUI (a desktop app, a dock or a launcher) can see a
  different environment.
- **`--auth`** adds each present entry's login state through the adapter (see the Codex and Claude
  Code [login state](#login-state)): `logged-in`, `needs-login`, `not-applicable` or `unknown` (also
  without the harness CLI). It prints a warning, on stderr and as a notice, that Claude Code's
  `claude mcp get` connects to each OAuth server.

## Doctor

`d-mcp doctor [--auth]` runs health checks. It is read-only (no lock, no writes, no `.hash-key`).
Each check has a severity, `ok`, `warn` or `error`, a message and a hint. `doctor` exits 1 with
`doctor-failed` when any check is `error`, and carries its document as `data`; `warn` keeps exit 0.
A store that is not initialized or corrupt is reported as `error` checks instead of aborting. For
an uninitialized store the harness checks still run on the harness files, with nothing managed; for
a corrupt `config.toml` or `state.toml` every check that needs the store (`harness-config` to
`name-length`) is left out. When discovery itself fails (such as an unknown current directory),
a `discovery` `error` check names the error, the checks that build on the harness files
(`harness-config`, `drift`, `location-changed`, `pending`, `shadowing`, `name-length`, `login`) are
left out, and `recurring-vanish` and `static-token` still run on the store. A check that finds
nothing reports one `ok`.

| Check | `warn` or `error` when |
|---|---|
| `store` | `error`: the store is not initialized, or `config.toml` is corrupt or has another schema version. |
| `state` | `error`: `state.toml` is corrupt or has another schema version. |
| `hash-key` | `warn`: `.hash-key` is missing (no display hashes); `error`: it is corrupt. |
| `harness-version` | Per harness. `warn`: the harness CLI is not on `PATH` although the harness file exists or d-mcp records or selects entries for it; its `--version` is not one of the verified versions (`2.1.310 (Claude Code)`, `codex-cli 0.165.0`, compared exactly); or the probe failed or printed no recognizable version. `ok` when the CLI is missing and nothing of the harness exists. |
| `discovery` | Only when it fails. `error`: the harness files could not be discovered at all (such as an unknown current directory). |
| `harness-config` | A discovered harness file cannot be read: `error` for the user scope (sync cannot plan it), `warn` for other scopes. |
| `drift` | `warn`: a managed entry was changed outside d-mcp. |
| `location-changed` | `warn`: a record is for another harness file. |
| `pending` | `warn`: a write was interrupted; the next sync settles it. |
| `recurring-vanish` | `warn`: a managed entry was removed outside d-mcp and re-added (`vanish_count`) at least twice. |
| `shadowing` | `warn`: a managed entry is shadowed by a narrower scope (see `shadowed_in` in [Inventory](#inventory)), including an adopted entry whose project-scope origin still holds its literals. |
| `static-token` | `warn`: a selected definition uses `bearer_env` with `oauth_supported = true`. |
| `name-length` | `warn`: an unmanaged or local-scope entry name is longer than 32 characters, so d-mcp cannot adopt or manage it. |
| `credential-names` | Never; informational: the [credential-name list](#credential-names) can be incomplete. |
| `managed-policy` | Never; informational: managed-policy deny lists can block user servers, and d-mcp cannot see them. |
| `login` | Only with `--auth`, per managed entry: `warn` for `needs-login` (the hint names `<cli> mcp login <name>`) and `unknown`; one `warn` when the store cannot be read or the harness files could not be discovered, so nothing was queried. |

**Version probe.** `doctor` runs `<cli> --version` (`claude`, `codex`) through the same runner as
every harness CLI call: without a shell, with the normal timeout (10 s) and output cap, from a fresh
temporary directory (removed afterwards), with `DISABLE_AUTOUPDATER=1` and the resolved `HOME`,
`CODEX_HOME` and `CLAUDE_CONFIG_DIR`. Its output is parsed only: a version is shown only when it has
the shape of a verified version (the same text around a numeric version token); anything else is
reported as unrecognized and never printed. A failure names the exit status or `timeout` only.

## Uninstall

`d-mcp uninstall [--apply] [--purge [--follow-store-link]]` stops managing every harness entry.
Without `--apply` it is a read-only preview (no lock, no writes); `--apply --dry-run` equals the
preview. Under the store lock, `--apply` first settles `pending` records as sync does, then plans
every record as no longer desired and gives it one action:

| Action | When | Effect |
|---|---|---|
| `remove` | d-mcp created the entry (`written`, not adopted) and it is unchanged | removed through a scoped sync (Remove), limited to these entries |
| `release` | the entry was adopted (`adopted_from`) or taken over without a write | ownership dropped as by [`unmanage`](#unmanage); nothing is written, the entry keeps d-mcp's last rendering, and the output lists the variables it reads instead of the replaced literals |
| `forget` | the entry is gone (or the harness is not present) | the record is dropped |
| `drifted` | changed outside d-mcp | left, record kept unless `--purge`; exit 1 |
| `location-changed` | recorded for another file | left, record kept; exit 1; blocks `--purge` |
| `unsupported` | written in a style d-mcp does not edit | left, record kept unless `--purge`; exit 1 |

- The harnesses of removed, released and forgotten entries leave their selections (an `all`
  selection becomes an explicit list, as for `unmanage`); selections of entries that are left stay.
  Records are released before the selections are written, and a removal's selection is dropped
  before its scoped sync, so a crash in between never removes an entry d-mcp did not create.
- A Claude Code harness with a `remove` and no `claude` on `PATH` fails with
  `harness-cli-not-found` (exit 4): nothing is removed there, and `--purge` is blocked. A harness
  whose file cannot be read keeps its records and blocks `--purge`.
- **`--purge`** runs last and only when nothing blocks it (`purge.blocked`: `location-changed`,
  `harness-cli-not-found`, `harness-failed` for a failed harness or removal, `store-root-symlink`).
  It deletes only the known store entries (`config.toml`, `local`, `state.toml`, `.hash-key`,
  `lock.toml`, `approvals.toml`, then `.lock`) inside the resolved store root, then the root itself
  when it is empty. A symlink among them is unlinked, never followed, and nothing inside `local/` is
  followed either; registered sources live outside the store and are never touched. Drifted and
  unsupported entries stay in their harness, and their records go with `state.toml`.
- **A symlinked store root** is purged only with `--follow-store-link`: without it `--purge` is
  blocked (`store-root-symlink`, exit 1) with a warning naming the link target; with it a warning
  names the target, the known entries inside the target are deleted, and the link and the target
  directory are left.

**Exit codes.** 0 complete; 1 `uninstall-incomplete` (entries left), `purge-blocked`, or an entry
failure of the scoped sync; 3 an unsafe state (such as `harness-config-invalid`); 4
`harness-cli-not-found` or another environment problem. The precedence is 3, 4, 1, 0. `data` is
present on every exit after planning, also when the apply stops at an error (such as a store
write that fails): the error is the aggregate error, and every step sets the outcomes of its
entries as soon as it ends, so earlier steps keep theirs (`applied`, and the `sync` document of
each harness whose removals ran). Entries whose outcome is still `planned` were not changed,
entries `failed` were in the step that stopped and may be partly done, the purge did not run, and
a notice says so. Global errors (an uninitialized
store, the lock, a corrupt `config.toml` or `state.toml`) carry none.

## Redaction

Every place that renders the content of a harness entry goes through one redacted view, an
allow-list with default deny:

- **visible**: the entry name, the transport kind (`stdio`, `http`, `sse`, `unknown`), of a URL the
  scheme, host and port, the *names* of env entries, headers and referenced variables, the auth
  kind of the imported candidate, the override *keys*, whether an OAuth client id is present, and
  project paths;
- **redacted**: `command`, `args`, `cwd`, URL user info, path, query and fragment, env and header
  values, `description`, the client id, scopes, override values, and any other key. A redacted part
  that is present is named in `hidden` (`command`, `args`, `cwd`, `env.values`, `headers.values`,
  `oauth.client_id`, `oauth.scopes`, `override.values`, `other`); the redacted parts of a URL are
  named in `url.hidden`.

The view is a JSON object with these keys (`view` in `inventory`, `definition`, `before` and
`after` in `adopt`):

| Key | Content |
|---|---|
| `transport` | `stdio`, `http`, `sse` or `unknown`. |
| `url` | `null`, or `{ "scheme", "host", "port", "hidden" }`; `port` is a number or `null`, and `hidden` lists the redacted parts of the URL (`userinfo`, `path`, `query`, `fragment`, or `url` as a whole when it is not URL-shaped). |
| `env`, `headers` | Sorted names. |
| `variables` | Sorted names of the variables the entry reads. |
| `auth` | The auth kind of the imported candidate (`oauth`, `bearer_env`, `header_env`, `none`), `null` when the import failed. |
| `override_keys` | Sorted keys of the override tables (of the candidate, or the entry's native override-like keys). |
| `oauth_client_id` | Whether a client id is present, never its value. |
| `hidden` | The redacted parts of the entry that are present, sorted. |

The text form is one line, `transport=... url=... env=... ...`, with the URL parts listed as
`url.<part>` next to the hidden parts.

A name or host that does not look like one (characters outside `[A-Za-z0-9_.-]`, longer than 64
characters, or matching `secret_value_heuristic`) is shown as `<redacted-key>`, also a variable
name in `inventory`, `adopt` and `uninstall` output. The existing
invariants stay: harness CLI output is parsed in memory and never printed (stderr is drained and
dropped), sync reports carry no field values, and parser errors carry file, line and column only.
A canary suite (`tests/redaction.rs`) runs every command in text and JSON form on a fixture with
canaries in every position and fails if one reaches stdout, stderr or a store file. It includes
`adopt`, preview and apply: a replaced literal never reaches a file under `local/` or
`state.toml`.

## Codex mapping

The Codex adapter edits `$CODEX_HOME/config.toml` directly. Its declarations: apply mode
`direct-edit` (all operations of a sync in one atomic persist), discovery and write scope `user`,
comparable sent and read-back hashes, no reserved names, entries never vanish on their own,
verified against `codex-cli 0.165.0`.

### Location and presence

- `CODEX_HOME` (an empty value counts as unset; a relative value is resolved against the current
  directory), otherwise `$HOME/.codex`. The recorded path is `$CODEX_HOME/config.toml` as resolved
  *before* following symlinks, so retargeting a dotfile link is not a location change. A path
  that is not valid UTF-8 is never stored lossily: Codex fails with `harness-path-not-utf8`
  (exit 4).
- Codex counts as present when its `config.toml` exists or a `codex` executable is on `PATH`
  (looked up, never run). If neither exists, Codex is skipped with "harness not present" (exit 0)
  and nothing is created. If only the file is missing, it is created with mode `0600`, and its
  directory with `0700`.
- Profile files (`$CODEX_HOME/<profile>.config.toml`) are not handled in v1. The project scope
  (`.codex/config.toml`) is discovered read-only, never written (see [Discovery](#discovery)).

### Rendering

| Definition | Codex entry |
|---|---|
| stdio `command`, `args` | `command`, `args` (omitted when empty) |
| `env` forwards | `env_vars` (sorted names) |
| `env` literals | `[mcp_servers.<name>.env]` |
| http `url` | `url` |
| `bearer_env` `var` | `bearer_token_env_var` |
| `header_env` | `[mcp_servers.<name>.env_http_headers]` `<header> = "<var>"` |
| `oauth` `client_id` | `[mcp_servers.<name>.oauth]` `client_id` |
| `oauth` `scopes` | `scopes` (omitted when empty) |
| `oauth` | `auth` is omitted (Codex's default) |
| `harness.codex` allowed keys | copied as they are, except Codex's defaults (`enabled = true`, `required = false`, `disabled_tools = []`) |

- `client_secret_env`: Codex has no environment form for the OAuth client secret (verified against
  Codex 0.165.0). d-mcp treats `oauth.client_secret` as a harness-local preserved key: it never
  writes, reads, hashes or shows it, and keeps it on Update. While it is missing, sync reports a
  manual step (`note` gap): enter it once into the Codex table. `oauth.callback_url` (which Codex
  writes itself next to a client secret given to `codex mcp add`) and `oauth.callback_port` are
  harness-local preserved keys in the same way.
- An Update keeps a preserved key only while it fits the new rendering, because Codex refuses to
  load its whole configuration otherwise (verified against Codex 0.165.0): `oauth.client_secret`
  stays only next to a rendered `oauth.client_id`; `oauth.callback_url` and `oauth.callback_port`
  stay only on an http entry that authenticates through OAuth (`oauth`, with or without
  `client_id`, or `none`, which falls back to OAuth discovery). A switch to stdio, `bearer_env` or
  `header_env`, or dropping `client_id`, removes the keys that no longer fit. Each removed key is
  reported as a `note` gap on the entry, by name only: `oauth.client_secret removed: no longer
  used by this definition`. Its value is never shown or recorded.
- d-mcp never writes a value Codex treats as its default. Codex drops such keys when it re-saves
  its file (for example for `codex mcp add` of another server), and the entry hash ignores them,
  so such a re-save is no drift.
- `auth = none`: Codex then falls back to OAuth discovery; reported as a `note` gap.

### Write rules

1. A symlinked `config.toml` is resolved to its target. The target is opened without following
   further links and without blocking; it must be a regular UTF-8 file. Its bytes stay in memory.
2. Only the managed `mcp_servers.<name>` subtrees are touched. d-mcp splices new text into the
   original bytes, so every byte outside the managed subtrees stays unchanged, including comments,
   CRLF line endings and a missing trailing newline. An entry's region starts at its header and
   ends where the next foreign header's prefix begins, or, for the last table of the file, where
   the file's trailing blank lines and comments begin; comments above a managed header and after
   the last table stay in place on Update and Remove. Add appends standard tables, separated by a
   blank line, using the file's line ending. d-mcp writes no marker comments.
3. Right before the persist, d-mcp re-resolves the link and re-reads the target. If the link target
   or the bytes changed, it aborts that harness with `harness-file-changed` (exit 3); nothing is
   written.
4. The new content is persisted atomically into the target's directory; the link is kept, and the
   file keeps its mode (a new file gets `0600`). A failed directory fsync after the rename is a
   `warning:` line. The replace does not keep the owner, group, extended attributes or ACLs; a
   file owned by another user or group gets a `warning:` line (see
   [Locking and atomic writes](#locking-and-atomic-writes)).
5. The file is read back and the expected hashes are verified. If that fails and the file still
   holds what d-mcp wrote, the original bytes are restored atomically, with
   `harness-verify-failed` (exit 3); a failed restore is reported with the same code.

d-mcp keeps no backup copy of harness files anywhere, so no file it writes can contain foreign
secrets. An owned entry rewritten in an inline or dotted-key style is `unsupported`. An
`mcp_servers` root defined as an inline table or through dotted keys makes the whole harness
unsupported for writing (exit 1). Invalid TOML, non-UTF-8 content or a non-regular file is
`harness-config-invalid` (exit 3), reported by position only.

### Login state

The adapter can read the login state of its entries from `codex mcp list --json` (`auth_status`),
run without a shell, with a timeout and an output cap, with the resolved `CODEX_HOME`, and from a
neutral working directory: the store root when no `.codex/config.toml` lies in it or above it up to
its project root, otherwise a fresh temporary directory (removed afterwards), so no project
configuration of d-mcp's own working directory is listed:

| `auth_status` | State |
|---|---|
| `o_auth` | `logged-in` |
| `not_logged_in` | `needs-login` |
| `unsupported`, `bearer_token` | `not-applicable` |
| anything else, a missing binary, a failure, a timeout or oversized output | `unknown` |

`sync` never calls it; `status --auth` and `doctor --auth` use it. d-mcp never reads Codex token
storage, and child output is never printed: it is parsed in memory only, because a harness CLI can
echo foreign secrets. On a timeout only the direct child is killed; a grandchild (such as the native
binary behind the npm shim) may outlive it.

## Claude Code mapping

The Claude Code adapter writes through the `claude` CLI and only reads Claude Code's configuration
file; d-mcp never opens that file for writing. Its declarations: apply mode `harness-cli` (one CLI
call per operation), discovery and write scope `user`, comparable sent and read-back hashes, the
reserved names below, entries can vanish on their own (see [Limits](#limits)), verified against
Claude Code `2.1.310`.

### Location and presence

- The file is resolved like Claude Code 2.1.310 does it (verified with separate `HOME` and
  `CLAUDE_CONFIG_DIR` tempdirs): a legacy `.config.json` in the configuration directory
  (`$CLAUDE_CONFIG_DIR`, otherwise `$HOME/.claude`) wins when it exists; otherwise
  `$CLAUDE_CONFIG_DIR/.claude.json`, or `$HOME/.claude.json` without `CLAUDE_CONFIG_DIR`. A
  `CLAUDE_CONFIG_DIR` that is exported but empty is not the same as unset: Claude Code 2.1.310 then
  resolves its configuration home relative to its working directory, so it ignores
  `$HOME/.claude/.config.json`, would prefer a legacy `<cwd>/.config.json`, and writes a full
  backup of the configuration file to `<cwd>/backups/` on every change. d-mcp passes the empty
  value on to `claude`, but always runs it from a fresh, empty temporary directory, never the store
  root, and removes that directory with its `backups/` afterwards; there the legacy file cannot
  exist, so both use `$HOME/.claude.json`. With `CLAUDE_CONFIG_DIR` set or unset, Claude Code keeps
  its backups in `$CLAUDE_CONFIG_DIR/backups/` or `$HOME/.claude/backups/`, which d-mcp does not
  manage. A relative one is resolved against the current directory.
  The recorded path is the file as resolved before following symlinks; a path that is not valid
  UTF-8 fails with `harness-path-not-utf8` (exit 4). A changed `CLAUDE_CONFIG_DIR` is a
  `location-changed`, and the notice names the variable.
- Claude Code counts as present when its file exists or a `claude` executable is on `PATH`. If
  neither exists, it is `not-present` (exit 0) and nothing is created.
- File present, `claude` missing: planning runs as usual, dry runs included. With operations, the
  harness fails with `harness-cli-not-found` (exit 4) before anything is journaled; without
  operations, it is `synced` with the notice "the harness CLI is not on PATH; d-mcp writes this harness only
  through it". Codex proceeds in
  both cases.
- `claude` present, file missing: the first `add-json` creates it (`created: true`).
- Only user-scope entries (top-level `mcpServers`) are written. The project scope (`.mcp.json`) is
  discovered read-only (see [Discovery](#discovery)); local-scope entries
  (`projects.<path>.mcpServers`) are read by name only, to report shadowing and for the inventory.

### Reading

d-mcp deserializes only the user-scope `mcpServers.<name>` values (every key of each entry, for the
hash), the names of each project's local-scope `mcpServers`, and, for discovery, each project's
`hasTrustDialogAccepted` and legacy approval keys (names and booleans only); every other key, such
as account data, caches and history, is skipped unread, and no file bytes are kept. Invalid JSON (also a top
level that is not an object, or non-UTF-8 content) is retried once after a short delay, because a
running Claude Code session can be in the middle of rewriting the file, and then fails with
`harness-config-invalid` (exit 3), reported by line and column only. An `mcpServers` that is not a
JSON object makes the harness `unsupported` (exit 1); an owned entry that is not an object is
`unsupported`.

### Rendering

| Definition | Claude Code entry |
|---|---|
| stdio `command`, `args` | `command`, `args` (omitted when empty) |
| `env` forward | `env` `"<VAR>": "${<VAR>:-}"` |
| `env` literal | `env` `"<VAR>": "<value>"` |
| http `url` | `type = "http"`, `url` |
| `bearer_env` `var` | `headers` `"Authorization": "Bearer ${<var>}"` |
| `header_env` | `headers` `"<header>": "${<var>}"` |
| `oauth` `client_id` | `oauth.clientId` |
| `oauth` `scopes` | `oauth.scopes`, joined with spaces (omitted when empty) |
| `harness.claude.oauth_callback_port` | `oauth.callbackPort` (OAuth only; otherwise ignored with a `note` gap) |
| `harness.claude.timeout` | `timeout` (milliseconds) |
| `client_secret_env` | not rendered; see below |

- Forwards use `${VAR:-}`: Claude Code 2.1.310 passes an unset `${VAR}` to the server as the
  literal text `${VAR}` (with a "missing environment variables" warning for that server only),
  while `${VAR:-}` reads as empty. This differs from Codex, which does not set an unset variable at
  all. Claude Code passes its own environment to stdio servers as well. Headers use `${VAR}`, so
  Claude Code's own diagnostics name a missing variable. The `variable-syntax` rule keeps `$` and
  `${` out of literals.
- `client_secret_env`: on Add and Update, d-mcp calls `add-json --client-secret` and puts the
  variable's value into `MCP_CLIENT_SECRET` of the child environment only; it never appears in
  arguments, output, logs or files written by d-mcp. Setting or rotating the variable later does
  not cause a write. When it is unset, the entry is added without a secret and that write reports a
  manual step (`note` gap on `auth.client_secret_env`): set the variable, run `claude mcp remove -s
  user <name>`, and the next `d-mcp sync` re-adds the entry with the secret.
- `note` gaps: `auth = none` (Claude Code falls back to OAuth discovery), `oauth_supported` on
  `bearer_env` (Claude Code sends the bearer token), `harness.codex.cwd` (Claude Code has no
  working-directory setting), and an ignored `oauth_callback_port`.
- `important` gaps (the entry is not written to Claude Code; Codex proceeds):
  `harness.codex.enabled_tools` and `harness.codex.disabled_tools` (Claude Code has no per-server
  tool filter), a credential name (below) and a reserved name.

### Credential names

Claude Code reads some variable names as empty in a remote server's `url` and `headers`, and
ignores a `:-default` for them. A `bearer_env` or `header_env` variable of an `http` server with
such a name is an `important` gap on `auth.var`. stdio forwards are not checked. The list, kept in
`src/adapter/claude.rs`:

- the names documented at code.claude.com/docs/en/mcp and observed with 2.1.310 through a local
  HTTP endpoint: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_FOUNDRY_API_KEY`,
  `AWS_BEARER_TOKEN_BEDROCK`, `AWS_SECRET_ACCESS_KEY`, `CODEARTIFACT_AUTH_TOKEN`, `HTTPS_PROXY`,
  `NPM_TOKEN`;
- by decision, conservatively, every name starting with `ANTHROPIC_` (except
  `ANTHROPIC_BASE_URL`) or `AWS_`, and every name ending in `_PROXY` (case-insensitive). Claude
  Code 2.1.310 still expands some of them (such as `AWS_ACCESS_KEY_ID`); they are blocked anyway.

A name outside the list, such as `API_KEY` or `GITHUB_TOKEN`, expands as written. The list can be
incomplete.

### Reserved names

`workspace`, `claude-in-chrome`, `computer-use`, `Claude Preview` and `Claude Browser`
(code.claude.com/docs/en/mcp; `claude mcp add-json` rejects `workspace` in 2.1.310). The neutral
name rule already excludes the last two. Such an entry is blocked before any CLI call; a rejection
by the CLI stays a per-entry failure.

### Write rules

1. Add: `claude mcp add-json -s user [--client-secret] <name> <json>`. Remove: `claude mcp remove
   -s user <name>`. Update: remove, then add. `-s user` is always passed; the JSON is one argument;
   no shell is involved, and names follow the neutral name rule, so no argument starts with `-`.
2. Every call runs from a neutral working directory: the store root, or a fresh temporary
   directory when the store root or one of its parents has a `.mcp.json` or a project entry in the
   file. With a `CLAUDE_CONFIG_DIR` that is set but empty, it is always a fresh temporary
   directory, which must also hold no `.config.json`, and it is removed afterwards together with
   the `backups/` Claude Code writes into it (see the Claude Code location rules above).
   When the temporary directory is not neutral either, the harness fails with
   `harness-no-neutral-dir` (exit 1) before any call. Claude Code therefore never creates or
   touches a project entry for d-mcp's working directory. The child gets `PATH` from d-mcp's
   environment, `HOME`, `CLAUDE_CONFIG_DIR` set (empty when d-mcp's is set but empty, removed when
   unset), `DISABLE_AUTOUPDATER=1`, and no inherited `MCP_CLIENT_SECRET`. stdin is
   null; there is no TTY. Child output is never printed or stored.
3. Right before every removal, including the first step of an Update, the file is re-read and the
   entry's hash compared with the hash it was planned from. A different hash is reported as `drift`
   and the entry is skipped. The small window between this check and the `remove` is accepted;
   absence is verified afterwards.
4. After every add, the file is re-read and the entry must have the sent hash; otherwise the entry
   fails with `harness-verify-failed` (exit 3), nothing is restored, and its pending record is
   settled against the file: for an Add the entry is not owned (the record is dropped, so a
   same-name entry is a `collision`); for an Update the record reverts to its previous hashes and
   stays owned, so the next sync reports the entry as `drift` (or `vanished` when it is gone).
5. If the add of an Update fails, the old entry, kept in memory from the drift check, is re-added
   (`rolled-back`), but only when its record carries the provenance marker (a d-mcp rendering) and
   with `--client-secret` when the variable is set. Otherwise, or when the rollback fails too, the
   entry is `lost`: the notice names the removed entry's fields (names only), the record reverts to
   its previous hashes, and the next sync re-adds the entry as `vanished`. The record is never
   dropped. The rollback passes the client secret of the new rendering (its `client_secret_env`).
   A `rolled-back` entry never reports the manual step of an added entry (the new entry is not in
   place); when the restored entry has an `oauth` object and no secret was passed, it reports the
   `note` gap "client secret needed; the restored entry has none" on `auth.client_secret_env`
   instead.
6. A CLI write that exits unsuccessfully is `harness-cli-failed` for that entry (exit 1; the hint
   names program, subcommand and exit status only). A write that does not finish within 30 s is
   stopped (`harness-cli-timeout`, exit 4); the outcome is unknown, so d-mcp re-reads the file and
   settles the pending record from it and never rolls back blindly: an Update whose entry is gone
   after the timeout is `lost`.
7. `claude mcp remove` clears Claude Code's stored OAuth tokens and client secret for the entry
   (verified against 2.1.310). An Update of a remote entry that can hold OAuth state therefore
   reports a `note` gap "re-login needed".

### Login state

The adapter can read the login state of managed entries: one `claude mcp get <name>` per entry
that can hold OAuth state (a remote entry with an `oauth` object or without static headers), run
from a neutral working directory with the child environment above. Every other entry is
`not-applicable` without a call; there is no `list` fallback. The `Status:` line maps as follows
(recorded outputs of 2.1.310 are test fixtures in `tests/fixtures/claude/get/`):

| `Status:` | State |
|---|---|
| `✔ Connected` | `logged-in` |
| `! Needs authentication` | `needs-login` |
| anything else (`✘ Failed to connect`), an absent entry, a missing binary, a failure, a timeout or oversized output | `unknown` |

`sync` never calls it and never runs `--version`; `status --auth` and `doctor --auth` use it, and
warn that `claude mcp get` connects to the server to check its health. Only `doctor` runs
`--version` (see [Doctor](#doctor)). d-mcp never reads Claude Code's token storage.

### Limits

- A running Claude Code session rewrites the file from memory, so it can undo a d-mcp add (the
  entry is `vanished` on the next sync and re-added) or bring back an entry d-mcp removed (it is
  then unowned and a `collision` if still selected).
- A same-name local-scope entry shadows the managed user entry in that project. `sync` reports it
  as a notice per project path. `add-json -s user` accepts a name that exists in local scope
  (verified against 2.1.310).
- Managed-policy deny lists can block user servers; d-mcp does not see them. `doctor` states this
  as its informational `managed-policy` check.

## `state.toml`

```toml
schema_version = 1

[[records]]
harness = "codex"
scope = "user"
name = "github"
file = "/home/me/.codex/config.toml"
status = "owned"
sent = "<64 hex>"
readback = "<64 hex>"
definition_hash = "<64 hex>"
written = true
adopted_from = { harness = "claude", scope = "project", root = "/home/me/work/app" }
replaced_fields = [{ field = "env.GITHUB_TOKEN", var = "GITHUB_TOKEN" }]
vanish_count = 2
last_vanish = 1793145600
time = 1793145600
version = "0.1.0"
```

- A record is keyed by `harness`, `scope` and `name`. `file` is the harness file as resolved before
  following symlinks, always valid UTF-8. `sent` is the hash of the entry as rendered, `readback`
  the semantic hash of the entry as read back, `definition_hash` the source definition hash (WP5
  binds approvals to it). `time` is in Unix epoch seconds; `version` is the d-mcp version that
  wrote the record.
- `status` is `owned` or `pending`. A `pending` record has `op` (`add`, `update` or `remove`), the
  expected `sent`, `readback` and `definition_hash` (not for `remove`), and the previous owned
  `prev_sent`, `prev_readback` and `prev_definition_hash` (not for an `add` of a new entry).
- `written = true` (and `prev_written = true` for the previous hashes) is the provenance marker:
  d-mcp itself wrote the entry with these hashes. It is absent after `--take-over`; `false` is
  never written. Records written before the marker existed have none.
- `adopted_from` (only on records created by `d-mcp adopt`): the origin's `harness`, `scope`
  (`user` or `project`) and, for `project`, the absolute repository `root`. `replaced_fields` (only
  with `adopted_from`, non-empty, sorted): the native fields whose literals adoption replaced, each
  with the variable the entry reads instead; never a value.
- `vanish_count` (at least 1) and `last_vanish` (Unix epoch seconds), always together: how often a
  `vanished` re-add of the entry was promoted, and when last. Absent until the first one.
- There is no downgrade path before 1.0: `schema_version` stays 1 when keys are added, and an older
  d-mcp refuses a file with keys it does not know as `state-corrupt` (exit 3); for example, d-mcp
  builds before the Claude Code adapter reject `written` and `prev_written`. Downgrading means
  removing `state.toml` by hand, which forgets every record.
- d-mcp writes the file atomically and in canonical form, records sorted by harness, scope and
  name. Any deviation makes it corrupt: `state-corrupt` (exit 3), reported by line, column and
  field path; another schema version fails with `schema-unsupported` (exit 3). d-mcp never
  rewrites a corrupt file.

## Exit codes

| Code | Meaning | Cases |
|---|---|---|
| 0 | Success | Including resolution problems, holds, `note` gaps and warnings |
| 1 | Expected failure | Store not initialized; invalid arguments; rejected `source add`; `init` into a non-empty directory that is not a store; a blocked sync entry; a failed harness CLI call; `unmanage` of an unmanaged server; a refused adoption (`adopt-refused`); an uninstall that left entries (`uninstall-incomplete`) or could not purge (`purge-blocked`); a doctor check at `error` (`doctor-failed`) |
| 2 | Usage error | clap usage errors (unknown flags, missing arguments, non-integer priority, an unknown `--harness` value, a malformed `adopt` argument, `--follow-store-link` without `--purge`); clap prints them as text on stderr, also with `--json` |
| 3 | Unsafe state | Corrupt or hand-broken `config.toml` or `state.toml`; unsupported schema version; `store-locked`; a harness file that changed, is invalid, could not be edited or did not verify; an entry that changed after the adoption preview (`adopt-entry-changed`) |
| 4 | Environment problem | A missing `HOME`; an unknown current directory; a harness path that is not UTF-8; a harness CLI that is missing or timed out; a failed file-system operation |

## Error codes

Every error has a stable code, a hint and an exit code. Hints name paths, ids, positions and rule
codes, never values.

<!-- error-codes:start -->
| Code | Exit | Meaning |
|---|---|---|
| `store-not-initialized` | 1 | The store directory has no `config.toml`; run `d-mcp init`. |
| `store-path-invalid` | 1 | The store path, or `local/` inside it, exists but is not a directory. |
| `foreign-directory` | 1 | `init` into a non-empty directory that is not a store. |
| `invalid-name` | 1 | A source id or server name breaks the name rule. |
| `source-id-reserved` | 1 | `local` is the built-in source; it cannot be added or removed. |
| `source-id-taken` | 1 | A source with this id exists. |
| `priority-invalid` | 1 | The priority is 0. |
| `priority-taken` | 1 | Another source uses this priority. |
| `source-path-not-found` | 1 | The source path does not exist. |
| `source-path-not-directory` | 1 | The source path is not a directory. |
| `source-path-not-utf8` | 1 | The canonical source path is not valid UTF-8. |
| `source-path-registered` | 1 | The path is registered under another id. |
| `source-path-in-store` | 1 | The path lies inside the store. |
| `source-not-found` | 1 | No source with this id exists. |
| `not-selected` | 1 | `unselect` of a server that is not selected. |
| `config-corrupt` | 3 | `config.toml` breaks a rule; it is never rewritten. |
| `schema-unsupported` | 3 | A persisted file has a `schema_version` other than 1. |
| `store-locked` | 3 | Another process holds the store lock after all retries. |
| `harness-file-changed` | 3 | A harness file or its link target changed between read and persist; nothing was written. |
| `harness-config-invalid` | 3 | A harness file is not valid TOML, not UTF-8 or not a regular file; reported by position only. |
| `harness-verify-failed` | 3 | A harness file did not read back as written; the original was restored (or the restore failed, as the hint says). For a harness written through its CLI (Claude Code) nothing is restored: after an Add the entry is not owned; after an Update the record keeps its previous hashes and the next sync reports `drift`. |
| `harness-edit-failed` | 3 | d-mcp could not produce a valid edit of a harness file (the spliced text did not parse, or it changed something outside the plan); nothing was written. |
| `harness-path-not-utf8` | 4 | A harness configuration path (such as `$CODEX_HOME/config.toml`) is not valid UTF-8, so the write state cannot record it; that harness is skipped. |
| `harness-cli-not-found` | 4 | A harness that d-mcp writes through its CLI (Claude Code: `claude`) has operations, but the CLI is not on `PATH`; nothing was journaled or written for that harness. |
| `harness-cli-failed` | 1 | A harness CLI call exited unsuccessfully (per entry); the hint names only the program, the subcommand and the exit status, never the output. |
| `harness-no-neutral-dir` | 1 | No neutral working directory exists for a harness CLI (Claude Code: `claude`): the store root and a fresh temporary directory both have a `.mcp.json` or a `projects` entry of the harness configuration at or above them; nothing was run for that harness. Remove the marker or point `TMPDIR` elsewhere. |
| `harness-cli-timeout` | 4 | A harness CLI call did not finish in time (Claude Code writes: 30 s); d-mcp re-read the file and settled the write. |
| `state-corrupt` | 3 | `state.toml` breaks a rule; it is never rewritten. |
| `sync-blocked` | 1 | At least one sync entry is blocked (drift, collision, location change, unsupported style, important gap). |
| `not-managed` | 1 | `unmanage` of a server without a write-state record. |
| `unknown-take-over` | 1 | `sync --take-over` names a server that is neither selected nor recorded. |
| `adopt-refused` | 1 | `adopt` refused at least one entry (see [Adoption refusals](#refusals)); nothing was written. The `adopt` document names every refusal. |
| `adopt-entry-changed` | 3 | An entry to adopt changed since the preview, or its `--expect` display hash does not match; nothing was written for it. |
| `doctor-failed` | 1 | At least one `doctor` check is at `error`; the `doctor` document lists every check. |
| `uninstall-incomplete` | 1 | `uninstall` left entries in the harnesses (drifted, location changed or unsupported); their records are kept unless `--purge`. |
| `purge-blocked` | 1 | `uninstall --purge` was blocked (a location-changed record, a missing harness CLI, a failed harness or removal, or a symlinked store root without `--follow-store-link`); nothing was purged. |
| `home-unset` | 4 | `HOME` is unset or empty but needed. |
| `cwd-unavailable` | 4 | The current directory cannot be determined. |
| `hash-key-corrupt` | 3 | `<store>/.hash-key` is not a regular file or does not hold a key; it is never replaced. Remove it and run `d-mcp init`. Read-only commands show no display hashes instead. |
| `hash-key-unavailable` | 4 | The system random source is unavailable, so no display-hash key can be created. |
| `io-error` | 4 | A file-system operation failed; only the error kind is reported. |
<!-- error-codes:end -->

## Rule codes

Rule codes appear in `rule` of problems and in `config-corrupt` hints.

| Code | Where | Meaning |
|---|---|---|
| `toml-syntax` | file, config | Not valid TOML (including duplicate keys). |
| `not-utf8` | file, config | Not UTF-8. |
| `file-too-large` | file | Larger than 64 KiB. |
| `not-regular-file` | file | A symlink, FIFO or other special file. |
| `unreadable` | file | The file cannot be read. |
| `invalid-stem` | file | The file stem breaks the name rule. |
| `unknown-key` | file, config | A key that the table does not allow. |
| `missing-field` | file, config | A required field is missing. |
| `wrong-type` | file, config | A field has the wrong TOML type. |
| `unsupported-type` | file | Unknown `transport.type` or `auth.type`. |
| `invalid-env-entry` | file | An env entry is not exactly `{ forward = true }` or `{ value = "..." }`. |
| `transport-mismatch` | file | `env` on http, or `auth` on stdio. |
| `non-finite-float` | file | NaN or infinity in an override table. |
| `name-rule` | file, config | A name or id breaks the name rule. |
| `name-mismatch` | file | `name` differs from the file stem. |
| `variable-name` | file | A variable name breaks `^[A-Za-z_][A-Za-z0-9_]*$`. |
| `header-name` | file | A header name is not an RFC 9110 token. |
| `client-secret-without-client-id` | file | `client_secret_env` without `client_id`. |
| `command-rule` | file | `command` is empty, relative or starts with `~`. |
| `url-invalid` | file | `url` is not URL-shaped (including whitespace, control characters, an unclosed `[` or an invalid port) or has no host. |
| `url-scheme` | file | Not `https`, or `http` to a non-loopback host. |
| `url-query` | file | `url` carries a query. |
| `url-userinfo` | file | A URL-shaped value or a Go DSN carries user info. |
| `variable-syntax` | file | Variable syntax in a checked position. |
| `secret-name` | file | The name heuristic matched. |
| `secret-prefix` | file | A known secret prefix. |
| `secret-jwt` | file | A JWT. |
| `secret-hex` | file | A hex run of at least 32 characters outside digest contexts. |
| `secret-entropy` | file | A high-entropy token. |
| `override-key` | file | A `harness.<id>` key that the adapter does not allow, or deliberately excludes (`validate`). |
| `override-type` | file | A `harness.<id>` value of the wrong type or outside its range (`validate`). |
| `override-transport` | file | A `harness.<id>` key that is not valid for the definition's transport, such as `harness.codex.cwd` on http (`validate`). |
| `invalid-value` | config | A value outside its allowed set or range. |
| `duplicate-entry` | config | A duplicate id, path, priority, selection or harness. |
| `hidden-file` | notice | A hidden entry in `servers/`. |
| `non-toml-file` | notice | A non-`.toml` entry in `servers/`. |
| `subdirectory` | notice | A subdirectory in `servers/`. |

## JSON documents

### Envelope

With `--json`, stdout carries exactly one JSON document:

```json
{
  "schema_version": 1,
  "ok": true,
  "data": {},
  "problems": [],
  "error": null
}
```

- `schema_version`: always `1`.
- `ok`: `true` on exit 0, `false` otherwise.
- `data`: the command's document on success, `null` on failure. `sync`, `adopt`, `uninstall` and
  `doctor` are the exception: on a non-zero exit after planning (1, 3, or 4 for a harness-level
  environment problem) they still carry their document, together with `ok: false` and the
  aggregate `error`. Global errors carry `data: null`.
- There are no per-document versions: documents only grow by added keys under `schema_version` 1.
- `problems`: an array of problems; only `resolve` and `sync` report any.
- `error`: `null` on success, otherwise `{ "code": "<error code>", "hint": "<text>" }`.

Warnings go to stderr as `warning: ...` lines, never to stdout. Usage errors (exit 2) are printed
by clap as text on stderr, with nothing on stdout.

In text output (without `--json`), control characters in names and messages, such as an ESC in a
file name, are escaped `escape_debug`-style (`\u{1b}`), so they cannot drive the terminal; the
layout's own line breaks and tabs are kept. JSON output carries the real value, escaped by JSON.

### Problems

Every problem has all of these keys; keys that do not apply are `null`.

```json
{
  "code": "invalid-definition",
  "server": "github",
  "source": "team",
  "file": "servers/github.toml",
  "rule": "secret-prefix",
  "field": "transport.args[1]",
  "line": 6,
  "column": 22,
  "message": "servers/github.toml in source `team` breaks rule `secret-prefix`"
}
```

The closed list of problem codes:

| Code | Keys set | Meaning |
|---|---|---|
| `invalid-definition` | `source`, `file`, `rule`; `server` when the stem is a valid name; `field`, `line`, `column` for problems inside the file | A file in an available source is invalid. |
| `invalid-winner` | `server`, `source`, `file`, `rule`, and the position when known | A selected server is held because its highest-priority file is invalid. |
| `not-offered` | `server` | A selected server is offered by no source. |
| `source-unavailable` | `source`; `server` for a held server | A source is unavailable (one problem per source), and a selected server is held because of it (one problem per server). |
| `shadowed` | `server`, `source`, `file` | A definition is shadowed by a higher-priority one. |
| `notice` | `source`, `file`, `rule` | An entry of `servers/` was ignored. |

Problems are sorted by code, server, source, file, line and column, in the order of the table
above. Messages are built from codes, names and paths only.

### `init`

```json
{ "store": "/home/me/.d-mcp", "created": ["/home/me/.d-mcp", "/home/me/.d-mcp/.lock", "/home/me/.d-mcp/config.toml", "/home/me/.d-mcp/local", "/home/me/.d-mcp/local/servers", "/home/me/.d-mcp/.hash-key"], "dry_run": false }
```

`created` lists absolute paths in creation order: root, `.lock`, `config.toml`, `local`,
`local/servers`, `.hash-key`; empty when the store is complete. A dry run lists what would be created, in the
same order.

### Source object

`source add`, `source list` and `source remove` share this object:

```json
{ "id": "team", "kind": "directory", "path": "/home/me/team-mcp", "priority": 10, "builtin": false, "available": true }
```

### `source add`

```json
{ "source": { "id": "team", "kind": "directory", "path": "/home/me/team-mcp", "priority": 10, "builtin": false, "available": true }, "dry_run": false }
```

### `source list`

```json
{ "sources": [
  { "id": "team", "kind": "directory", "path": "/home/me/team-mcp", "priority": 10, "builtin": false, "available": true },
  { "id": "local", "kind": "directory", "path": "/home/me/.d-mcp/local", "priority": 0, "builtin": true, "available": true }
] }
```

Sources are sorted by descending priority, so `local` comes last.

### `source remove`

```json
{ "source": { "id": "team", "kind": "directory", "path": "/home/me/team-mcp", "priority": 10, "builtin": false, "available": true }, "no_longer_offered": ["github"], "dry_run": false }
```

### `select`

```json
{ "server": "github", "harnesses": ["all"], "offered": true, "replaced": false, "dry_run": false }
```

`harnesses` is `["all"]` or the sorted list of `claude` and `codex`. `offered` tells whether any
source offers the name; `replaced` whether an existing selection was replaced.

### `unselect`

```json
{ "server": "github", "dry_run": false }
```

### `resolve`

```json
{ "store": "/home/me/.d-mcp", "servers": [
  { "name": "github", "status": "resolved", "reason": null, "source": "team", "file": "servers/github.toml",
    "harnesses": ["all"], "transport": "stdio", "auth": null, "forwarded_env": ["GITHUB_TOKEN"],
    "definition_hash": "<64 hex>", "core_hash": "<64 hex>" },
  { "name": "linear", "status": "hold", "reason": "invalid-winner", "source": "local", "file": "servers/linear.toml",
    "harnesses": ["claude"], "transport": null, "auth": null, "forwarded_env": null,
    "definition_hash": null, "core_hash": null }
] }
```

Servers are sorted by name. `resolve` shows only name, status, reason, origin source
and file, harnesses, transport kind, auth kind (`null` for stdio), forwarded variable names and
both hashes. It never shows literal values, `args` or override contents. `forwarded_env` lists the
env forwards of a stdio server, or the auth variables (`var`, `client_secret_env`) of an HTTP
server. For `hold`, `source` names the source that causes the hold; the definition fields are
`null` unless the status is `resolved`.

### `sync`

```json
{ "store": "/home/me/.d-mcp", "dry_run": false, "harnesses": [
  { "harness": "codex", "scope": "user", "file": "/home/me/.codex/config.toml", "status": "synced",
    "created": false, "error": null, "notices": ["harness not present"],
    "entries": [
      { "name": "linear", "operation": "add", "outcome": "applied", "reason": null, "notice": null,
        "hash": "<64 hex>", "display_hash": null, "error": null,
        "gaps": [ { "field": "auth.client_secret_env", "severity": "note", "reason": "manual step: ..." } ] }
    ] }
] }
```

- `status`: `synced`, `not-present` (nothing created), `location-changed`, `unsupported` (an inline
  or dotted `mcp_servers` root, or a Claude Code `mcpServers` that is not an object) or `failed`
  (with `error`; for a harness-CLI adapter also when an entry failed).
- `error` of a harness: `null`, or `{ "code", "hint" }`: its own error, or the most severe entry
  error.
- `operation`: see [Operations](#operations); a `drift` found by the Claude Code adapter right
  before a removal replaces the planned operation. `outcome`: `applied` (written), `planned` (a
  dry run would write or record it), `failed` (the harness write failed), `rolled-back` (an Update
  failed and the previous entry was restored), `lost` (an Update removed the entry and could not
  add it again; the next sync re-adds it), `recorded` (only the write state changed: take-over,
  refresh, relocate, orphaned), `unchanged`, `blocked` (counts toward exit 1) or `kept` (held).
- `error` of an entry: `null`, or its own `{ "code", "hint" }` (Claude Code: `harness-cli-failed`,
  `harness-cli-timeout`, `harness-verify-failed`, ...).
- `reason`: the hold reason for `held`: `invalid-winner`, `source-unavailable` or `not-offered`.
- `notice`: a hint such as how to resolve drift, or `null`.
- `hash`: the read-back hash of an owned entry that d-mcp wrote itself (`written`), `null`
  otherwise. `display_hash`: the keyed display hash of an owned entry d-mcp did not write (a
  take-over), `null` otherwise or without a `.hash-key` (see [Display hashes](#display-hashes)).
  Never the plain hash of a foreign entry.
- `gaps`: `field`, `severity` (`note` or `important`) and `reason`.

Harnesses come in adapter order (`codex`, then `claude`), entries sorted by name; problems are the
resolver's (see [Problems](#problems)). In text
mode each harness prints one line, then one line per entry (`operation`, name, outcome), then its
notices and gaps; a dry run ends with "Dry run: nothing was written."

### `adopt`

The `data` of a preview, also on exit 1 (refusals) and 3 or 4; after `--apply` it adds `written`
and the `sync` document of the scoped sync.

```json
{ "store": "/home/me/.d-mcp", "repository": "/home/me/work/app", "apply": false,
  "display_hashes": true,
  "servers": [
    { "name": "tasks", "harnesses": ["claude", "codex"], "verdict": "duplicate", "from": null,
      "file": "/home/me/.d-mcp/local/servers/tasks.toml",
      "definition": { "transport": "stdio", "url": null, "env": ["TASKS_TOKEN"], "headers": [],
                      "variables": ["TASKS_TOKEN"], "auth": null, "override_keys": [],
                      "oauth_client_id": false, "hidden": ["args", "command"] },
      "entries": [
        { "entry": "claude:user:tasks", "harness": "claude", "scope": "user", "path": null,
          "file": "/home/me/.claude.json", "display_hash": "<64 hex>", "write": "update",
          "before": { "transport": "stdio", "url": null, "env": ["TASKS_TOKEN"], "headers": [],
                      "variables": [], "auth": null, "override_keys": [], "oauth_client_id": false,
                      "hidden": ["args", "command", "env.values"] },
          "after": { "transport": "stdio", "url": null, "env": ["TASKS_TOKEN"], "headers": [],
                     "variables": ["TASKS_TOKEN"], "auth": null, "override_keys": [],
                     "oauth_client_id": false, "hidden": ["args", "command"] },
          "changed": ["env.TASKS_TOKEN"], "gaps": [],
          "replaced_fields": [ { "field": "env.TASKS_TOKEN", "var": "TASKS_TOKEN" } ],
          "outcome": null, "error": null }
      ],
      "replaced": [ { "origin": "claude:user:tasks", "field": "env.TASKS_TOKEN", "kind": "env", "var": "TASKS_TOKEN" } ],
      "kept_literals": [],
      "normalizations": [ { "origin": "claude:user:tasks", "field": "env.TASKS_HOME", "kind": "empty-default" } ],
      "variables": [ { "name": "TASKS_TOKEN", "field": "env.TASKS_TOKEN", "origin": "claude:user:tasks",
                       "status": "ok", "accepted": false } ],
      "refusals": [], "notices": [] }
  ],
  "refusals": [], "notices": [], "written": null, "sync": null }
```

- `verdict`: `single`, `duplicate`, `conflict`, or `null` when an origin cannot be imported.
  `from`: the `--from` origin, or `null`. `harnesses`: the selection the apply writes.
  `definition`: the redacted view of the definition, `null` when it cannot be built.
- `entries[]`: one per origin. `scope`: `user`, `local` or `project`; `path`: the repository root
  of a project-scope origin. `write`: `none`, `update`, `add` or `null` (not planned).
  `before`, `after`: redacted views (see [Redaction](#redaction)). `changed`: native field names.
  `gaps`: the rendering's `note` gaps. `replaced_fields`: what the record will carry. `outcome`,
  `error`: the scoped sync's outcome (see [`sync`](#sync-1)) and error after an apply, else
  `null`; a harness that failed as a whole gives its status (such as `failed`) and error.
- `replaced[].kind`: `env`, `bearer-header` or `header`. `normalizations[].kind`: `empty-default`,
  `default-dropped`, `scopes-sorted` or `env-vars-sorted`. `variables[].status`: `ok`, `missing`
  or `differs`; `origin` is the entry whose literal was compared (`null` for the client secret);
  `accepted` tells whether an `--allow-*` flag accepted it. Variable names (`replaced[].var`,
  `replaced_fields[].var`, `variables[].name`) are shown as `<redacted-key>` when they do not look
  like a name.
- `kept_literals`: the names of the env literals kept with `--keep-literal`.
- `refusals[]` (per server, and at the top for flags): `entry` (or `null`), `field` (or `""`) and
  `reason` (see [Refusals](#refusals)).
- `written`: after an apply, `{ "definitions": [paths], "selections": [names] }`, also when the
  scoped sync failed; `null` otherwise. `sync`: the scoped sync's `sync` document, or `null`.

In text mode, each server prints one line (name, file, verdict, `from`, selection), the definition
view, per origin a line with `write`, `display_hash` and the outcome, its `before`, `after`,
`changed` and gaps, then the replaced literals, normalizations, variables, refusals and notices.
A preview ends with the `--expect` flags for `--apply`.

### `unmanage`

```json
{ "server": "github", "harnesses": ["codex"], "selection": ["claude"], "converted_from_all": true, "selection_removed": false, "dry_run": false }
```

`harnesses` lists the harnesses whose records were removed; `selection` is the server's harness list
afterwards (`null` when the server is not selected any more); `converted_from_all` tells whether an
`all` selection became an explicit list.

### `inventory`

```json
{ "store": "/home/me/.d-mcp", "initialized": true, "repository": "/home/me/work/app",
  "display_hashes": true, "notices": [],
  "harnesses": [
    { "harness": "codex", "notices": [], "scopes": [
      { "scope": "user", "path": null, "file": "/home/me/.codex/config.toml", "present": true,
        "trust": null, "orphaned": false, "error": null, "entries": [
        { "name": "github", "class": "unmanaged", "hash": null, "display_hash": "<64 hex>",
          "shadowed_in": [ { "scope": "project", "path": "/home/me/work/app" } ], "approval": null,
          "view": { "transport": "stdio", "url": null, "env": ["GITHUB_TOKEN"], "headers": [],
                    "variables": ["GITHUB_TOKEN"], "auth": null, "override_keys": [],
                    "oauth_client_id": false, "hidden": ["args", "command", "env.values"] },
          "import": { "ok": true, "gaps": [],
                      "replaced": [ { "field": "env.GITHUB_TOKEN", "kind": "env", "var": "GITHUB_TOKEN" } ],
                      "normalizations": [] } }
      ] },
      { "scope": "project", "path": "/home/me/work/app", "file": "/home/me/work/app/.codex/config.toml",
        "present": true, "trust": "trusted", "orphaned": false, "error": null, "entries": [] }
    ] }
  ],
  "same_name": [
    { "name": "github", "verdict": "conflict", "places": [
      { "harness": "codex", "scope": "user", "file": "/home/me/.codex/config.toml" },
      { "harness": "claude", "scope": "user", "file": "/home/me/.claude.json" } ] }
  ] }
```

- Harnesses come in adapter order (`codex`, then `claude`). Scopes: `user` first, then Claude
  Code's `local` scopes by project path, then the `project` scopes (Codex layers root first);
  entries are sorted by name.
- `scope`: `user`, `local` or `project`; `path`: the project path (`local`) or repository root
  (`project`), `null` for `user`. `present`: the file exists (for `user` also when only the
  harness binary is on `PATH`). `trust`: `trusted`, `untrusted` or `unknown-trust` for a project
  scope, `null` otherwise. `orphaned`: a local scope whose project path no longer exists.
  `error`: `null` or `{ "code", "hint" }`, by path and position only.
- `class`: see [Inventory](#inventory). `hash` and `display_hash`: see
  [Display hashes](#display-hashes). `approval`: `approved`, `disabled`, `pending` or `unknown` for
  a Claude Code `.mcp.json` entry, `null` otherwise. `view`: the redacted view (see
  [Redaction](#redaction)), `null` for names-only and absent entries. `import`: `ok`, the import
  `gaps` (`field`, `reason`), the `replaced` literals (`field`, `kind` `env`, `bearer-header` or
  `header`, `var`) and the declared `normalizations` (`field`, `kind`), `null` for names-only and
  absent entries.
- `same_name`: `verdict` `duplicate`, `conflict` or `undetermined`, with the places in discovery
  order.

In text mode the inventory prints the repository, then per scope a heading (harness, scope,
path, file, trust, orphaned), its error and one tab-separated line per entry (name, class, hashes,
approval, view, import gaps, shadowing), then the same-name groups and the notices.

### `status`

```json
{ "store": "/home/me/.d-mcp", "initialized": true, "display_hashes": true, "auth": true,
  "missing_variables": ["GITHUB_TOKEN"],
  "notices": ["variables are checked in this shell only; a harness launched from a GUI (a desktop app, a dock or a launcher) can see a different environment"],
  "harnesses": [
    { "harness": "codex", "scope": "user", "file": "/home/me/.codex/config.toml", "status": "read",
      "error": null, "notices": [],
      "entries": [
        { "name": "github", "desired": "resolved", "reason": null, "source": "team",
          "recorded": "owned", "adopted_from": null, "vanish_count": 0, "actual": "present",
          "hash": "<64 hex>", "display_hash": null, "operation": "unchanged",
          "missing_variables": ["GITHUB_TOKEN"], "login": "not-applicable" }
      ] }
  ] }
```

- Harnesses come in adapter order (`codex`, then `claude`), entries sorted by name.
  `missing_variables` (top level) is the sorted union of the entries' lists. `notices` always
  starts with the GUI caveat; with `--auth` it also holds the `claude mcp get` warning.
- `status` of a harness: `read`, `not-present`, `failed` (with `error` `{ "code", "hint" }`),
  `location-changed` or `unsupported`.
- `desired`: `resolved`, `held` or `not-selected`; `reason` (for `held`): `invalid-winner`,
  `source-unavailable` or `not-offered`; `source`: the source of the winning or holding definition.
  `recorded`: `owned`, `pending` or `none`. `adopted_from`: `<harness>:<scope>` or `null`.
  `actual`: `present`, `absent` or `unknown`. `hash`, `display_hash`: see
  [Display hashes](#display-hashes). `operation`: an [operation](#operations) such as `add`,
  `update`, `remove`, `drift`, `vanished`, `held` or `unchanged`, or `null` when the harness was not
  planned. `login`: `logged-in`, `needs-login`, `not-applicable` or `unknown` with `--auth`, `null`
  otherwise.

In text mode each harness prints one line (harness, scope, file, status), its error and notices,
then one tab-separated line per entry (`desired`, `recorded`, `actual`, the next sync operation,
hashes, origin, vanishes, missing variables, login), then the missing variables and the notes.

### `doctor`

The `data` of every run, also on exit 1 (`doctor-failed`).

```json
{ "store": "/home/me/.d-mcp", "repository": "/home/me/work/app", "auth": false,
  "summary": { "ok": 14, "warn": 1, "error": 0 },
  "checks": [
    { "check": "harness-version", "severity": "warn", "harness": "claude", "server": null, "code": null,
      "message": "`claude --version` reports 2.1.312 (Claude Code), which d-mcp was not verified against (verified: 2.1.310 (Claude Code))",
      "hint": "d-mcp may still work; check `d-mcp sync --dry-run` before writing" }
  ] }
```

- `check`: `store`, `state`, `hash-key`, `harness-version`, `discovery`, `harness-config`, `drift`,
  `location-changed`, `pending`, `recurring-vanish`, `shadowing`, `static-token`, `name-length`,
  `credential-names`, `managed-policy` or `login`, in this order (see [Doctor](#doctor)).
  `severity`: `ok`, `warn` or `error`. `harness`, `server`: what the check is about, or `null`.
  `code`: the error code behind the check (such as `config-corrupt`), or `null`. `hint`: what to
  do, or `null`.
- `repository`: the current repository, `null` when the store could not be read. `summary`: the
  number of checks per severity.

In text mode each check prints one tab-separated line (severity, check, harness, message, error
code) and its hint indented below, then the summary.

### `uninstall`

The `data` of every run after planning, also on exit 1, 3 and 4.

```json
{ "store": "/home/me/.d-mcp", "apply": true, "dry_run": false,
  "harnesses": [
    { "harness": "claude", "scope": "user", "file": "/home/me/.claude.json", "status": "ready",
      "error": null, "notices": [],
      "entries": [
        { "name": "notes", "action": "release", "outcome": "applied", "adopted_from": "claude:project",
          "replaced_fields": [ { "field": "env.NOTES_TOKEN", "var": "NOTES_TOKEN" } ],
          "notice": "d-mcp drops its ownership and writes nothing; the entry stays as it is; it reads these variables instead of the literals adoption replaced: NOTES_TOKEN (env.NOTES_TOKEN)" }
      ] }
  ],
  "purge": { "requested": true, "follow_store_link": false, "store_root_symlink": false,
             "target": "/home/me/.d-mcp",
             "entries": ["/home/me/.d-mcp/config.toml", "/home/me/.d-mcp/local", "/home/me/.d-mcp/.lock"],
             "blocked": [ { "reason": "location-changed", "harness": "codex" } ], "deleted": false },
  "sync": null, "notices": [] }
```

- `apply`: whether `--apply` was given; `dry_run`: whether nothing was written (a preview, or
  `--apply --dry-run`).
- `status` of a harness: `ready`, `not-present`, `failed` (with `error`, such as
  `harness-cli-not-found`), `location-changed` or `unsupported`.
- `action`: `remove`, `release`, `forget`, `drifted`, `location-changed` or `unsupported` (see
  [Uninstall](#uninstall)). `outcome`: `planned` (preview), `applied`, `failed` (the removal failed;
  see `sync`; or the step that removes or releases the entry stopped at an error) or `left` (the entry and its record are left). `adopted_from`: `<harness>:<scope>` or
  `null`. `replaced_fields`: the fields whose literals adoption replaced, with their variables.
- `purge`: `requested` (`--purge`), `follow_store_link`, `store_root_symlink`, `target` (the
  directory the entries are deleted from), `entries` (the existing known entries, in deletion
  order), `blocked` (`reason` `location-changed`, `harness-cli-not-found`, `harness-failed` or
  `store-root-symlink`, with the `harness` or `null`) and `deleted`.
- `sync`: after an apply with removals, the [`sync`](#sync-1) document of the scoped removals (one
  entry in `harnesses` per harness with removals); `null` otherwise.

In text mode each harness prints one line, its error and notices, then per entry a line (action,
name, outcome, origin) with its notice; then the purge (`Purge would delete`, `Purged` or `Purge
blocked`) and the notices.
