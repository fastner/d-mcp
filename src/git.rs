//! Guarded `git` operations. This is the only module that runs `git`, and
//! only mutating commands and dry runs call it; read-only commands never do.
//!
//! Every call runs through [`crate::process`] without a shell and carries the
//! same hardening:
//!
//! - `-c core.hooksPath=/dev/null -c core.fsmonitor=false
//!   -c transfer.fsckObjects=true -c gc.auto=0`, so no hook or fsmonitor of a
//!   cloned repository (or of the user's configuration) runs, fetched objects
//!   are checked, and an automatic gc never drops the pin;
//! - `GIT_ALLOW_PROTOCOL=https:ssh:file` (`git:` is unauthenticated, `ext::`
//!   and the other remote helpers run commands), `GIT_PROTOCOL_FROM_USER=0`,
//!   `GIT_TERMINAL_PROMPT=0`, and `GIT_ASKPASS` and `SSH_ASKPASS` set to the
//!   empty value, which disables an askpass program, `core.askPass` included;
//! - on every network call, unless `GIT_SSH_COMMAND`, `GIT_SSH` or
//!   `core.sshCommand` is configured,
//!   `GIT_SSH_COMMAND=ssh -oBatchMode=yes -oConnectTimeout=15`: an `https`
//!   URL can turn into an SSH one through a `url.<base>.insteadOf` rewrite of
//!   the user's configuration, so the typed URL's scheme does not decide;
//! - `GIT_NO_REPLACE_OBJECTS=1`, so no replace ref changes what a commit or
//!   tree reads as;
//! - inherited variables that would redirect, rewrite or trace git are
//!   removed ([`REMOVED_ENV`] and every `GIT_TRACE*`): the repository and
//!   object locations (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_OBJECT_DIRECTORY`,
//!   ...), the ancestry and ref overrides (`GIT_SHALLOW_FILE`,
//!   `GIT_GRAFT_FILE`, `GIT_REPLACE_REF_BASE`, `GIT_QUARANTINE_PATH`) and
//!   `GIT_CONFIG` (read by `git config` only, so it would fake the
//!   `core.sshCommand` lookup). A repository is always named explicitly
//!   through `GIT_DIR`, and every other call runs in a directory d-mcp
//!   created, with `GIT_CEILING_DIRECTORIES` stopping the repository
//!   discovery there;
//! - the user's git configuration is honoured on purpose (proxies, CA
//!   bundles, credential helpers, `insteadOf` rewrites), the variables that
//!   carry it (`GIT_CONFIG_GLOBAL`, `GIT_CONFIG_NOSYSTEM`,
//!   `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`) included: the hardening
//!   `-c` options are appended after `GIT_CONFIG_PARAMETERS` and
//!   `GIT_CONFIG_COUNT`, so they win over every configured value;
//! - user input is validated first ([`validate_url`], [`validate_ref_syntax`],
//!   [`CommitId`]), and `--` or `--end-of-options` precedes positional
//!   arguments where git supports it.
//!
//! Network calls (`clone`, `fetch`, `ls-remote`) have their own 120 s timeout
//! and classify stderr into stable error codes; git's output is never printed
//! or persisted. On a timeout only the direct child is killed: a surviving
//! `ssh` or `git-remote-https` grandchild is a known limit of the runner.

mod validate;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use validate::{
    CommitId, GitUrl, is_object_id, validate_ref_syntax, validate_url, validate_url_syntax,
};

use crate::error::{Error, Result};
use crate::process::{
    DEFAULT_MAX_OUTPUT, Exit, Invocation, Output, ProcessRunner, RunError, StderrClass,
    find_program,
};
use crate::store::Environment;

/// Timeout of the network calls `clone`, `fetch` and `ls-remote`.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout of the local calls.
pub const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
/// The protocols git may use.
pub const ALLOWED_PROTOCOLS: &str = "https:ssh:file";
/// The SSH command used unless the user configured one.
pub const DEFAULT_SSH_COMMAND: &str = "ssh -oBatchMode=yes -oConnectTimeout=15";
/// The ref that keeps the pinned commit reachable in a clone.
pub const PIN_REF: &str = "refs/d-mcp/pin";

/// The configuration overrides of every call.
const HARDENING: [&str; 8] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "transfer.fsckObjects=true",
    "-c",
    "gc.auto=0",
];

/// Inherited variables removed from every call, besides every variable
/// whose name starts with `GIT_TRACE`.
pub const REMOVED_ENV: [&str; 15] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CEILING_DIRECTORIES",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_QUARANTINE_PATH",
    "GIT_CONFIG",
    "GIT_CURL_VERBOSE",
];
/// The prefix of git's trace variables (`GIT_TRACE`, `GIT_TRACE2_EVENT`,
/// `GIT_TRACE_PACKFILE`, ...), all removed from every call.
const TRACE_PREFIX: &str = "GIT_TRACE";

/// One entry of a non-recursive `ls-tree -z --long` listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The mode, such as `100644`, `100755`, `120000`, `160000` or `040000`.
    pub mode: String,
    /// The object type: `blob`, `tree` or `commit`.
    pub kind: String,
    /// The object id (40 or 64 hex characters).
    pub oid: String,
    /// The size of a blob, in bytes; `None` for a tree or a gitlink.
    pub size: Option<u64>,
    /// The raw name bytes.
    pub name: Vec<u8>,
}

/// The located `git` binary and the `PATH` it runs with.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    path: Option<OsString>,
    /// Variables set (`Some`) or removed (`None`) on every call; only tests
    /// use it (to isolate git's configuration).
    extra_env: Vec<(OsString, Option<OsString>)>,
}

impl Git {
    /// Finds `git` on the `PATH` d-mcp was given. Nothing is run.
    ///
    /// # Errors
    ///
    /// [`Error::GitNotFound`].
    pub fn locate(env: &Environment) -> Result<Self> {
        let program =
            find_program(env.path.as_deref(), OsStr::new("git")).ok_or(Error::GitNotFound)?;
        Ok(Self {
            program,
            path: env.path.clone(),
            extra_env: Vec::new(),
        })
    }

    /// The hardened invocation of every call. `cwd` is a directory d-mcp
    /// controls; with `git_dir`, the call works on that repository.
    fn invocation(&self, cwd: &Path, git_dir: Option<&Path>) -> Invocation {
        let ceiling = cwd.parent().unwrap_or(cwd);
        let mut invocation = Invocation::new(&self.program)
            .args(HARDENING)
            .current_dir(cwd)
            .timeout(LOCAL_TIMEOUT);
        for name in REMOVED_ENV {
            invocation = invocation.env_remove(name);
        }
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(TRACE_PREFIX))
            {
                invocation = invocation.env_remove(name);
            }
        }
        invocation = invocation
            .env("GIT_CEILING_DIRECTORIES", ceiling)
            .env("GIT_ALLOW_PROTOCOL", ALLOWED_PROTOCOLS)
            .env("GIT_PROTOCOL_FROM_USER", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("SSH_ASKPASS", "")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        if let Some(dir) = git_dir {
            invocation = invocation.env("GIT_DIR", dir);
        }
        for (name, value) in &self.extra_env {
            invocation = match value {
                Some(value) => invocation.env(name, value),
                None => invocation.env_remove(name),
            };
        }
        invocation
    }

    /// The value of an inherited variable as a call sees it.
    fn inherited(&self, name: &str) -> Option<OsString> {
        match self.extra_env.iter().find(|(set, _)| set == name) {
            Some((_, value)) => value.clone(),
            None => std::env::var_os(name),
        }
    }

    /// A network call: classified stderr, the network timeout and the
    /// batch-mode SSH command unless one is configured. It is set whatever
    /// the scheme of the typed URL, because a `url.<base>.insteadOf` rewrite
    /// can turn an `https` URL into an SSH one.
    fn network(&self, cwd: &Path, git_dir: Option<&Path>) -> Result<Invocation> {
        let mut invocation = self
            .invocation(cwd, git_dir)
            .classify_stderr()
            .timeout(NETWORK_TIMEOUT);
        if !self.ssh_command_configured(cwd)? {
            invocation = invocation.env("GIT_SSH_COMMAND", DEFAULT_SSH_COMMAND);
        }
        Ok(invocation)
    }

    /// Whether the user configured an SSH command (`GIT_SSH_COMMAND`,
    /// `GIT_SSH` or `core.sshCommand`).
    fn ssh_command_configured(&self, cwd: &Path) -> Result<bool> {
        if self.inherited("GIT_SSH_COMMAND").is_some() || self.inherited("GIT_SSH").is_some() {
            return Ok(true);
        }
        let output = self.run(
            "config",
            self.invocation(cwd, None)
                .args(["config", "--get", "core.sshCommand"]),
            DEFAULT_MAX_OUTPUT,
        )?;
        match output.exit {
            Exit::Code(0) => Ok(true),
            Exit::Code(1) => Ok(false),
            _ => Err(failure("config", &output)),
        }
    }

    fn run(&self, operation: &'static str, invocation: Invocation, cap: usize) -> Result<Output> {
        let runner = ProcessRunner {
            timeout: LOCAL_TIMEOUT,
            max_output: cap,
        };
        runner
            .run(&invocation, self.path.as_deref())
            .map_err(|error| match error {
                RunError::NotFound { .. } => Error::GitNotFound,
                RunError::Timeout { .. } => Error::GitTimeout { operation },
                RunError::Spawn { .. } => Error::GitFailed {
                    operation,
                    status: "cannot start".to_owned(),
                },
            })
    }

    fn run_ok(&self, operation: &'static str, invocation: Invocation) -> Result<Output> {
        let output = self.run(operation, invocation, DEFAULT_MAX_OUTPUT)?;
        if output.success() {
            Ok(output)
        } else {
            Err(failure(operation, &output))
        }
    }

    /// Checks a ref with `git check-ref-format --allow-onelevel`, after the
    /// syntax checks of [`validate_ref_syntax`].
    ///
    /// # Errors
    ///
    /// [`Error::SourceRefInvalid`], or a git error.
    pub fn check_ref_format(&self, cwd: &Path, reference: &str) -> Result<()> {
        validate_ref_syntax(reference)?;
        // `check-ref-format` has no `--`; the leading `-` is excluded above.
        let output = self.run(
            "check-ref-format",
            self.invocation(cwd, None)
                .args(["check-ref-format", "--allow-onelevel", reference]),
            DEFAULT_MAX_OUTPUT,
        )?;
        match output.exit {
            Exit::Code(0) => Ok(()),
            Exit::Code(1) => Err(Error::SourceRefInvalid { what: "ref" }),
            _ => Err(failure("check-ref-format", &output)),
        }
    }

    /// Clones `url` into `dest` without a checkout and without submodules,
    /// through the transport even for a local path (`--no-local`, so objects
    /// are checked), with an empty template. With `branch`, that branch or
    /// tag is cloned (`HEAD` points at it).
    ///
    /// # Errors
    ///
    /// A classified git error.
    pub fn clone_repo(
        &self,
        cwd: &Path,
        url: &GitUrl,
        branch: Option<&str>,
        dest: &Path,
    ) -> Result<()> {
        let mut invocation = self.network(cwd, None)?.args([
            "clone",
            "--quiet",
            "--no-checkout",
            "--no-recurse-submodules",
            "--no-local",
            "--template=",
        ]);
        if let Some(branch) = branch {
            validate_ref_syntax(branch)?;
            invocation = invocation.arg(format!("--branch={branch}"));
        }
        let invocation = invocation.arg("--").arg(url.as_str()).arg(dest.as_os_str());
        self.run_ok("clone", invocation).map(|_| ())
    }

    /// Fetches `refspecs` from `url` into the repository at `git_dir`,
    /// without submodules.
    ///
    /// # Errors
    ///
    /// A classified git error.
    pub fn fetch(
        &self,
        cwd: &Path,
        git_dir: &Path,
        url: &GitUrl,
        refspecs: &[String],
    ) -> Result<()> {
        let invocation = self
            .network(cwd, Some(git_dir))?
            .args([
                "fetch",
                "--quiet",
                "--no-recurse-submodules",
                "--no-write-fetch-head",
                "--",
            ])
            .arg(url.as_str())
            .args(refspecs);
        self.run_ok("fetch", invocation).map(|_| ())
    }

    /// Lists the refs of `url` matching `pattern` as (commit, ref name).
    ///
    /// # Errors
    ///
    /// A classified git error.
    pub fn ls_remote(
        &self,
        cwd: &Path,
        url: &GitUrl,
        pattern: &str,
    ) -> Result<Vec<(String, String)>> {
        validate_ref_syntax(pattern)?;
        let invocation = self
            .network(cwd, None)?
            .args(["ls-remote", "--"])
            .arg(url.as_str())
            .arg(pattern);
        let output = self.run_ok("ls-remote", invocation)?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (oid, name) = line.split_once('\t')?;
                is_object_id(oid).then(|| (oid.to_owned(), name.to_owned()))
            })
            .collect())
    }

    /// Resolves `rev` to a commit in the repository at `git_dir`; `None`
    /// when it does not name one.
    ///
    /// # Errors
    ///
    /// A git error other than "no such commit".
    pub fn rev_parse_commit(&self, git_dir: &Path, rev: &str) -> Result<Option<CommitId>> {
        let invocation = self
            .invocation(work_dir(git_dir), Some(git_dir))
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(format!("{rev}^{{commit}}"));
        let output = self.run("rev-parse", invocation, DEFAULT_MAX_OUTPUT)?;
        match output.exit {
            Exit::Code(0) => Ok(CommitId::parse(
                String::from_utf8_lossy(&output.stdout).trim(),
            )),
            Exit::Code(1) => Ok(None),
            _ => Err(failure("rev-parse", &output)),
        }
    }

    /// The branch `HEAD` points at (`main` for `refs/heads/main`); `None`
    /// for a detached `HEAD`.
    ///
    /// # Errors
    ///
    /// A git error.
    pub fn head_branch(&self, git_dir: &Path) -> Result<Option<String>> {
        let output = self.run_ok(
            "rev-parse",
            self.invocation(work_dir(git_dir), Some(git_dir)).args([
                "rev-parse",
                "--symbolic-full-name",
                "HEAD",
            ]),
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim()
            .strip_prefix("refs/heads/")
            .filter(|branch| validate_ref_syntax(branch).is_ok())
            .map(str::to_owned))
    }

    /// Lists the tree `<commit>:<dir>` non-recursively, with the size of
    /// every blob (`--long`, so no blob needs a call of its own to be
    /// sized); an empty `dir` is the root tree.
    ///
    /// # Errors
    ///
    /// A git error, also for output that is not an `ls-tree -z --long`
    /// listing.
    pub fn ls_tree(&self, git_dir: &Path, commit: &CommitId, dir: &str) -> Result<Vec<TreeEntry>> {
        let output = self.run_ok(
            "ls-tree",
            self.invocation(work_dir(git_dir), Some(git_dir))
                .args(["ls-tree", "-z", "--long", "--end-of-options"])
                .arg(format!("{commit}:{dir}")),
        )?;
        if output.truncated {
            return Err(Error::GitFailed {
                operation: "ls-tree",
                status: "listing too large".to_owned(),
            });
        }
        parse_ls_tree(&output.stdout).ok_or(Error::GitFailed {
            operation: "ls-tree",
            status: "unexpected output".to_owned(),
        })
    }

    /// Reads a blob of at most `max` bytes; `None` when it is larger.
    ///
    /// # Errors
    ///
    /// A git error.
    pub fn read_blob(&self, git_dir: &Path, oid: &str, max: usize) -> Result<Option<Vec<u8>>> {
        let oid = checked_oid(oid, "cat-file")?;
        let output = self.run(
            "cat-file",
            self.invocation(work_dir(git_dir), Some(git_dir))
                .args(["cat-file", "blob", oid]),
            max.saturating_add(1),
        )?;
        if !output.success() {
            return Err(failure("cat-file", &output));
        }
        Ok((!output.truncated && output.stdout.len() <= max).then_some(output.stdout))
    }

    /// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
    ///
    /// # Errors
    ///
    /// A git error.
    pub fn is_ancestor(
        &self,
        git_dir: &Path,
        ancestor: &CommitId,
        descendant: &CommitId,
    ) -> Result<bool> {
        let output = self.run(
            "merge-base",
            self.invocation(work_dir(git_dir), Some(git_dir))
                .args(["merge-base", "--is-ancestor", "--"])
                .args([ancestor.as_str(), descendant.as_str()]),
            DEFAULT_MAX_OUTPUT,
        )?;
        match output.exit {
            Exit::Code(0) => Ok(true),
            Exit::Code(1) => Ok(false),
            _ => Err(failure("merge-base", &output)),
        }
    }

    /// Points [`PIN_REF`] at `commit`, so it stays reachable.
    ///
    /// # Errors
    ///
    /// A git error.
    pub fn update_pin(&self, git_dir: &Path, commit: &CommitId) -> Result<()> {
        self.run_ok(
            "update-ref",
            self.invocation(work_dir(git_dir), Some(git_dir)).args([
                "update-ref",
                "--no-deref",
                PIN_REF,
                commit.as_str(),
            ]),
        )
        .map(|_| ())
    }
}

/// The working directory of a call on a repository: the directory that
/// holds its git directory.
fn work_dir(git_dir: &Path) -> &Path {
    git_dir.parent().unwrap_or(git_dir)
}

fn checked_oid<'a>(oid: &'a str, operation: &'static str) -> Result<&'a str> {
    if is_object_id(oid) {
        Ok(oid)
    } else {
        Err(Error::GitFailed {
            operation,
            status: "invalid object id".to_owned(),
        })
    }
}

/// Maps a failed call to its error code; git's output is never part of it.
fn failure(operation: &'static str, output: &Output) -> Error {
    match output.stderr_class {
        Some(StderrClass::AuthFailed) => Error::GitAuthFailed { operation },
        Some(StderrClass::HostUnknown) => Error::GitHostUnknown { operation },
        Some(StderrClass::Unreachable) => Error::GitUnreachable { operation },
        Some(StderrClass::RefNotFound) => Error::GitRefNotFound { operation },
        Some(StderrClass::Failed) | None => Error::GitFailed {
            operation,
            status: match output.exit {
                Exit::Code(code) => format!("exit status {code}"),
                Exit::Signal(signal) => format!("signal {signal}"),
                Exit::Unknown => "unknown status".to_owned(),
            },
        },
    }
}

/// Parses `ls-tree -z --long` output:
/// `<mode> SP <type> SP <oid> SP+ <size> TAB <name> NUL`, where the size is
/// right-aligned with spaces and `-` for a tree or a gitlink.
fn parse_ls_tree(bytes: &[u8]) -> Option<Vec<TreeEntry>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let tab = record.iter().position(|byte| *byte == b'\t')?;
            let (meta, name) = (&record[..tab], &record[tab + 1..]);
            let meta = std::str::from_utf8(meta).ok()?;
            let mut parts = meta.split(' ').filter(|part| !part.is_empty());
            let (mode, kind, oid, size) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            if parts.next().is_some() || !is_object_id(oid) || name.is_empty() {
                return None;
            }
            let size = match (kind, size) {
                ("blob", size) => Some(size.parse().ok()?),
                (_, "-") => None,
                _ => return None,
            };
            Some(TreeEntry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                oid: oid.to_owned(),
                size,
                name: name.to_vec(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
