# d-mcp reference

This is the reference for the harness-independent core of d-mcp: the store, the definition format,
the secret rules, sources, selections, resolution, exit and error codes, and the JSON documents.
Later work packages (harness adapters, sync, adoption, approvals) build on it.

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
├── .lock              advisory lock file
└── local/             root of the built-in local source
    └── servers/       definitions of the built-in local source
```

Directories are created with mode `0700`, files with mode `0600`. Later work packages create
`state.toml`, `lock.toml` and `approvals.toml` on first write. Every persisted file carries
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
| `harness.codex`, `harness.claude` | no | Opaque per-harness override tables. |

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
which leaves at least 25 characters for tool names; WP3 re-verifies the 64-character assumption.
Reserved names are adapter-specific (WP2, WP3).

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
sorts first, independent of the input order. WP2 turns `hold` into its *Held*
operation (keep the existing managed entry unchanged), and WP5 adds the reason
`pending-approval`.

## Exit codes

| Code | Meaning | Cases |
|---|---|---|
| 0 | Success | Including resolution problems, holds and warnings |
| 1 | Expected failure | Store not initialized; invalid arguments; rejected `source add`; `init` into a non-empty directory that is not a store |
| 2 | Usage error | clap usage errors (unknown flags, missing arguments, non-integer priority); clap prints them as text on stderr, also with `--json` |
| 3 | Unsafe state | Corrupt or hand-broken `config.toml`; unsupported schema version; `store-locked` |
| 4 | Environment problem | A missing `HOME`; an unknown current directory; a failed file-system operation |

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
- `data`: the command's document on success, `null` on failure.
- `problems`: an array of problems; only `resolve` reports any.
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
