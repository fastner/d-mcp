# ADR 0002: Secret References Only

- Status: Accepted
- Date: 2026-10-26, amended 2026-10-31 (WP5 approval view)
- Source: concept `docs/concept/2026-10-26-d-mcp.md`, WP1b plan "Core model, store and resolver"

## Context

MCP servers need credentials: API tokens, client secrets, database passwords. Definitions are
shared through sources and may end up in Git repositories, and d-mcp prints definitions, problems
and errors that users paste into issues and chats. A secret that enters a definition, or that
leaks through an error message, is hard to take back.

## Decision

Definitions hold secret *references* only, limited to the forms both harnesses can express, and
d-mcp never prints or persists a secret value.

- Environment entries are either forwards (`{ forward = true }`, passed under the same name) or
  non-secret literals (`{ value = "..." }`). HTTP auth reads tokens from named variables
  (`bearer_env`, `header_env`, `client_secret_env`). Renamed forwards are not modelled.
- Variable syntax (`$VAR`, `${VAR}`, `${env:VAR}`, `{env:VAR}`) is rejected in commands,
  arguments, URLs, literals and override strings, so no harness-specific expansion sneaks in.
- Two explicit heuristics, `secret_name_heuristic` and `secret_value_heuristic`, reject likely
  secrets in every position that can carry one: literal names and values, arguments and
  `--flag=value`, `NAME=value` tokens, URL path segments and query values, descriptions, client
  ids, scopes, override strings and the keys of override strings (except env-reference keys
  whose value is a variable name, such as `bearer_token_env_var` or keys in
  `env_http_headers`). URLs embedded in a token (`jdbc:postgresql://...`) get the URL checks.
  User info is rejected wherever
  `scheme://` appears in a token, and in Go-style DSNs (`user:password@tcp(host)/db`).
- There is deliberately no "allow literal" escape. Only an `env` literal has a remedy for a
  false positive: turning it into a forward. In arguments, the `url` field and override strings a
  false positive is a hard rejection.
- Parsing is two-phase (spanned TOML, then d-mcp's own walker). Errors carry file, line, column,
  field path and rule code only; parser messages are never printed, and secret-looking keys are
  redacted from field paths. `resolve` shows only variable names, never literal values, `args` or
  override contents.

## Consequences

- A definition that passes validation can be shared without leaking credentials, within the
  limits of the heuristics.
- The heuristics produce false positives (for example a long random-looking literal) and false
  negatives (a short password). Both are accepted trade-offs; the fixture matrix documents every
  rule.
- Users must export credentials in their environment; d-mcp never stores them.
- Diagnostics are less specific than a parser message, but they are always safe to share.

**Amendment 2026-10-31 (WP5): the approval view.** "`resolve` shows only variable names, never
literal values, `args` or override contents" holds for every definition except one case, recorded
in ADR 0006: for a winner from a Git source, the approval view (the `source refresh` change set,
`approve`, `accept-risk` and `resolve`'s findings) also shows `command`, `args`, the URL path and
`harness.codex.cwd`, and the findings name the matched launcher arguments. Those fields have passed
the secret heuristics above; env values, header values and all other override values stay hidden,
and no secret value is persisted or printed.
