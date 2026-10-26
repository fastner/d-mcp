# WP1a: Project scaffold

**Plan status:** Not implemented
**Source:** effective-flow plan
**Recommended workflow:** Feature (`effective-flow build`)

## Requirement

This is the first half of work package 1 of the concept `docs/concept/2026-10-26-d-mcp.md`. By
user decision, WP1 was split so that the toolchain, CI and contributor conventions are proven
green before any domain code exists. WP1a delivers an empty but fully wired Rust project:

- the crate with an empty library and binary;
- the toolchain, lints, formatting and dependency policy;
- the mise tasks, the CI and the PR-title check;
- the contributor docs.

The workflow is Feature: it creates the product's buildable foundation.

**Dependencies and re-planning.** WP1a depends on nothing. WP1b
(`docs/plan/2026-10-26-wp1-core-model-store-and-resolver.md`) builds on it and starts once this
plan's status is `Implemented`.

**Context:**

- **Verified:** the repository holds only `LICENSE` (MIT), `AGENTS.md`, `CLAUDE.md`,
  `.gitignore` and `docs/`, with the remote `github.com/fastner/d-mcp`.
- **Verified:** no Rust toolchain is installed on this machine.
- **Verified (Dalo conventions):**
  - a single crate with library and binary, edition 2024, `rust-version = "1.94"`;
  - `[lints]`: `unsafe-code = "forbid"`, `nonstandard-style = "deny"`, clippy `all = "warn"`, plus
    `redundant-clone`, `manual-ok-or` and `needless-collect`;
  - `rustfmt.toml` with edition 2024 and Unix newlines;
  - a `deny.toml` license allow-list, `yanked = deny`, `wildcards = deny`, crates.io only;
  - `coverage-threshold`, measured with `cargo llvm-cov` in CI;
  - mise tasks;
  - SHA-pinned actions and a Conventional Commit PR-title check.

## Architecture decisions

- **Layout like Dalo (user decision).** One crate, `d-mcp`, with `src/lib.rs` (a `run` entry
  point that returns the exit code) and `src/main.rs`, which calls it. WP1a ships no commands,
  only `--version` and `--help` through clap.
- **Dependencies in WP1a:** only `clap` 4 (derive), plus `assert_cmd` and `predicates` for the
  first smoke test. Everything else arrives with WP1b, so `cargo-deny` sees exactly the
  dependencies in use.
- **Toolchain:** `rust-toolchain.toml` pins the stable channel with `clippy`, `rustfmt` and
  `llvm-tools-preview`.
- **mise:**
  - pinned `cargo:cargo-llvm-cov` and `cargo:cargo-deny`;
  - task `check`: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D
    warnings`, and `cargo test --locked`;
  - task `check:full`: `check` plus coverage with the threshold and `cargo deny check`.
- **CI** (`.github/workflows/ci.yml`), with every action pinned by SHA:
  - fmt and clippy;
  - tests on ubuntu and macos;
  - MSRV, read from `rust-version` in `Cargo.toml`;
  - `cargo-deny`;
  - coverage through `cargo llvm-cov --workspace --all-features --summary-only --fail-under-lines
    "$(cat coverage-threshold)"`;
  - an aggregate `check` job.
- **Coverage threshold:** starts at 85. The empty scaffold reaches it through the smoke test. The
  threshold may be raised but never lowered.
- **Schema rule** for `CONTRIBUTING.md`: until 1.0.0 (WP7), persisted schemas stay at version 1
  and fields are added without migration. From 1.0.0 on, every change bumps the version and
  ships a migration.

## Affected files

| File | Description |
|---|---|
| `Cargo.toml`, `Cargo.lock` | Crate `d-mcp` (lib plus bin), edition 2024, `rust-version = "1.94"`, `[lints]`, minimal dependencies |
| `rust-toolchain.toml` | Stable toolchain with `clippy`, `rustfmt` and `llvm-tools-preview` |
| `rustfmt.toml` | Edition 2024, Unix newlines |
| `deny.toml` | License allow-list and registry rules |
| `coverage-threshold` | `85` |
| `mise.toml`, `mise.lock` | Pinned tools and the `check`, `check:full` tasks |
| `.github/workflows/ci.yml` | CI jobs as listed above |
| `.github/workflows/pr-title.yml` | Conventional Commit PR-title check |
| `.gitignore` | Add `target/` |
| `AGENTS.md` | Keep the Effective Flow pointer. Add "What this is", "Language" (English), "Commits" (Conventional Commits), "Preflight" (`mise run check`), and "Safety" (never write unmanaged harness state, never persist or print secrets) |
| `CONTRIBUTING.md` | Preflight; test rules (no network, a failure-path test for every mutation, dry-run coverage); the schema rule |
| `src/lib.rs`, `src/main.rs` | Empty `run` entry point and binary |
| `tests/smoke.rs` | `d-mcp --version` prints the crate version and exits 0 |

## Implementation details

### Approach

1. Install the stable Rust toolchain per `rust-toolchain.toml` (rustup or mise), and install the
   mise tools.
2. Create the crate, lints, formatting, `deny.toml` and the threshold.
3. Add the smoke test and run `mise run check` and `mise run check:full` locally.
4. Add the CI and PR-title workflows with SHA-pinned actions.
5. Add the `AGENTS.md` and `CONTRIBUTING.md` sections.
6. Open the PR and make every CI job green.

### Component structure

Only `lib.rs` and `main.rs`. WP1b adds every module.

### State management

Not relevant.

### API integration

Not relevant.

### Styling approach

Not relevant.

### Accessibility

Not relevant.

### Edge cases

- **The installed stable toolchain is older than 1.94:** lower the `rust-version` floor to the
  oldest version that builds, and record the change in the PR.
- **`cargo-deny` flags a license or advisory of a transitive dependency:** add a narrow,
  commented exception, as Dalo's `CONTRIBUTING.md` requires.
- **The MSRV job fails because the library uses newer APIs:** adjust the floor, never pin the
  dependency to an older version without a reason.

## Acceptance criteria

- [ ] `mise run check` and `mise run check:full` pass locally, and every CI job, including MSRV,
      `cargo-deny`, coverage and the PR-title check, is green on the WP1a pull request.
- [ ] `d-mcp --version` prints the crate version and exits 0, verified by `tests/smoke.rs`.
- [ ] Every workflow action is pinned by a full commit SHA.
- [ ] `AGENTS.md` keeps the Effective Flow pointer and has the new sections; `CONTRIBUTING.md`
      contains the preflight, the test rules and the schema rule.

## Validation plan

- Run `mise run check` and `mise run check:full` locally.
- Check the CI run on the PR.
- Review the workflow files for SHA pins.

## Assumptions and open points

- Dalo's standards tooling (`@sebastian-software/standards`) is not adopted.

## Plan review

**Result:** Approved

### Summary

| Area | Critical | Important | Note |
|---|---:|---:|---:|
| Architecture | 0 | 0 | 0 |
| Security | 0 | 0 | 0 |
| Data protection | 0 | 0 | 0 |
| Error cases | 0 | 0 | 0 |
| Testability | 0 | 0 | 0 |
| Scope | 0 | 1 | 0 |
| Maintainability | 0 | 0 | 1 |

### Findings

- **Scope, Important.** This plan was carved out of WP1 after the deep review found WP1 too large
  for one PR. Without a local toolchain, scaffold and domain errors would have shared one feedback
  loop. Adjusted by user decision: WP1a (this plan) and WP1b.
- **Maintainability, Note.** Schema evolution before 1.0.0 had no rule. Adjusted: the schema rule
  in `CONTRIBUTING.md`.

## Open points

- No open points.
