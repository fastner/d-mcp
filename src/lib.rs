//! Core library of `d-mcp`.
//!
//! The binary in `src/main.rs` is a thin wrapper around [`run`]. Keeping the
//! entry point in the library lets tests drive the command-line interface
//! in-process through [`run_from`] and lets later work packages add global
//! options and subcommands to [`Cli`] without touching the binary.
//!
//! Module boundaries: [`definition`], [`credential`], [`config`] and
//! [`resolver`] do no I/O, and [`resolver`] is pure. [`source`] reads files.
//! [`store`] owns every write and the lock. [`cli`] parses, dispatches and
//! renders through [`output`].

pub mod cli;
pub mod config;
pub mod credential;
pub mod definition;
pub mod error;
pub mod output;
pub mod resolver;
pub mod rule;
pub mod source;
pub mod spanned;
pub mod store;

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use clap::Parser;

pub use cli::Cli;

/// Runs `d-mcp` with the arguments of the current process.
///
/// Returns the exit code the process should terminate with.
#[must_use]
pub fn run() -> ExitCode {
    run_from(std::env::args_os())
}

/// Runs `d-mcp` with the given arguments, the first being the program name.
///
/// Help and version output go to stdout with exit code 0; usage errors go to
/// stderr with clap's usage exit code 2. If writing help or version output
/// fails for a reason other than a broken pipe, the exit code is 1 instead.
/// A parsed command runs against the process environment; its exit code is
/// documented in `docs/reference.md`.
#[must_use]
pub fn run_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match Cli::try_parse_from(args) {
        Ok(cli) => {
            let environment = store::Environment::from_process();
            let code = cli::execute(
                &cli,
                &environment,
                &mut io::stdout().lock(),
                &mut io::stderr().lock(),
            );
            ExitCode::from(code)
        }
        Err(error) => exit_code_after_print(error.print(), error.exit_code()),
    }
}

/// Chooses the exit code once clap has written its help, version, or error
/// output.
///
/// Only a successful outcome can be overturned: when clap's code is 0 (help
/// or version) and the write failed, for example on a full disk, the run
/// fails with exit code 1, so `d-mcp --version >/dev/full` does not report
/// success. A broken pipe is not treated as a failure, because the reader
/// closed the stream on purpose, as in `d-mcp --help | head -n 1`. A non-zero
/// clap code, such as the usage error code 2, is always kept, so
/// `d-mcp --bogus 2>/dev/full` still exits 2.
fn exit_code_after_print(printed: io::Result<()>, clap_code: i32) -> ExitCode {
    match printed {
        Err(error) if clap_code == 0 && error.kind() != io::ErrorKind::BrokenPipe => {
            ExitCode::FAILURE
        }
        _ => exit_code_from(clap_code),
    }
}

/// Maps a clap exit status to an [`ExitCode`], falling back to 1 for values
/// outside the portable `u8` range.
fn exit_code_from(code: i32) -> ExitCode {
    u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_succeeds() {
        assert_eq!(run_from(["d-mcp", "--version"]), ExitCode::SUCCESS);
    }

    #[test]
    fn unknown_argument_fails_with_usage_code() {
        assert_eq!(run_from(["d-mcp", "--no-such-flag"]), ExitCode::from(2));
    }

    #[test]
    fn write_failure_turns_success_into_failure() {
        let full = io::Error::new(io::ErrorKind::StorageFull, "no space left on device");
        assert_eq!(exit_code_after_print(Err(full), 0), ExitCode::FAILURE);
    }

    #[test]
    fn write_error_with_code_2_stays_2() {
        let full = io::Error::new(io::ErrorKind::StorageFull, "no space left on device");
        assert_eq!(exit_code_after_print(Err(full), 2), ExitCode::from(2));
    }

    #[test]
    fn broken_pipe_keeps_clap_exit_code() {
        let closed = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(exit_code_after_print(Err(closed), 0), ExitCode::SUCCESS);
        let closed = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(exit_code_after_print(Err(closed), 2), ExitCode::from(2));
    }

    #[test]
    fn successful_print_keeps_clap_exit_code() {
        assert_eq!(exit_code_after_print(Ok(()), 2), ExitCode::from(2));
    }

    #[test]
    fn out_of_range_status_maps_to_failure() {
        assert_eq!(exit_code_from(256), ExitCode::FAILURE);
        assert_eq!(exit_code_from(-1), ExitCode::FAILURE);
        assert_eq!(exit_code_from(2), ExitCode::from(2));
    }
}
