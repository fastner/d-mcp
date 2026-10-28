//! The shared external-program runner.
//!
//! Programs run without a shell, with stdin connected to null and an explicit
//! environment: the inherited one plus the variables the caller sets (for
//! example the resolved `CODEX_HOME`, or WP3's child-environment injection).
//! stdout and stderr are drained on reader threads with a size cap; the
//! timeout polls `try_wait`.
//!
//! **Child output is never printed.** stdout is returned in memory for
//! parsing only, and stderr is drained and dropped, because a harness CLI can
//! echo foreign secrets from a broken config line. Errors name only the
//! program, the exit status or signal, and `timeout` or `not-found`.
//!
//! Known limit: on timeout only the direct child is killed, and the reader
//! threads are not joined afterwards. A grandchild (for example the native
//! binary behind the npm `codex` shim) may outlive the kill and keep the pipes
//! open; killing the whole process group would need `unsafe` or `libc`.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Default timeout of a run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cap on the captured stdout, in bytes.
pub const DEFAULT_MAX_OUTPUT: usize = 1024 * 1024;
/// Interval of the `try_wait` polling.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// One program invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The program: an absolute path, or a bare name looked up on `PATH`.
    pub program: PathBuf,
    /// The arguments.
    pub args: Vec<OsString>,
    /// Variables set on top of the inherited environment.
    pub env: Vec<(OsString, OsString)>,
}

impl Invocation {
    /// An invocation without arguments or extra variables.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// Adds arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets a variable in the child environment (the WP3 injection hook).
    #[must_use]
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }
}

/// How a finished program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with a code.
    Code(i32),
    /// It was terminated by a signal (Unix).
    Signal(i32),
    /// Neither is known.
    Unknown,
}

/// The output of a finished program. Only stdout is kept.
#[derive(Clone, PartialEq, Eq)]
pub struct Output {
    /// How it ended.
    pub exit: Exit,
    /// stdout, up to the cap. For parsing only; never print it.
    pub stdout: Vec<u8>,
    /// Whether stdout exceeded the cap and was cut.
    pub truncated: bool,
}

impl Output {
    /// Whether the program exited with code 0.
    #[must_use]
    pub fn success(&self) -> bool {
        self.exit == Exit::Code(0)
    }
}

impl fmt::Debug for Output {
    /// Child output can hold secrets; only its size is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("exit", &self.exit)
            .field("stdout", &format_args!("<{} bytes>", self.stdout.len()))
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Why a program could not be run to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The program was not found.
    NotFound {
        /// The program.
        program: PathBuf,
    },
    /// The program did not finish, or did not close its output, in time.
    Timeout {
        /// The program.
        program: PathBuf,
    },
    /// The program could not be started.
    Spawn {
        /// The program.
        program: PathBuf,
        /// The I/O error kind (the OS message is not kept).
        kind: std::io::ErrorKind,
    },
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::NotFound { program } => write!(f, "{}: not-found", program.display()),
            RunError::Timeout { program } => write!(f, "{}: timeout", program.display()),
            RunError::Spawn { program, kind } => write!(
                f,
                "{}: cannot start: {}",
                program.display(),
                std::io::Error::from(*kind)
            ),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs external programs with a timeout and an output cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessRunner {
    /// The timeout of a run, output included.
    pub timeout: Duration,
    /// The stdout cap, in bytes.
    pub max_output: usize,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

/// Drains a pipe, keeping at most `cap` bytes, and reports the kept bytes and
/// whether more were seen.
fn drain(mut pipe: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 8192];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if read > room {
                    truncated = true;
                }
                kept.extend_from_slice(&buffer[..read.min(room)]);
            }
        }
    }
    (kept, truncated)
}

impl ProcessRunner {
    /// Runs an invocation. A bare program name is looked up in `path`
    /// (the `PATH` value d-mcp was given), never through a shell.
    ///
    /// # Errors
    ///
    /// [`RunError::NotFound`], [`RunError::Timeout`] (also when a grandchild
    /// keeps the output open after the child exited) and [`RunError::Spawn`].
    pub fn run(&self, invocation: &Invocation, path: Option<&OsStr>) -> Result<Output, RunError> {
        let program = if invocation.program.is_absolute() {
            invocation.program.clone()
        } else {
            find_program(path, invocation.program.as_os_str()).ok_or_else(|| {
                RunError::NotFound {
                    program: invocation.program.clone(),
                }
            })?
        };
        let mut command = Command::new(&program);
        command
            .args(&invocation.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &invocation.env {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                RunError::NotFound {
                    program: program.clone(),
                }
            } else {
                RunError::Spawn {
                    program: program.clone(),
                    kind: error.kind(),
                }
            }
        })?;
        let deadline = Instant::now() + self.timeout;
        let (sender, receiver) = mpsc::channel();
        if let Some(stdout) = child.stdout.take() {
            let sender = sender.clone();
            let cap = self.max_output;
            thread::spawn(move || {
                let _ = sender.send(drain(stdout, cap));
            });
        }
        if let Some(stderr) = child.stderr.take() {
            // stderr is drained so the child never blocks on it, then
            // dropped unread.
            thread::spawn(move || {
                let _ = drain(stderr, 0);
            });
        }
        drop(sender);
        let timeout = || RunError::Timeout {
            program: program.clone(),
        };
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(timeout());
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    let _ = child.kill();
                    return Err(RunError::Spawn {
                        program: program.clone(),
                        kind: error.kind(),
                    });
                }
            }
        };
        // A grandchild that holds stdout open keeps the reader from seeing
        // EOF; the remaining time bounds the wait.
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (stdout, truncated) = receiver.recv_timeout(remaining).map_err(|_| timeout())?;
        Ok(Output {
            exit: exit_of(status),
            stdout,
            truncated,
        })
    }
}

#[cfg(unix)]
fn exit_of(status: std::process::ExitStatus) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Unknown,
    }
}

#[cfg(not(unix))]
fn exit_of(status: std::process::ExitStatus) -> Exit {
    status.code().map_or(Exit::Unknown, Exit::Code)
}

/// Looks a bare program name up in a `PATH` value without running it.
/// Relative `PATH` entries are skipped. On Unix the file must be executable.
#[must_use]
pub fn find_program(path: Option<&OsStr>, name: &OsStr) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_caps_and_flags_truncation() {
        let (kept, truncated) = drain(&b"abcdef"[..], 4);
        assert_eq!(kept, b"abcd");
        assert!(truncated);
        let (kept, truncated) = drain(&b"ab"[..], 4);
        assert_eq!(kept, b"ab");
        assert!(!truncated);
    }

    #[test]
    fn errors_name_program_and_reason_only() {
        let program = PathBuf::from("/bin/codex");
        assert_eq!(
            RunError::NotFound {
                program: program.clone()
            }
            .to_string(),
            "/bin/codex: not-found"
        );
        assert_eq!(
            RunError::Timeout {
                program: program.clone()
            }
            .to_string(),
            "/bin/codex: timeout"
        );
        assert!(
            RunError::Spawn {
                program,
                kind: std::io::ErrorKind::PermissionDenied
            }
            .to_string()
            .starts_with("/bin/codex: cannot start")
        );
        let output = Output {
            exit: Exit::Code(0),
            stdout: b"ghp_secret".to_vec(),
            truncated: false,
        };
        assert!(output.success());
        assert!(!format!("{output:?}").contains("ghp_secret"));
    }

    #[test]
    fn missing_programs_are_not_found() {
        let runner = ProcessRunner::default();
        let error = runner
            .run(
                &Invocation::new("d-mcp-no-such-program"),
                Some(OsStr::new("/nonexistent")),
            )
            .unwrap_err();
        assert!(matches!(error, RunError::NotFound { .. }));
        assert!(matches!(
            runner.run(&Invocation::new("x"), None).unwrap_err(),
            RunError::NotFound { .. }
        ));
        let error = runner
            .run(&Invocation::new("/nonexistent/d-mcp-prog"), None)
            .unwrap_err();
        assert!(matches!(error, RunError::NotFound { .. }));
        assert_eq!(
            find_program(Some(OsStr::new("relative:dir")), OsStr::new("sh")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn runs_without_a_shell_and_reports_exit() {
        let runner = ProcessRunner::default();
        let path = std::env::var_os("PATH");
        let output = runner
            .run(
                &Invocation::new("sh")
                    .args(["-c", "printf \"$D_MCP_TEST_VAR\"; echo err >&2; exit 3"])
                    .env("D_MCP_TEST_VAR", "value"),
                path.as_deref(),
            )
            .unwrap();
        assert_eq!(output.stdout, b"value");
        assert_eq!(output.exit, Exit::Code(3));
        assert!(!output.success());
        let output = runner
            .run(
                &Invocation::new("sh").args(["-c", "kill -9 $$"]),
                path.as_deref(),
            )
            .unwrap();
        assert_eq!(output.exit, Exit::Signal(9));
    }
}
