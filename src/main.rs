//! Binary entry point of `d-mcp`; all behavior lives in the library.

use std::process::ExitCode;

fn main() -> ExitCode {
    d_mcp::run()
}
