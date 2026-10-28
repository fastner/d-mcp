# d-mcp reference

This is the reference for d-mcp: the store, the definition format, the secret rules, sources,
selections, resolution, override validation, sync with the Codex and Claude Code adapters, the
write state, exit and error codes, and the JSON documents. Later work packages (adoption,
approvals) build on it.

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
| `d-mcp sync [--harness claude\|codex]... [--take-over <server>]... [--relocate]` | mutating | Write the selected servers into the harnesses (see [Sync](#sync)). |
| `d-mcp unmanage <server> [--harness claude\|codex]...` | mutating | Stop managing a server's harness entries (see [Unmanage](#unmanage)). |

Global options, accepted before or after the command:

- `--store <path>`: the store directory (see [Store location](#store-location)).
- `--json`: print exactly one JSON document on stdout (see [JSON documents](#json-documents)).
- `--dry-run`: show what a mutating command would do. A dry run takes no lock and creates, changes
  or deletes no file or directory, `.lock` included. On a read-only command, `--dry-run` has no
  effect and prints a warning on stderr; stdout stays valid JSON.

Mutating commands take the store lock, then read `config.toml`, then write. A mutating command on
an uninitialized store fails before it tries to lock. Read-only commands take no lock and succeed
while another process holds it.

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
├── .lock              advisory lock file
└── local/             root of the built-in local source
    └── servers/       definitions of the built-in local source
```

Directories are created with mode `0700`, files with mode `0600`. `sync` creates `state.toml` on
its first write; later work packages create `lock.toml` and `approvals.toml`. Every persisted file carries
`schema_version = 1`; another version fails with `schema-unsupported` (exit 3).

`init`:

- creates the missing parts of the layout and reports each created path, in creation order;
- is idempotent: on a complete store it creates nothing and succeeds;
- completes a partial store: a directory that holds only known store entries (`config.toml`,
  `local`, `.lock`, `state.toml`, `lock.toml`, `approvals.toml`);
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
| `harness.codex`, `harness.claude` | no | Per-harness override tables. `harness.codex` is checked against the Codex allow-list (see [Override validation](#override-validation)); `harness.claude` stays opaque until the Claude Code adapter. |

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
  tables. WP4 uses it to detect duplicates across harnesses.

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
  `unknown-take-over` (exit 1). WP4 adoption reuses this operation.
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
write is promoted and cleared by `--take-over` (and by WP4 adoption). The Claude Code adapter rolls
back only entries with the marker, so foreign content never reaches a command line.

### Dry run and lock

`sync` and `unmanage` fail on an uninitialized store before locking. `sync` holds the store lock
for the whole run. `sync --dry-run` takes no lock, writes neither `state.toml` nor a harness file,
creates no file or directory, and treats a settled `pending` record as owned for planning only.
`unmanage --dry-run` only reports.

### Output rules

Until WP4, sync output (text, JSON and errors) names only the harness, scope, file path, entry
name, operation, outcome, hold reason, gaps and positions, plus the hashes of owned entries. It
never contains field values, hashes of foreign or unowned entries, child process output, or
`toml_edit` or `serde_json` error text. Shadowing notices name the entry and the project path
only.

### Sync exit codes

| Exit | When |
|---:|---|
| 0 | Everything was applied, unchanged, held or re-added after `vanished`, or the harness is not present. `note` gaps do not change the code. |
| 1 | `sync-blocked`: at least one entry is blocked by drift, a collision, a location change, an unsupported entry or root style, or an `important` gap; `harness-cli-failed`: a harness CLI call of an entry failed; `harness-no-neutral-dir`: no neutral working directory for a harness CLI, so nothing was run for that harness. |
| 3 | `harness-file-changed`, `harness-config-invalid`, `harness-edit-failed`, `harness-verify-failed`; also `state-corrupt` and `schema-unsupported` for `state.toml`. |
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
- Profile files (`$CODEX_HOME/<profile>.config.toml`) and project scope are not handled in v1.

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
run without a shell, with a timeout and an output cap, and with the resolved `CODEX_HOME`:

| `auth_status` | State |
|---|---|
| `o_auth` | `logged-in` |
| `not_logged_in` | `needs-login` |
| `unsupported`, `bearer_token` | `not-applicable` |
| anything else, a missing binary, a failure, a timeout or oversized output | `unknown` |

`sync` never calls it; `status --auth` (WP4) is its first consumer. d-mcp never reads Codex token
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
- Only user-scope entries (top-level `mcpServers`) are written. Project scope (`.mcp.json`) is not
  handled; local-scope entries (`projects.<path>.mcpServers`) are read by name only, to report
  shadowing.

### Reading

d-mcp deserializes only the user-scope `mcpServers.<name>` values (every key of each entry, for the
hash) and the names of each project's local-scope `mcpServers`; every other key, such as account
data, caches and history, is skipped unread, and no file bytes are kept. Invalid JSON (also a top
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
  Claude Code's own diagnostics name a missing variable. WP1's `variable-syntax` rule keeps `$` and
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

`sync` never calls it and never runs `--version`; `status --auth` (WP4) is its first consumer.
`claude mcp get` connects to the server to check its health. d-mcp never reads Claude Code's token
storage.

### Limits

- A running Claude Code session rewrites the file from memory, so it can undo a d-mcp add (the
  entry is `vanished` on the next sync and re-added) or bring back an entry d-mcp removed (it is
  then unowned and a `collision` if still selected).
- A same-name local-scope entry shadows the managed user entry in that project. `sync` reports it
  as a notice per project path. `add-json -s user` accepts a name that exists in local scope
  (verified against 2.1.310).
- Managed-policy deny lists can block user servers; d-mcp does not see them.

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
| 1 | Expected failure | Store not initialized; invalid arguments; rejected `source add`; `init` into a non-empty directory that is not a store; a blocked sync entry; a failed harness CLI call; `unmanage` of an unmanaged server |
| 2 | Usage error | clap usage errors (unknown flags, missing arguments, non-integer priority, an unknown `--harness` value); clap prints them as text on stderr, also with `--json` |
| 3 | Unsafe state | Corrupt or hand-broken `config.toml` or `state.toml`; unsupported schema version; `store-locked`; a harness file that changed, is invalid, could not be edited or did not verify |
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
| `home-unset` | 4 | `HOME` is unset or empty but needed. |
| `cwd-unavailable` | 4 | The current directory cannot be determined. |
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
- `data`: the command's document on success, `null` on failure. `sync` is the exception: on exit 1
  and 3 (and 4 for a harness-level environment problem) it still carries its document with the
  per-entry outcome, together with `ok: false` and the aggregate `error`. Global errors carry
  `data: null`.
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
{ "store": "/home/me/.d-mcp", "created": ["/home/me/.d-mcp", "/home/me/.d-mcp/.lock", "/home/me/.d-mcp/config.toml", "/home/me/.d-mcp/local", "/home/me/.d-mcp/local/servers"], "dry_run": false }
```

`created` lists absolute paths in creation order: root, `.lock`, `config.toml`, `local`,
`local/servers`; empty when the store is complete. A dry run lists what would be created, in the
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

Servers are sorted by name. Until WP4, `resolve` shows only name, status, reason, origin source
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
        "hash": "<64 hex>", "error": null,
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
- `hash`: the read-back hash of an owned entry, `null` otherwise (never the hash of a foreign
  entry).
- `gaps`: `field`, `severity` (`note` or `important`) and `reason`.

Harnesses come in adapter order (`codex`, then `claude`), entries sorted by name; problems are the
resolver's (see [Problems](#problems)). In text
mode each harness prints one line, then one line per entry (`operation`, name, outcome), then its
notices and gaps; a dry run ends with "Dry run: nothing was written."

### `unmanage`

```json
{ "server": "github", "harnesses": ["codex"], "selection": ["claude"], "converted_from_all": true, "selection_removed": false, "dry_run": false }
```

`harnesses` lists the harnesses whose records were removed; `selection` is the server's harness list
afterwards (`null` when the server is not selected any more); `converted_from_all` tells whether an
`all` selection became an explicit list.
