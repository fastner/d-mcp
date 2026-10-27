# Contributing to d-mcp

d-mcp edits configuration files that belong to users and their coding-agent harnesses. Keep changes small, tested and explicit about the behavior they change.

## Preflight

The Rust toolchain comes from `rust-toolchain.toml` (stable, with `clippy`, `rustfmt` and `llvm-tools-preview`); `rustup` installs it on first use. The extra tools (`cargo-llvm-cov`, `cargo-deny`) are pinned in `mise.toml`; install them with `mise install`.

```sh
mise run check        # cargo fmt --check, cargo clippy -D warnings, cargo test --locked
mise run check:full   # check, plus coverage threshold and cargo deny check, as in CI
```

Run `mise run check` before every commit and `mise run check:full` before opening a PR. CI additionally builds against the MSRV (`rust-version` in `Cargo.toml`) and runs tests on Linux and macOS. Keep the tool versions in `mise.toml` in sync with the `cargo install --version` pins in `.github/workflows/ci.yml`.

## Test rules

- Tests never use the network.
- Every mutation of user data has a test for its failure path (the write fails or is refused, and the existing data is left intact).
- Every mutating command has `--dry-run` coverage that proves nothing is written.
- Command-level tests live in `tests/` and build on `tests/common/mod.rs`. Every command runs with `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the `XDG_*` directories inside a fresh temporary directory and without `D_MCP_STORE`, so no test touches the real `~/.d-mcp` or real harness configuration. Never bypass this helper.
- Definition fixtures live in `tests/fixtures/definitions/`. A file in `invalid/<rule-code>/` must fail with exactly that rule code; a file in `valid/` must pass. The file stem is the server name. A new rule code needs a fixture and an entry in the rule-code table of `docs/reference.md`.
- The error-code table in `docs/reference.md` (between the `error-codes` markers) is parsed by a unit test; keep every error code and its exit code in sync with `src/error.rs`.
- The coverage threshold lives in `coverage-threshold` (percent of lines). It may be raised but never lowered.

## Dependencies

`cargo deny check` enforces the policy in `deny.toml`: allowed licenses, no yanked crates, no wildcard versions, crates.io only. Add exceptions only when unavoidable, keep them narrow (one crate or advisory) and comment why.

## Schema rule

Until 1.0.0, persisted schemas stay at version 1 and fields are added without migration. From 1.0.0 on, every schema change bumps the version and ships a migration.

## Commits and pull requests

- Use Conventional Commits (`feat:`, `fix:`, `docs:`, `chore:` ...), in English.
- PRs are squash-merged, so the PR title becomes the commit subject; the `pr-title` workflow checks it.
- Pin every GitHub Action by full commit SHA, with the version in a trailing comment.
