//! Writing through the `claude` CLI, one operation at a time.
//!
//! - Add: `claude mcp add-json -s user [--client-secret] <name> <json>`.
//! - Remove: `claude mcp remove -s user <name>`, after a drift check.
//! - Update: remove, then add. When the add fails, an entry d-mcp wrote is
//!   re-added from memory (rollback); otherwise it is reported as lost.
//!
//! Every call runs without a shell and without a TTY, from a neutral working
//! directory (the store root, or a fresh temporary directory, never a
//! directory with a `.mcp.json` or a project entry of `.claude.json` at or
//! above it), with `PATH` from the environment d-mcp was given, an explicit
//! `HOME`, `CLAUDE_CONFIG_DIR` set (empty when d-mcp's own is set but empty)
//! or removed, `DISABLE_AUTOUPDATER=1`, and
//! `MCP_CLIENT_SECRET` removed unless d-mcp sets it for `--client-secret`.
//! Each intent is journaled right before its call. Child output is never
//! printed.
//!
//! With an empty `CLAUDE_CONFIG_DIR`, Claude Code 2.1.310 resolves its
//! configuration home relative to its working directory: it would read a
//! legacy `<cwd>/.config.json` and writes a full backup of the configuration
//! file to `<cwd>/backups/` on every write. In that mode the child therefore
//! always runs from a fresh temporary directory, never the store root, and
//! the directory is removed with its `backups/` afterwards. Otherwise the
//! backups go to `$CLAUDE_CONFIG_DIR/backups/` or `$HOME/.claude/backups/`,
//! outside the working directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};
use tempfile::TempDir;

use super::file::{self, Parsed, entry_hash};
use crate::adapter::{
    ApplyContext, ApplyResult, EntryOutcome, EntryResult, Gap, GapSeverity, HarnessPlan,
    HarnessRead, Hooks, Intent, Journal, LoginState, WriteKind, WriteOp,
};
use crate::error::{Error, Result};
use crate::process::{Exit, Invocation, ProcessRunner, RunError, find_program};
use crate::store::Environment;

/// The program name.
pub const PROGRAM: &str = "claude";
/// The harness name, for errors.
const HARNESS: &str = "claude";
/// The timeout of a CLI write. A timeout means "unknown": the file is
/// re-read and the pending record settled; nothing is rolled back blindly.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// A configured `claude` CLI.
pub(super) struct Cli {
    program: PathBuf,
    path: Option<OsString>,
    runner: ProcessRunner,
    cwd: PathBuf,
    home: PathBuf,
    /// The child's `CLAUDE_CONFIG_DIR`: the resolved directory, an empty
    /// value when d-mcp's own is set but empty, or `None` to remove it.
    config_dir: Option<OsString>,
    /// Keeps a temporary neutral directory alive.
    _temp: Option<TempDir>,
}

/// Whether a directory is neutral for Claude Code: no `.mcp.json` at or
/// above it, and no project entry of `.claude.json` at or above it. With
/// `cwd_config_home` (an empty `CLAUDE_CONFIG_DIR`, which makes the
/// directory Claude Code's configuration home), it must also hold no legacy
/// `.config.json`, which Claude Code would use instead of the file d-mcp
/// reads.
fn is_neutral(dir: &Path, projects: &BTreeSet<String>, cwd_config_home: bool) -> bool {
    dir.is_absolute()
        && dir.is_dir()
        && !(cwd_config_home && std::fs::symlink_metadata(dir.join(".config.json")).is_ok())
        && dir.ancestors().all(|ancestor| {
            std::fs::symlink_metadata(ancestor.join(".mcp.json")).is_err()
                && !projects.contains(ancestor.to_string_lossy().as_ref())
        })
}

fn describe(exit: Exit) -> String {
    match exit {
        Exit::Code(code) => format!("exit status {code}"),
        Exit::Signal(signal) => format!("terminated by signal {signal}"),
        Exit::Unknown => "unknown exit status".to_owned(),
    }
}

impl Cli {
    /// Finds `claude` and a neutral working directory: `preferred` when it is
    /// neutral, otherwise a fresh temporary directory. With an empty
    /// `CLAUDE_CONFIG_DIR` it is always a fresh temporary directory, since
    /// Claude Code then writes `backups/` into its working directory.
    ///
    /// # Errors
    ///
    /// [`Error::HarnessCliNotFound`] without a `claude` on `PATH`,
    /// [`Error::HomeUnset`], and [`Error::HarnessNoNeutralDir`] when no
    /// neutral working directory exists.
    pub(super) fn new(
        env: &Environment,
        runner: ProcessRunner,
        preferred: Option<&Path>,
        projects: &BTreeSet<String>,
    ) -> Result<Self> {
        let program = find_program(env.path.as_deref(), PROGRAM.as_ref()).ok_or(
            Error::HarnessCliNotFound {
                harness: HARNESS,
                program: PROGRAM,
            },
        )?;
        let home = env.home()?;
        let cwd_config_home = env.claude_config_dir_empty();
        let config_dir = match env.claude_config_dir()? {
            Some(dir) => Some(dir.into_os_string()),
            None if cwd_config_home => Some(OsString::new()),
            None => None,
        };
        let mut temp = None;
        let preferred =
            preferred.filter(|dir| !cwd_config_home && is_neutral(dir, projects, false));
        let cwd = match preferred {
            Some(dir) => dir.to_path_buf(),
            None => {
                let dir = tempfile::Builder::new()
                    .prefix("d-mcp-")
                    .tempdir()
                    .map_err(|error| Error::io("create", std::env::temp_dir(), &error))?;
                let path = dir.path().to_path_buf();
                temp = Some(dir);
                if !is_neutral(&path, projects, cwd_config_home) {
                    return Err(Error::HarnessNoNeutralDir {
                        harness: HARNESS,
                        program: PROGRAM,
                        path,
                    });
                }
                path
            }
        };
        Ok(Self {
            program,
            path: env.path.clone(),
            runner,
            cwd,
            home,
            config_dir,
            _temp: temp,
        })
    }

    /// Runs `claude mcp <subcommand> <args>`. The secret, if any, goes into
    /// `MCP_CLIENT_SECRET` of the child environment only.
    fn call(
        &self,
        subcommand: &'static str,
        args: &[&str],
        secret: Option<&OsString>,
    ) -> Result<crate::process::Output> {
        let mut invocation = Invocation::new(&self.program)
            .args(["mcp", subcommand])
            .args(args.iter().copied())
            .current_dir(&self.cwd)
            .env("HOME", &self.home)
            .env("DISABLE_AUTOUPDATER", "1")
            .env_remove("MCP_CLIENT_SECRET");
        invocation = match &self.config_dir {
            Some(dir) => invocation.env("CLAUDE_CONFIG_DIR", dir),
            None => invocation.env_remove("CLAUDE_CONFIG_DIR"),
        };
        if let Some(secret) = secret {
            invocation = invocation.env("MCP_CLIENT_SECRET", secret);
        }
        self.runner
            .run(&invocation, self.path.as_deref())
            .map_err(|error| match error {
                RunError::Timeout { .. } => Error::HarnessCliTimeout {
                    program: PROGRAM,
                    subcommand,
                },
                RunError::NotFound { .. } => Error::HarnessCliNotFound {
                    harness: HARNESS,
                    program: PROGRAM,
                },
                RunError::Spawn { .. } => Error::HarnessCliFailed {
                    program: PROGRAM,
                    subcommand,
                    status: "could not be started".to_owned(),
                },
            })
    }

    /// Runs a write subcommand and requires exit status 0.
    fn write(
        &self,
        subcommand: &'static str,
        args: &[&str],
        secret: Option<&OsString>,
    ) -> Result<()> {
        let output = self.call(subcommand, args, secret)?;
        if output.success() {
            Ok(())
        } else {
            Err(Error::HarnessCliFailed {
                program: PROGRAM,
                subcommand,
                status: describe(output.exit),
            })
        }
    }

    fn add(&self, name: &str, entry: &Value, secret: Option<&OsString>) -> Result<()> {
        let json = entry.to_string();
        let mut args = vec!["-s", "user"];
        if secret.is_some() {
            args.push("--client-secret");
        }
        args.extend([name, json.as_str()]);
        self.write("add-json", &args, secret)
    }

    fn remove(&self, name: &str) -> Result<()> {
        self.write("remove", &["-s", "user", name], None)
    }
}

/// The rendered entry as one JSON object.
pub(super) fn entry_object(entry: &[(String, Value)]) -> Value {
    Value::Object(entry.iter().cloned().collect::<Map<String, Value>>())
}

/// Whether an entry can hold OAuth state in Claude Code: a remote entry with
/// an `oauth` object, or without static headers.
pub(super) fn uses_oauth(entry: &Value) -> bool {
    let remote = matches!(
        entry.get("type").and_then(Value::as_str),
        Some("http" | "sse")
    );
    let headers = entry
        .get("headers")
        .and_then(Value::as_object)
        .is_some_and(|headers| !headers.is_empty());
    remote && (entry.get("oauth").is_some() || !headers)
}

fn note(field: &str, reason: String) -> Gap {
    Gap {
        field: field.to_owned(),
        severity: GapSeverity::Note,
        reason,
        unless_present: None,
    }
}

struct Writer<'a> {
    cli: Cli,
    file: &'a Path,
    read: &'a HarnessRead,
    ctx: &'a ApplyContext<'a>,
    hooks: &'a Hooks<'a>,
}

impl Writer<'_> {
    /// The client secret for a rendering, and the manual-step gap for an
    /// entry that is added without it because its variable is not set. The
    /// gap applies only when the new entry is in place.
    fn secret(&self, op: &WriteOp) -> (Option<OsString>, Option<Gap>) {
        let Some(var) = op
            .rendered
            .as_ref()
            .and_then(|rendered| rendered.secret_env.as_ref())
        else {
            return (None, None);
        };
        let value = (self.ctx.var)(var).filter(|value| !value.is_empty());
        let manual = value.is_none().then(|| {
            note(
                "auth.client_secret_env",
                format!(
                    "manual step: `{var}` was not set, so the entry was added without a client \
                     secret; set it, run `claude mcp remove -s user {}`, then `d-mcp sync` re-adds \
                     the entry with the secret",
                    op.name
                ),
            )
        });
        (value, manual)
    }

    /// The note for a rolled-back entry with an `oauth` object that was
    /// re-added without a client secret: the removal cleared the stored one.
    fn restored_without_secret(op: &WriteOp) -> Gap {
        let var = op
            .rendered
            .as_ref()
            .and_then(|rendered| rendered.secret_env.as_deref())
            .map_or_else(
                || "the variable named by `client_secret_env`".to_owned(),
                |var| format!("`{var}`"),
            );
        note(
            "auth.client_secret_env",
            format!(
                "client secret needed; the restored entry has none: the removal cleared Claude \
                 Code's stored client secret for `{}`. If the server needs one, set {var}; the \
                 next `d-mcp sync` replaces the entry and passes it",
                op.name
            ),
        )
    }

    /// The entry as it is now, re-read from the file.
    fn current(&self, name: &str) -> Result<Option<Value>> {
        file::read(self.file).map(|mut parsed| parsed.entries.remove(name))
    }

    /// Verifies an add: the entry reads back with the sent hash.
    fn verify_added(&self, op: &WriteOp, sent: &str) -> EntryResult {
        match self.current(&op.name) {
            Ok(Some(entry)) if entry_hash(&entry) == sent && !self.hooks.force_verify_failure => {
                EntryResult::applied(&op.name, Some(sent.to_owned()))
            }
            Ok(_) => EntryResult::other(
                &op.name,
                EntryOutcome::Failed(Error::HarnessVerifyFailed {
                    path: self.file.to_path_buf(),
                    restored: None,
                }),
            ),
            Err(error) => EntryResult::other(&op.name, EntryOutcome::Failed(error)),
        }
    }

    /// The drift check right before a removal: the entry must still have the
    /// hash it was planned from. Returns the entry as it is now.
    fn recheck(&self, op: &WriteOp) -> std::result::Result<Option<Value>, Box<EntryResult>> {
        let planned = self
            .read
            .entries
            .get(&op.name)
            .map(|entry| entry.hash.as_str());
        match self.current(&op.name) {
            Ok(Some(entry)) if Some(entry_hash(&entry).as_str()) == planned => Ok(Some(entry)),
            Ok(None) => Ok(None),
            Ok(Some(_)) => Err(Box::new(EntryResult::other(&op.name, EntryOutcome::Drift))),
            Err(error) => Err(Box::new(EntryResult::other(
                &op.name,
                EntryOutcome::Failed(error),
            ))),
        }
    }

    fn intend(&self, journal: &mut dyn Journal, op: &WriteOp) -> Result<()> {
        journal.intend(&[Intent {
            name: op.name.clone(),
            kind: op.kind,
            expected_readback: op
                .rendered
                .as_ref()
                .map(|rendered| rendered.sent_hash.clone()),
        }])
    }

    fn add(&self, op: &WriteOp, journal: &mut dyn Journal) -> Result<EntryResult> {
        let Some(rendered) = &op.rendered else {
            return Ok(EntryResult::other(
                &op.name,
                EntryOutcome::Failed(Error::HarnessEditFailed {
                    path: self.file.to_path_buf(),
                }),
            ));
        };
        let (secret, manual) = self.secret(op);
        self.intend(journal, op)?;
        let entry = entry_object(&rendered.entry);
        let mut result = match self.cli.add(&op.name, &entry, secret.as_ref()) {
            Ok(()) => self.verify_added(op, &rendered.sent_hash),
            // A timeout is unknown: the write may have taken effect.
            Err(error @ Error::HarnessCliTimeout { .. }) => {
                match self.verify_added(op, &rendered.sent_hash) {
                    applied @ EntryResult {
                        outcome: EntryOutcome::Applied,
                        ..
                    } => applied,
                    _ => EntryResult::other(&op.name, EntryOutcome::Failed(error)),
                }
            }
            Err(error) => EntryResult::other(&op.name, EntryOutcome::Failed(error)),
        };
        if matches!(result.outcome, EntryOutcome::Applied) {
            result.gaps.extend(manual);
        }
        Ok(result)
    }

    /// Removes an entry and verifies that it is gone. A timeout counts as
    /// done when the entry is gone.
    fn remove_verified(&self, name: &str) -> std::result::Result<(), Error> {
        let outcome = self.cli.remove(name);
        match (outcome, self.current(name)) {
            (Ok(()) | Err(Error::HarnessCliTimeout { .. }), Ok(None)) => Ok(()),
            (Err(error), _) => Err(error),
            (Ok(()), Ok(Some(_))) => Err(Error::HarnessVerifyFailed {
                path: self.file.to_path_buf(),
                restored: None,
            }),
            (Ok(()), Err(error)) => Err(error),
        }
    }

    fn remove(&self, op: &WriteOp, journal: &mut dyn Journal) -> Result<EntryResult> {
        match self.recheck(op) {
            Err(result) => return Ok(*result),
            // Already gone: nothing to call.
            Ok(None) => return Ok(EntryResult::applied(&op.name, None)),
            Ok(Some(_)) => {}
        }
        self.intend(journal, op)?;
        Ok(match self.remove_verified(&op.name) {
            Ok(()) => EntryResult::applied(&op.name, None),
            Err(error) => EntryResult::other(&op.name, EntryOutcome::Failed(error)),
        })
    }

    fn update(&self, op: &WriteOp, journal: &mut dyn Journal) -> Result<EntryResult> {
        let Some(rendered) = &op.rendered else {
            return self.add(op, journal);
        };
        let old = match self.recheck(op) {
            Err(result) => return Ok(*result),
            Ok(old) => old,
        };
        // The re-login note applies whatever the outcome once the old entry
        // is removed; the manual step only once the new entry is in place.
        let mut gaps = Vec::new();
        let (secret, manual) = self.secret(op);
        self.intend(journal, op)?;
        if let Some(old) = &old {
            if let Err(error) = self.remove_verified(&op.name) {
                return Ok(EntryResult::other(&op.name, EntryOutcome::Failed(error)));
            }
            if uses_oauth(old) {
                gaps.push(note(
                    "auth",
                    format!(
                        "re-login needed: replacing the entry cleared Claude Code's stored OAuth \
                         tokens and client secret; run `claude mcp login {}` or use `/mcp`",
                        op.name
                    ),
                ));
            }
        }
        if let Some(hook) = self.hooks.after_remove {
            hook()?;
        }
        let entry = entry_object(&rendered.entry);
        let error = match self.cli.add(&op.name, &entry, secret.as_ref()) {
            Ok(()) => {
                let mut result = self.verify_added(op, &rendered.sent_hash);
                if matches!(result.outcome, EntryOutcome::Applied) {
                    gaps.extend(manual);
                    result.gaps = gaps;
                }
                return Ok(result);
            }
            Err(error @ Error::HarnessCliTimeout { .. }) => {
                // Unknown: settle from the file, never roll back blindly.
                return Ok(match self.current(&op.name) {
                    Ok(Some(now)) if entry_hash(&now) == rendered.sent_hash => {
                        let mut result =
                            EntryResult::applied(&op.name, Some(rendered.sent_hash.clone()));
                        gaps.extend(manual);
                        result.gaps = gaps;
                        result
                    }
                    Ok(Some(_)) | Err(_) => {
                        EntryResult::other(&op.name, EntryOutcome::Failed(error))
                    }
                    Ok(None) => EntryResult::other(
                        &op.name,
                        EntryOutcome::Lost {
                            error,
                            fields: fields(old.as_ref()),
                        },
                    ),
                });
            }
            Err(error) => error,
        };
        let Some(old) = old else {
            return Ok(EntryResult::other(&op.name, EntryOutcome::Failed(error)));
        };
        if op.rollback {
            // Only d-mcp's own rendering is re-added, so no foreign content
            // reaches the command line. The secret is the one of the new
            // rendering, when its variable is set.
            let planned = self
                .read
                .entries
                .get(&op.name)
                .map(|entry| entry.hash.clone());
            let restored = self.cli.add(&op.name, &old, secret.as_ref()).is_ok()
                && self
                    .current(&op.name)
                    .ok()
                    .flatten()
                    .is_some_and(|now| Some(entry_hash(&now)) == planned);
            if restored {
                // The old entry is back, not the new one: no manual step for
                // the new entry, but a note when the restored OAuth entry
                // went without a client secret.
                if old.get("oauth").is_some() && secret.is_none() {
                    gaps.push(Self::restored_without_secret(op));
                }
                let mut result = EntryResult::other(&op.name, EntryOutcome::RolledBack(error));
                result.gaps = gaps;
                return Ok(result);
            }
        }
        Ok(EntryResult::other(
            &op.name,
            EntryOutcome::Lost {
                error,
                fields: fields(Some(&old)),
            },
        ))
    }
}

/// The names of an entry's fields.
fn fields(entry: Option<&Value>) -> Vec<String> {
    entry
        .and_then(Value::as_object)
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

/// Applies a plan one operation at a time.
///
/// # Errors
///
/// [`Error::HarnessCliNotFound`] before anything is journaled; journal and
/// environment errors; an error of the `after_remove` seam.
pub(super) fn apply(
    ctx: &ApplyContext<'_>,
    read: &HarnessRead,
    plan: &HarnessPlan,
    journal: &mut dyn Journal,
    hooks: &Hooks<'_>,
) -> Result<ApplyResult> {
    let file = read.location.file.as_path();
    let runner = ProcessRunner {
        timeout: hooks.cli_timeout.unwrap_or(WRITE_TIMEOUT),
        ..*ctx.runner
    };
    let projects = match file::read(file) {
        Ok(parsed) => parsed.projects,
        Err(_) => BTreeSet::new(),
    };
    let cli = Cli::new(ctx.env, runner, Some(ctx.neutral_dir), &projects)?;
    let writer = Writer {
        cli,
        file,
        read,
        ctx,
        hooks,
    };
    let mut result = ApplyResult::default();
    for op in &plan.ops {
        let entry = match op.kind {
            WriteKind::Add => writer.add(op, journal)?,
            WriteKind::Update => writer.update(op, journal)?,
            WriteKind::Remove => writer.remove(op, journal)?,
        };
        result.entries.push(entry);
    }
    result.created = !read.location.file_exists && std::fs::symlink_metadata(file).is_ok();
    Ok(result)
}

/// Maps the `Status:` line of `claude mcp get` (verified against 2.1.310).
pub(super) fn login_state_of(stdout: &[u8]) -> LoginState {
    let text = String::from_utf8_lossy(stdout);
    let status = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Status:"))
        .map(str::trim);
    match status {
        Some(status) if status.starts_with("\u{2714} Connected") => LoginState::LoggedIn,
        Some(status) if status.starts_with("! Needs authentication") => LoginState::NeedsLogin,
        _ => LoginState::Unknown,
    }
}

/// Reads the login state of managed entries: one `claude mcp get <name>`
/// per entry that can hold OAuth state, from a neutral working directory;
/// every other entry is `not-applicable` without a call, and an entry that
/// is absent, a missing binary or any failure is `unknown`.
pub(super) fn login_state(
    env: &Environment,
    runner: &ProcessRunner,
    names: &[String],
) -> BTreeMap<String, LoginState> {
    let parsed: Option<Parsed> = env
        .claude_config_file()
        .ok()
        .and_then(|path| file::read(&path).ok());
    let Some(parsed) = parsed else {
        return names
            .iter()
            .map(|name| (name.clone(), LoginState::Unknown))
            .collect();
    };
    let store = env.store_root(None).ok();
    let mut cli: Option<Option<Cli>> = None;
    names
        .iter()
        .map(|name| {
            let state = match parsed.entries.get(name) {
                None => LoginState::Unknown,
                Some(entry) if !uses_oauth(entry) => LoginState::NotApplicable,
                Some(_) => {
                    let cli = cli.get_or_insert_with(|| {
                        Cli::new(env, *runner, store.as_deref(), &parsed.projects).ok()
                    });
                    match cli.as_ref().map(|cli| cli.call("get", &[name], None)) {
                        Some(Ok(output)) if output.success() && !output.truncated => {
                            login_state_of(&output.stdout)
                        }
                        _ => LoginState::Unknown,
                    }
                }
            };
            (name.clone(), state)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONNECTED: &[u8] = include_bytes!("../../../tests/fixtures/claude/get/connected.txt");
    const NEEDS: &[u8] =
        include_bytes!("../../../tests/fixtures/claude/get/needs-authentication.txt");
    const FAILED: &[u8] = include_bytes!("../../../tests/fixtures/claude/get/failed.txt");

    #[test]
    fn recorded_get_outputs_map_to_login_states() {
        assert_eq!(login_state_of(CONNECTED), LoginState::LoggedIn);
        assert_eq!(login_state_of(NEEDS), LoginState::NeedsLogin);
        assert_eq!(login_state_of(FAILED), LoginState::Unknown);
        assert_eq!(login_state_of(b"no status"), LoginState::Unknown);
        assert_eq!(describe(Exit::Code(2)), "exit status 2");
        assert_eq!(describe(Exit::Signal(9)), "terminated by signal 9");
        assert_eq!(describe(Exit::Unknown), "unknown exit status");
    }

    #[test]
    fn oauth_capable_entries() {
        assert!(uses_oauth(&json!({"type": "http", "url": "u"})));
        assert!(uses_oauth(
            &json!({"type": "http", "url": "u", "headers": {}})
        ));
        assert!(uses_oauth(
            &json!({"type": "sse", "url": "u", "oauth": {"clientId": "c"}})
        ));
        assert!(!uses_oauth(
            &json!({"type": "http", "url": "u", "headers": {"A": "b"}})
        ));
        assert!(!uses_oauth(&json!({"command": "x"})));
        assert_eq!(fields(Some(&json!({"b": 1, "a": 2}))), ["a", "b"]);
        assert!(fields(None).is_empty());
    }

    #[test]
    fn neutral_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = root.join("store");
        std::fs::create_dir_all(&store).unwrap();
        assert!(is_neutral(&store, &BTreeSet::new(), false));
        assert!(is_neutral(&store, &BTreeSet::new(), true));
        assert!(!is_neutral(Path::new("relative"), &BTreeSet::new(), false));
        assert!(!is_neutral(&root.join("missing"), &BTreeSet::new(), false));
        // A legacy `.config.json` matters only when the directory is
        // Claude Code's configuration home (an empty `CLAUDE_CONFIG_DIR`).
        std::fs::write(store.join(".config.json"), "{}").unwrap();
        assert!(is_neutral(&store, &BTreeSet::new(), false));
        assert!(!is_neutral(&store, &BTreeSet::new(), true));
        let projects = BTreeSet::from([root.to_string_lossy().into_owned()]);
        assert!(!is_neutral(&store, &projects, false));
        std::fs::write(root.join(".mcp.json"), "{}").unwrap();
        assert!(!is_neutral(&store, &BTreeSet::new(), false));
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_config_dir_never_runs_from_the_preferred_dir() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let store = root.join("store");
        let bin = root.join("bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join(PROGRAM);
        std::fs::write(&program, "").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = Environment {
            home: Some(root.join("home").into_os_string()),
            store: None,
            cwd: Some(root.clone()),
            codex_home: None,
            path: Some(bin.into_os_string()),
            claude_config_dir: None,
        };
        let runner = ProcessRunner::default();
        let projects = BTreeSet::new();
        let cli = Cli::new(&env, runner, Some(&store), &projects).unwrap();
        assert_eq!(cli.cwd, store);
        env.claude_config_dir = Some(OsString::new());
        let cli = Cli::new(&env, runner, Some(&store), &projects).unwrap();
        assert_ne!(cli.cwd, store);
        assert!(cli.cwd.is_dir());
        assert!(cli._temp.is_some());
        let cwd = cli.cwd.clone();
        drop(cli);
        assert!(!cwd.exists(), "the temporary directory is removed");
    }
}
