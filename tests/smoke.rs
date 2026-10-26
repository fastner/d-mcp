//! Smoke tests that run the compiled `d-mcp` binary.

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

/// Returns a command for the `d-mcp` binary, resolved at compile time.
fn d_mcp() -> Command {
    cargo_bin_cmd!("d-mcp")
}

#[test]
fn version_prints_crate_version_and_exits_zero() {
    d_mcp()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")))
        .stderr(predicate::str::is_empty());
}

#[test]
fn help_prints_usage_and_exits_zero() {
    d_mcp()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: d-mcp"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn unknown_argument_fails_with_usage_error() {
    d_mcp()
        .arg("--no-such-flag")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--no-such-flag"));
}

#[test]
fn bare_invocation_shows_help_and_fails() {
    d_mcp()
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Usage: d-mcp"));
}
