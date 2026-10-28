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
- `PATH` is inherited, and a `codex` executable on it makes Codex count as present. Tests that depend on the presence or absence of Codex control `PATH` through `TestEnv::with_path` or `TestEnv::without_codex`; the login-state tests install `tests/support/fake-codex.sh` as `codex` in a controlled `PATH` directory. The fake records its calls in `$CODEX_HOME/fake-codex.log`, takes its behavior from `$CODEX_HOME/fake-codex-mode`, and prints canary strings that must never reach d-mcp's output or store. Because that `PATH` holds only the fake, it uses shell builtins only and calls `sleep` by absolute path; keep it that way. Timing assertions use generous bounds for slow CI machines.
- Tests never run the real `codex`. When you verify Codex behavior by hand, export `HOME` and `CODEX_HOME` to a fresh temporary directory in the same shell command, so your own `~/.codex` is never touched. `tests/fixtures/codex-rewrite/` holds a file d-mcp wrote and the same file after the real Codex re-saved it; recapture both this way when the rendering or the verified Codex version changes.
- Codex config fixtures live in `tests/fixtures/codex/` and must stay byte-exact: `crlf.toml` and `trailing-comments-crlf.toml` hold CRLF line endings and `no-trailing-newline.toml` has no final newline, so do not let an editor normalize them; `tests/fixtures/codex/.gitattributes` marks them `-text` so Git keeps their bytes.
- Override fixtures for the Codex allow-list live in `tests/fixtures/codex-overrides/`; a file in `invalid/<rule-code>/` (`override-key`, `override-type`, `override-transport`) must fail with exactly that rule code, a file in `valid/` must pass.
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
