# ADR 0004: OAuth First

- Status: Accepted
- Date: 2026-10-26
- Source: concept `docs/concept/2026-10-26-d-mcp.md`, WP1b plan "Core model, store and resolver"

## Context

Remote MCP servers increasingly support OAuth, and both Claude Code and Codex run the OAuth flow
themselves and keep the tokens in their own credential stores. Static tokens in environment
variables are still common, but they are long-lived and easy to leak. d-mcp must not become a
second credential store, and it cannot offer single sign-on across harnesses without handling
tokens itself.

## Decision

OAuth is the default and preferred auth method for HTTP servers, and tokens never leave the
harness.

- An HTTP definition without an `auth` table means `{ type = "oauth" }` with no scopes.
- OAuth definitions carry only non-secret client parameters: an optional `client_id`, `scopes`,
  and, for confidential clients, the *name* of the variable holding the client secret
  (`client_secret_env`, which requires `client_id`).
- Static credentials are explicit opt-ins: `bearer_env` (a token from a named variable, optionally
  marked `oauth_supported = true` so a later migration can be suggested) and `header_env` (a
  header value from a named variable). `none` declares a server without authentication.
- d-mcp never runs the OAuth flow, never reads harness token stores, and never stores tokens.

## Consequences

- The secure default needs no secret at all in the definition or the user's environment.
- Each harness authenticates separately; there is no single sign-on across harnesses, by design.
- Servers that only support static tokens remain supported, with the variable named explicitly.
- `oauth_supported` lets later tooling nudge users from static tokens towards OAuth without
  guessing.
