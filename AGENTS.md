**Effective Flow project setup:** docs/adr/effective-flow-project-setup.md

## What this is

d-mcp is a Rust CLI plus a bundled agent skill that manages MCP server configuration across coding-agent harnesses, Claude Code and Codex first. The product concept lives at `docs/concept/2026-10-26-d-mcp.md`.

## Language

English everywhere: code, identifiers, comments, docs, commit messages and PR titles.

## Commits

Use Conventional Commits. Pull request titles are checked in CI because the squash title becomes the commit subject.

## Preflight

Run `mise run check` before every commit (format, clippy, tests). `mise run check:full` adds the coverage gate (at least the value in `coverage-threshold`) and `cargo deny check`. See `CONTRIBUTING.md`.

## Safety

- Never write unmanaged harness state; only touch configuration d-mcp manages.
- Never persist or print secrets.
