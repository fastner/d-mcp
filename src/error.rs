//! The error enum, its exit codes and its stable error codes.
//!
//! Every variant maps to exactly one documented exit code and one stable
//! error code (see `docs/reference.md`). Messages name paths, ids, rule codes
//! and positions only; they never contain a definition or config value.
//!
//! | Exit | Meaning |
//! |---:|---|
//! | 0 | Success, including resolution problems, holds and warnings |
//! | 1 | Expected failure |
//! | 2 | Usage error (clap) |
//! | 3 | Unsafe state |
//! | 4 | Environment problem |

use std::io;
use std::path::PathBuf;

use crate::rule::Rule;

/// Exit code for an expected failure.
pub const EXIT_FAILURE: u8 = 1;
/// Exit code for a clap usage error.
pub const EXIT_USAGE: u8 = 2;
/// Exit code for an unsafe store state.
pub const EXIT_UNSAFE: u8 = 3;
/// Exit code for an environment problem.
pub const EXIT_ENVIRONMENT: u8 = 4;

/// Result alias for d-mcp operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A d-mcp error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The store directory has no `config.toml`.
    #[error("the store at {} is not initialized; run `d-mcp init`", path.display())]
    StoreNotInitialized {
        /// The store root.
        path: PathBuf,
    },
    /// The store path exists but is not a directory.
    #[error("the store path {} exists but is not a directory", path.display())]
    StorePathInvalid {
        /// The store root.
        path: PathBuf,
    },
    /// `init` was pointed at a non-empty directory that is not a store.
    #[error("{} is not empty and is not a d-mcp store; refusing to initialize it", path.display())]
    ForeignDirectory {
        /// The directory.
        path: PathBuf,
    },
    /// A name argument breaks the neutral name rule.
    #[error(
        "the {what} is not a valid name: use lowercase letters, digits, `-` and `_`, start with a \
         letter or digit, do not end with `-` or `_`, avoid `__`, at most 32 characters"
    )]
    InvalidName {
        /// What was named: `source id` or `server name`.
        what: &'static str,
    },
    /// The id `local` belongs to the built-in source.
    #[error(
        "the source id `local` is reserved for the built-in local source, which cannot be added, removed or re-prioritized"
    )]
    SourceIdReserved,
    /// The source id is already registered.
    #[error("a source with id `{id}` already exists")]
    SourceIdTaken {
        /// The id.
        id: String,
    },
    /// The priority is 0.
    #[error("the priority must be at least 1 (0 belongs to the built-in local source)")]
    PriorityInvalid,
    /// The priority is used by another source.
    #[error("priority {priority} is already used by source `{id}`")]
    PriorityTaken {
        /// The priority.
        priority: u32,
        /// The source using it.
        id: String,
    },
    /// The source path does not exist or cannot be resolved.
    #[error("the source path {} does not exist", path.display())]
    SourcePathNotFound {
        /// The path as given, after `~` expansion.
        path: PathBuf,
    },
    /// The source path is not a directory.
    #[error("the source path {} is not a directory", path.display())]
    SourcePathNotDirectory {
        /// The canonical path.
        path: PathBuf,
    },
    /// The source path is not valid UTF-8.
    #[error("the source path is not valid UTF-8")]
    SourcePathNotUtf8,
    /// The path is already registered under another id.
    #[error("the source path {} is already registered as `{id}`", path.display())]
    SourcePathRegistered {
        /// The canonical path.
        path: PathBuf,
        /// The registered id.
        id: String,
    },
    /// The path lies inside the store.
    #[error("the source path {} lies inside the store; use the built-in `local` source instead", path.display())]
    SourcePathInStore {
        /// The canonical path.
        path: PathBuf,
    },
    /// No source with this id exists.
    #[error("no source with id `{id}` exists")]
    SourceNotFound {
        /// The id.
        id: String,
    },
    /// `unselect` of a server that is not selected.
    #[error("`{server}` is not selected")]
    NotSelected {
        /// The server name.
        server: String,
    },
    /// `config.toml` breaks a rule. It is never rewritten.
    #[error(
        "{} is corrupt at line {line}, column {column}{} (rule `{rule}`); fix it by hand, d-mcp will not rewrite it",
        path.display(),
        if field.is_empty() { String::new() } else { format!(", field `{field}`") }
    )]
    ConfigCorrupt {
        /// The file.
        path: PathBuf,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
        /// The field path.
        field: String,
        /// The broken rule.
        rule: Rule,
    },
    /// A persisted file declares an unsupported schema version.
    #[error("{} has schema_version {found}, but this d-mcp supports only version 1", path.display())]
    SchemaUnsupported {
        /// The file.
        path: PathBuf,
        /// The declared version.
        found: i64,
    },
    /// Another process holds the store lock.
    #[error("the store is locked by another d-mcp process ({}); try again later", path.display())]
    StoreLocked {
        /// The lock file.
        path: PathBuf,
    },
    /// `HOME` is unset or empty, but it is needed.
    #[error("HOME is unset or empty; set HOME, D_MCP_STORE or pass --store")]
    HomeUnset,
    /// The current directory cannot be determined.
    #[error("the current directory cannot be determined")]
    CurrentDirUnavailable,
    /// A harness configuration file changed between d-mcp's read and its
    /// persist, or its symlink was retargeted. Nothing was written.
    #[error(
        "{} changed while d-mcp was editing it; nothing was written, run the command again",
        path.display()
    )]
    HarnessFileChanged {
        /// The harness file, as resolved before following symlinks.
        path: PathBuf,
    },
    /// A harness configuration file is not valid TOML, not UTF-8 or not a
    /// regular file. It is reported by position only.
    #[error(
        "{} is {reason}{}; fix it by hand, d-mcp will not edit it",
        path.display(),
        position.map_or_else(String::new, |(line, column)| format!(" at line {line}, column {column}"))
    )]
    HarnessConfigInvalid {
        /// The harness file.
        path: PathBuf,
        /// What is wrong: `not valid TOML`, `not UTF-8` or `not a regular file`.
        reason: &'static str,
        /// 1-based line and column, for a syntax error.
        position: Option<(usize, usize)>,
    },
    /// d-mcp could not produce a valid edit of a harness file: the spliced
    /// text did not parse, or it changed something outside the plan. Nothing
    /// was written.
    #[error(
        "d-mcp could not produce a valid edit of {}; nothing was written, please report this",
        path.display()
    )]
    HarnessEditFailed {
        /// The harness file.
        path: PathBuf,
    },
    /// A harness configuration path is not valid UTF-8, so the write state
    /// cannot record it.
    #[error("the {harness} configuration path is not valid UTF-8; d-mcp cannot record it")]
    HarnessPathNotUtf8 {
        /// The harness.
        harness: &'static str,
    },
    /// A harness file did not read back as written.
    #[error(
        "{} did not read back as written; {}",
        path.display(),
        match restored {
            Some(true) => "the original content was restored",
            Some(false) => "restoring the original content FAILED, check the file by hand",
            None => "the entry is left as the harness wrote it and is not taken as d-mcp's write (a new entry stays unowned; a replaced one is reported as drift on the next sync)",
        }
    )]
    HarnessVerifyFailed {
        /// The harness file.
        path: PathBuf,
        /// Whether the original bytes were restored; `None` when the harness
        /// wrote the file through its own CLI and nothing was restored.
        restored: Option<bool>,
    },
    /// A harness CLI that d-mcp writes through is not on `PATH`.
    #[error(
        "the `{program}` CLI was not found on PATH; d-mcp writes {harness} entries through it, so nothing was written for {harness}"
    )]
    HarnessCliNotFound {
        /// The harness.
        harness: &'static str,
        /// The program name.
        program: &'static str,
    },
    /// A harness CLI call exited unsuccessfully. Its output is never shown.
    #[error(
        "`{program} mcp {subcommand}` failed ({status}); its output is not shown because it can contain secrets"
    )]
    HarnessCliFailed {
        /// The program name.
        program: &'static str,
        /// The subcommand, such as `add-json`.
        subcommand: &'static str,
        /// The exit status, such as `exit status 1`.
        status: String,
    },
    /// No neutral working directory exists for a harness CLI: even a fresh
    /// temporary directory has a `.mcp.json` or a project entry of the
    /// harness configuration at or above it. Nothing was run.
    #[error(
        "no neutral working directory for `{program}`: {} has a `.mcp.json` or a `projects` entry of the {harness} configuration at or above it, so nothing was run for {harness}; remove it or set TMPDIR to another directory",
        path.display()
    )]
    HarnessNoNeutralDir {
        /// The harness.
        harness: &'static str,
        /// The program name.
        program: &'static str,
        /// The temporary directory that was tried.
        path: PathBuf,
    },
    /// A harness CLI call did not finish in time.
    #[error(
        "`{program} mcp {subcommand}` did not finish in time and was stopped; d-mcp re-read the harness file to settle the write"
    )]
    HarnessCliTimeout {
        /// The program name.
        program: &'static str,
        /// The subcommand.
        subcommand: &'static str,
    },
    /// `state.toml` breaks a rule. It is never rewritten.
    #[error(
        "{} is corrupt at line {line}, column {column}{}; fix or remove it by hand, d-mcp will not rewrite it",
        path.display(),
        if field.is_empty() { String::new() } else { format!(", field `{field}`") }
    )]
    StateCorrupt {
        /// The file.
        path: PathBuf,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
        /// The field path.
        field: String,
    },
    /// At least one entry was blocked during `sync`.
    #[error(
        "{count} entr{} blocked (drift, collision, location change, unsupported style or an important gap); see the report",
        if *count == 1 { "y is" } else { "ies are" }
    )]
    SyncBlocked {
        /// The number of blocked entries.
        count: usize,
    },
    /// `unmanage` of a server without a write-state record.
    #[error("`{server}` is not managed by d-mcp in the requested harnesses")]
    NotManaged {
        /// The server name.
        server: String,
    },
    /// `sync --take-over` names a server that is neither selected nor
    /// recorded.
    #[error("`{name}` is neither selected nor managed; nothing to take over")]
    UnknownTakeOver {
        /// The name.
        name: String,
    },
    /// `adopt` refused: an entry cannot be adopted losslessly and
    /// literal-free, or an adoption precondition does not hold. The `data`
    /// of the `adopt` document names each refusal by entry and field, never
    /// by value.
    #[error(
        "{count} {} the adoption; see the report (nothing was written)",
        if *count == 1 { "refusal blocks" } else { "refusals block" }
    )]
    AdoptRefused {
        /// The number of refusals.
        count: usize,
    },
    /// An entry to adopt changed after the preview it was bound to with
    /// `--expect`. Nothing was written for it.
    #[error(
        "the entry `{entry}` changed since the preview (or its `--expect` hash does not match); run `d-mcp adopt` without `--apply` again"
    )]
    AdoptEntryChanged {
        /// `<harness>:<scope>:<name>`.
        entry: String,
    },
    /// `doctor` found at least one check at severity `error`. The `doctor`
    /// document lists every check.
    #[error(
        "{count} doctor {} at `error`; see the report",
        if *count == 1 { "check is" } else { "checks are" }
    )]
    DoctorFailed {
        /// The number of checks at `error`.
        count: usize,
    },
    /// `uninstall` left entries in the harnesses: drifted, location-changed
    /// or unsupported ones, whose records are kept unless `--purge`. The
    /// `uninstall` document names each entry.
    #[error(
        "{count} {} left in the harnesses (drifted, location changed or unsupported); see the report",
        if *count == 1 { "entry is" } else { "entries are" }
    )]
    UninstallIncomplete {
        /// The number of entries left.
        count: usize,
    },
    /// `uninstall --purge` was blocked (a location-changed record, a
    /// missing harness CLI, a failed harness, or a symlinked store root
    /// without `--follow-store-link`); nothing was purged.
    #[error(
        "{count} {} `--purge`; nothing was purged; see the report",
        if *count == 1 { "condition blocks" } else { "conditions block" }
    )]
    PurgeBlocked {
        /// The number of blocking conditions.
        count: usize,
    },
    /// `<store>/.hash-key` is not a regular file or does not hold a key.
    #[error(
        "the display-hash key {} is corrupt; remove it and run `d-mcp init` to create a new one",
        path.display()
    )]
    HashKeyCorrupt {
        /// The key file.
        path: PathBuf,
    },
    /// The operating system has no random source for a new display-hash key.
    #[error("cannot create the display-hash key: the system random source is unavailable")]
    HashKeyUnavailable,
    /// A Git source URL is not accepted. The hint never echoes the URL.
    #[error(
        "the Git source URL is not accepted (rule `{rule}`): use an `https://`, `ssh://`, scp-like (`git@host:org/repo.git`) or `file://` URL or an absolute path, without a password, a query or a fragment, and without user info on `https` and `file`; keep credentials in a credential helper or SSH"
    )]
    SourceUrlInvalid {
        /// The broken rule: `url-invalid`, `url-scheme`, `url-userinfo`,
        /// `url-query` or a secret rule.
        rule: Rule,
    },
    /// A Git ref or commit id is not valid.
    #[error(
        "the {what} is not valid: a ref must pass `git check-ref-format` and must not start with `-`; a commit id is 40 or 64 hex characters"
    )]
    SourceRefInvalid {
        /// `ref` or `commit`.
        what: &'static str,
    },
    /// The requested commit does not exist or is not reachable from the ref.
    #[error("commit {commit} is not reachable from the ref `{reference}` of source `{id}`")]
    SourceCommitUnreachable {
        /// The source id.
        id: String,
        /// The ref.
        reference: String,
        /// The commit id.
        commit: String,
    },
    /// The new commit of a refresh does not descend from the pinned one,
    /// or the source has no pin to check it against.
    #[error(
        "the ref `{reference}` of source `{id}` was rewritten (force-push or moved tag), or the source has no pin to check the new commit against; the old pin stays reachable, pass --allow-rewrite to accept the new commit"
    )]
    SourceRewrite {
        /// The source id.
        id: String,
        /// The ref.
        reference: String,
    },
    /// The ref of a Git source no longer exists on the remote.
    #[error(
        "the ref `{reference}` of source `{id}` no longer exists on the remote; the pin is kept"
    )]
    SourceRefDeleted {
        /// The source id.
        id: String,
        /// The ref.
        reference: String,
    },
    /// A Git source changed (removed, re-added or re-pinned) while a command
    /// worked on it. Nothing was written.
    #[error(
        "source `{id}` changed while d-mcp was working on it; nothing was written, run the command again"
    )]
    SourceChanged {
        /// The source id.
        id: String,
    },
    /// An approval is blocked: audit findings without a matching risk
    /// acceptance, an invalid or revoked definition (`--all-from`), or an
    /// untrusted source for `--all-from`.
    #[error("{}", approval_blocked_hint(*count, untrusted.as_deref()))]
    ApprovalBlocked {
        /// The number of blocked servers.
        count: usize,
        /// The source id, when `approve --all-from` was refused because the
        /// source is not trusted.
        untrusted: Option<String>,
    },
    /// `approve`, `accept-risk` or `revoke` of a server that needs no
    /// approval or has nothing to accept.
    #[error("`{server}` needs no {what}: {why}")]
    ApprovalNotNeeded {
        /// The server name, or the source id for `--all-from`.
        server: String,
        /// `approval` or `risk acceptance`.
        what: &'static str,
        /// Why, from a fixed vocabulary.
        why: &'static str,
    },
    /// `approve` or `accept-risk` without `--expect` outside a dry run.
    #[error("`{command}` needs --expect <{value}> from its preview; run it with --dry-run first")]
    ExpectRequired {
        /// The command.
        command: &'static str,
        /// What `--expect` takes: `definition_hash` or `pinned commit`.
        value: &'static str,
    },
    /// The `accept-risk` reason is not accepted. The reason is never echoed.
    #[error("the --reason is not accepted: {why}; it is not shown")]
    ReasonInvalid {
        /// Why, from a fixed vocabulary.
        why: &'static str,
    },
    /// The definition (or pinned commit) changed since the preview, or the
    /// `--expect` value does not match.
    #[error(
        "`{server}` changed since the preview (or its `--expect` value does not match); run the command with --dry-run again"
    )]
    ApprovalChanged {
        /// The server name, or the source id for `--all-from`.
        server: String,
    },
    /// `lock.toml` breaks a rule. It is never rewritten.
    #[error(
        "{} is corrupt at line {line}, column {column}{}; fix it by hand, d-mcp will not rewrite it",
        path.display(),
        if field.is_empty() { String::new() } else { format!(", field `{field}`") }
    )]
    LockCorrupt {
        /// The file.
        path: PathBuf,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
        /// The field path.
        field: String,
    },
    /// `approvals.toml` breaks a rule. It is never rewritten.
    #[error(
        "{} is corrupt at line {line}, column {column}{}; fix it by hand, d-mcp will not rewrite it",
        path.display(),
        if field.is_empty() { String::new() } else { format!(", field `{field}`") }
    )]
    ApprovalsCorrupt {
        /// The file.
        path: PathBuf,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
        /// The field path.
        field: String,
    },
    /// `git` was not found on `PATH`.
    #[error("`git` was not found on PATH; Git sources need it for add-git and refresh")]
    GitNotFound,
    /// A `git` call did not finish in time and was stopped.
    #[error("`git {operation}` did not finish in time and was stopped")]
    GitTimeout {
        /// The git subcommand.
        operation: &'static str,
    },
    /// `git` reported failed authentication. Its output is never shown.
    #[error(
        "`git {operation}` failed: authentication failed; d-mcp never prompts, so set up a credential helper or an SSH key (its output is not shown)"
    )]
    GitAuthFailed {
        /// The git subcommand.
        operation: &'static str,
    },
    /// `git` refused an unknown or changed SSH host key.
    #[error(
        "`git {operation}` failed: the SSH host key is unknown or changed; verify it and add it to known_hosts first (its output is not shown)"
    )]
    GitHostUnknown {
        /// The git subcommand.
        operation: &'static str,
    },
    /// `git` could not reach the host or the repository.
    #[error(
        "`git {operation}` failed: the host or repository cannot be reached (its output is not shown)"
    )]
    GitUnreachable {
        /// The git subcommand.
        operation: &'static str,
    },
    /// The requested ref does not exist on the remote.
    #[error(
        "`git {operation}` failed: the ref does not exist on the remote (its output is not shown)"
    )]
    GitRefNotFound {
        /// The git subcommand.
        operation: &'static str,
    },
    /// Any other `git` failure. Its output is never shown.
    #[error("`git {operation}` failed ({status}); its output is not shown")]
    GitFailed {
        /// The git subcommand.
        operation: &'static str,
        /// The exit status, such as `exit status 128`.
        status: String,
    },
    /// A file-system operation failed.
    #[error("cannot {operation} {}: {}", path.display(), io::Error::from(*kind))]
    Io {
        /// What d-mcp was doing.
        operation: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The I/O error kind (the OS message is not kept).
        kind: io::ErrorKind,
    },
}

/// The hint of [`Error::ApprovalBlocked`].
fn approval_blocked_hint(count: usize, untrusted: Option<&str>) -> String {
    match untrusted {
        Some(id) => format!(
            "source `{id}` is not trusted; `approve --all-from` needs a trusted source (`d-mcp source trust {id} --on`), or approve servers one by one"
        ),
        None => format!(
            "{count} {} blocked from approval (audit findings without a matching `accept-risk`, an invalid definition, or a revoked server); see the report",
            if count == 1 {
                "server is"
            } else {
                "servers are"
            }
        ),
    }
}

impl Error {
    /// Builds an [`Error::Io`] from an I/O error.
    #[must_use]
    pub fn io(operation: &'static str, path: impl Into<PathBuf>, error: &io::Error) -> Self {
        Error::Io {
            operation,
            path: path.into(),
            kind: error.kind(),
        }
    }

    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::StoreNotInitialized { .. } => "store-not-initialized",
            Error::StorePathInvalid { .. } => "store-path-invalid",
            Error::ForeignDirectory { .. } => "foreign-directory",
            Error::InvalidName { .. } => "invalid-name",
            Error::SourceIdReserved => "source-id-reserved",
            Error::SourceIdTaken { .. } => "source-id-taken",
            Error::PriorityInvalid => "priority-invalid",
            Error::PriorityTaken { .. } => "priority-taken",
            Error::SourcePathNotFound { .. } => "source-path-not-found",
            Error::SourcePathNotDirectory { .. } => "source-path-not-directory",
            Error::SourcePathNotUtf8 => "source-path-not-utf8",
            Error::SourcePathRegistered { .. } => "source-path-registered",
            Error::SourcePathInStore { .. } => "source-path-in-store",
            Error::SourceNotFound { .. } => "source-not-found",
            Error::NotSelected { .. } => "not-selected",
            Error::ConfigCorrupt { .. } => "config-corrupt",
            Error::SchemaUnsupported { .. } => "schema-unsupported",
            Error::StoreLocked { .. } => "store-locked",
            Error::HarnessFileChanged { .. } => "harness-file-changed",
            Error::HarnessConfigInvalid { .. } => "harness-config-invalid",
            Error::HarnessVerifyFailed { .. } => "harness-verify-failed",
            Error::HarnessEditFailed { .. } => "harness-edit-failed",
            Error::HarnessPathNotUtf8 { .. } => "harness-path-not-utf8",
            Error::HarnessCliNotFound { .. } => "harness-cli-not-found",
            Error::HarnessCliFailed { .. } => "harness-cli-failed",
            Error::HarnessCliTimeout { .. } => "harness-cli-timeout",
            Error::HarnessNoNeutralDir { .. } => "harness-no-neutral-dir",
            Error::StateCorrupt { .. } => "state-corrupt",
            Error::SyncBlocked { .. } => "sync-blocked",
            Error::NotManaged { .. } => "not-managed",
            Error::UnknownTakeOver { .. } => "unknown-take-over",
            Error::AdoptRefused { .. } => "adopt-refused",
            Error::AdoptEntryChanged { .. } => "adopt-entry-changed",
            Error::DoctorFailed { .. } => "doctor-failed",
            Error::UninstallIncomplete { .. } => "uninstall-incomplete",
            Error::PurgeBlocked { .. } => "purge-blocked",
            Error::HomeUnset => "home-unset",
            Error::CurrentDirUnavailable => "cwd-unavailable",
            Error::HashKeyCorrupt { .. } => "hash-key-corrupt",
            Error::HashKeyUnavailable => "hash-key-unavailable",
            Error::SourceUrlInvalid { .. } => "source-url-invalid",
            Error::SourceRefInvalid { .. } => "source-ref-invalid",
            Error::SourceCommitUnreachable { .. } => "source-commit-unreachable",
            Error::SourceRewrite { .. } => "source-rewrite",
            Error::SourceRefDeleted { .. } => "source-ref-deleted",
            Error::SourceChanged { .. } => "source-changed",
            Error::ApprovalBlocked { .. } => "approval-blocked",
            Error::ApprovalNotNeeded { .. } => "approval-not-needed",
            Error::ExpectRequired { .. } => "expect-required",
            Error::ReasonInvalid { .. } => "reason-invalid",
            Error::ApprovalChanged { .. } => "approval-changed",
            Error::LockCorrupt { .. } => "lock-corrupt",
            Error::ApprovalsCorrupt { .. } => "approvals-corrupt",
            Error::GitNotFound => "git-not-found",
            Error::GitTimeout { .. } => "git-timeout",
            Error::GitAuthFailed { .. } => "git-auth-failed",
            Error::GitHostUnknown { .. } => "git-host-unknown",
            Error::GitUnreachable { .. } => "git-unreachable",
            Error::GitRefNotFound { .. } => "git-ref-not-found",
            Error::GitFailed { .. } => "git-failed",
            Error::Io { .. } => "io-error",
        }
    }

    /// The documented exit code.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::StoreNotInitialized { .. }
            | Error::StorePathInvalid { .. }
            | Error::ForeignDirectory { .. }
            | Error::InvalidName { .. }
            | Error::SourceIdReserved
            | Error::SourceIdTaken { .. }
            | Error::PriorityInvalid
            | Error::PriorityTaken { .. }
            | Error::SourcePathNotFound { .. }
            | Error::SourcePathNotDirectory { .. }
            | Error::SourcePathNotUtf8
            | Error::SourcePathRegistered { .. }
            | Error::SourcePathInStore { .. }
            | Error::SourceNotFound { .. }
            | Error::NotSelected { .. }
            | Error::SyncBlocked { .. }
            | Error::NotManaged { .. }
            | Error::UnknownTakeOver { .. }
            | Error::AdoptRefused { .. }
            | Error::DoctorFailed { .. }
            | Error::UninstallIncomplete { .. }
            | Error::PurgeBlocked { .. }
            | Error::HarnessCliFailed { .. }
            | Error::HarnessNoNeutralDir { .. }
            | Error::SourceUrlInvalid { .. }
            | Error::SourceRefInvalid { .. }
            | Error::SourceCommitUnreachable { .. }
            | Error::SourceRewrite { .. }
            | Error::SourceRefDeleted { .. }
            | Error::ApprovalBlocked { .. }
            | Error::ApprovalNotNeeded { .. }
            | Error::ExpectRequired { .. }
            | Error::ReasonInvalid { .. } => EXIT_FAILURE,
            Error::ConfigCorrupt { .. }
            | Error::SchemaUnsupported { .. }
            | Error::StoreLocked { .. }
            | Error::HarnessFileChanged { .. }
            | Error::HarnessConfigInvalid { .. }
            | Error::HarnessVerifyFailed { .. }
            | Error::HarnessEditFailed { .. }
            | Error::StateCorrupt { .. }
            | Error::AdoptEntryChanged { .. }
            | Error::HashKeyCorrupt { .. }
            | Error::ApprovalChanged { .. }
            | Error::SourceChanged { .. }
            | Error::LockCorrupt { .. }
            | Error::ApprovalsCorrupt { .. } => EXIT_UNSAFE,
            Error::HomeUnset
            | Error::CurrentDirUnavailable
            | Error::HarnessPathNotUtf8 { .. }
            | Error::HarnessCliNotFound { .. }
            | Error::HarnessCliTimeout { .. }
            | Error::HashKeyUnavailable
            | Error::GitNotFound
            | Error::GitTimeout { .. }
            | Error::GitAuthFailed { .. }
            | Error::GitHostUnknown { .. }
            | Error::GitUnreachable { .. }
            | Error::GitRefNotFound { .. }
            | Error::GitFailed { .. }
            | Error::Io { .. } => EXIT_ENVIRONMENT,
        }
    }

    /// The human-readable hint; the same text as [`std::fmt::Display`].
    #[must_use]
    pub fn hint(&self) -> String {
        self.to_string()
    }

    /// One sample of every variant, for contract tests. The exhaustive match
    /// makes adding a variant without a sample a compile error.
    #[must_use]
    pub fn samples() -> Vec<Error> {
        let path = PathBuf::from("/store");
        let samples = vec![
            Error::StoreNotInitialized { path: path.clone() },
            Error::StorePathInvalid { path: path.clone() },
            Error::ForeignDirectory { path: path.clone() },
            Error::InvalidName { what: "source id" },
            Error::SourceIdReserved,
            Error::SourceIdTaken { id: "team".into() },
            Error::PriorityInvalid,
            Error::PriorityTaken {
                priority: 1,
                id: "team".into(),
            },
            Error::SourcePathNotFound { path: path.clone() },
            Error::SourcePathNotDirectory { path: path.clone() },
            Error::SourcePathNotUtf8,
            Error::SourcePathRegistered {
                path: path.clone(),
                id: "team".into(),
            },
            Error::SourcePathInStore { path: path.clone() },
            Error::SourceNotFound { id: "team".into() },
            Error::NotSelected {
                server: "github".into(),
            },
            Error::ConfigCorrupt {
                path: path.clone(),
                line: 1,
                column: 1,
                field: "sources[0].id".into(),
                rule: Rule::NameRule,
            },
            Error::SchemaUnsupported {
                path: path.clone(),
                found: 2,
            },
            Error::StoreLocked { path: path.clone() },
            Error::HarnessFileChanged { path: path.clone() },
            Error::HarnessConfigInvalid {
                path: path.clone(),
                reason: "not valid TOML",
                position: Some((2, 3)),
            },
            Error::HarnessVerifyFailed {
                path: path.clone(),
                restored: Some(true),
            },
            Error::HarnessCliNotFound {
                harness: "claude",
                program: "claude",
            },
            Error::HarnessCliFailed {
                program: "claude",
                subcommand: "add-json",
                status: "exit status 1".into(),
            },
            Error::HarnessCliTimeout {
                program: "claude",
                subcommand: "remove",
            },
            Error::HarnessNoNeutralDir {
                harness: "claude",
                program: "claude",
                path: PathBuf::from("/tmp/d-mcp-x"),
            },
            Error::HarnessEditFailed { path: path.clone() },
            Error::HarnessPathNotUtf8 { harness: "codex" },
            Error::StateCorrupt {
                path: path.clone(),
                line: 1,
                column: 1,
                field: "records[0].sent".into(),
            },
            Error::SyncBlocked { count: 2 },
            Error::NotManaged {
                server: "github".into(),
            },
            Error::UnknownTakeOver {
                name: "github".into(),
            },
            Error::AdoptRefused { count: 2 },
            Error::AdoptEntryChanged {
                entry: "claude:user:github".into(),
            },
            Error::DoctorFailed { count: 1 },
            Error::UninstallIncomplete { count: 2 },
            Error::PurgeBlocked { count: 1 },
            Error::HomeUnset,
            Error::CurrentDirUnavailable,
            Error::HashKeyCorrupt { path: path.clone() },
            Error::HashKeyUnavailable,
            Error::SourceUrlInvalid {
                rule: Rule::UrlUserinfo,
            },
            Error::SourceRefInvalid { what: "ref" },
            Error::SourceCommitUnreachable {
                id: "team".into(),
                reference: "main".into(),
                commit: "0".repeat(40),
            },
            Error::SourceRewrite {
                id: "team".into(),
                reference: "main".into(),
            },
            Error::SourceRefDeleted {
                id: "team".into(),
                reference: "main".into(),
            },
            Error::SourceChanged { id: "team".into() },
            Error::ApprovalBlocked {
                count: 1,
                untrusted: None,
            },
            Error::ApprovalNotNeeded {
                server: "github".into(),
                what: "approval",
                why: "it does not come from a Git source",
            },
            Error::ExpectRequired {
                command: "approve",
                value: "definition_hash",
            },
            Error::ReasonInvalid {
                why: "it looks like a secret",
            },
            Error::ApprovalChanged {
                server: "github".into(),
            },
            Error::LockCorrupt {
                path: path.clone(),
                line: 1,
                column: 1,
                field: "pins[0].commit".into(),
            },
            Error::ApprovalsCorrupt {
                path: path.clone(),
                line: 1,
                column: 1,
                field: String::new(),
            },
            Error::GitNotFound,
            Error::GitTimeout { operation: "clone" },
            Error::GitAuthFailed { operation: "clone" },
            Error::GitHostUnknown { operation: "clone" },
            Error::GitUnreachable { operation: "clone" },
            Error::GitRefNotFound { operation: "clone" },
            Error::GitFailed {
                operation: "ls-tree",
                status: "exit status 128".into(),
            },
            Error::Io {
                operation: "read",
                path,
                kind: io::ErrorKind::PermissionDenied,
            },
        ];
        for sample in &samples {
            // Exhaustiveness guard: a new variant must be listed here and
            // above.
            match sample {
                Error::StoreNotInitialized { .. }
                | Error::StorePathInvalid { .. }
                | Error::ForeignDirectory { .. }
                | Error::InvalidName { .. }
                | Error::SourceIdReserved
                | Error::SourceIdTaken { .. }
                | Error::PriorityInvalid
                | Error::PriorityTaken { .. }
                | Error::SourcePathNotFound { .. }
                | Error::SourcePathNotDirectory { .. }
                | Error::SourcePathNotUtf8
                | Error::SourcePathRegistered { .. }
                | Error::SourcePathInStore { .. }
                | Error::SourceNotFound { .. }
                | Error::NotSelected { .. }
                | Error::ConfigCorrupt { .. }
                | Error::SchemaUnsupported { .. }
                | Error::StoreLocked { .. }
                | Error::HarnessFileChanged { .. }
                | Error::HarnessConfigInvalid { .. }
                | Error::HarnessVerifyFailed { .. }
                | Error::HarnessEditFailed { .. }
                | Error::HarnessPathNotUtf8 { .. }
                | Error::HarnessCliNotFound { .. }
                | Error::HarnessCliFailed { .. }
                | Error::HarnessCliTimeout { .. }
                | Error::HarnessNoNeutralDir { .. }
                | Error::StateCorrupt { .. }
                | Error::SyncBlocked { .. }
                | Error::NotManaged { .. }
                | Error::UnknownTakeOver { .. }
                | Error::AdoptRefused { .. }
                | Error::AdoptEntryChanged { .. }
                | Error::DoctorFailed { .. }
                | Error::UninstallIncomplete { .. }
                | Error::PurgeBlocked { .. }
                | Error::HomeUnset
                | Error::CurrentDirUnavailable
                | Error::HashKeyCorrupt { .. }
                | Error::HashKeyUnavailable
                | Error::SourceUrlInvalid { .. }
                | Error::SourceRefInvalid { .. }
                | Error::SourceCommitUnreachable { .. }
                | Error::SourceRewrite { .. }
                | Error::SourceRefDeleted { .. }
                | Error::SourceChanged { .. }
                | Error::ApprovalBlocked { .. }
                | Error::ApprovalNotNeeded { .. }
                | Error::ExpectRequired { .. }
                | Error::ReasonInvalid { .. }
                | Error::ApprovalChanged { .. }
                | Error::LockCorrupt { .. }
                | Error::ApprovalsCorrupt { .. }
                | Error::GitNotFound
                | Error::GitTimeout { .. }
                | Error::GitAuthFailed { .. }
                | Error::GitHostUnknown { .. }
                | Error::GitUnreachable { .. }
                | Error::GitRefNotFound { .. }
                | Error::GitFailed { .. }
                | Error::Io { .. } => {}
            }
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFERENCE: &str = include_str!("../docs/reference.md");

    /// Parses the error-code table of `docs/reference.md`: rows of the form
    /// `` | `code` | exit | ... | ``.
    fn documented_codes() -> Vec<(String, u8)> {
        let section = REFERENCE
            .split("<!-- error-codes:start -->")
            .nth(1)
            .and_then(|rest| rest.split("<!-- error-codes:end -->").next())
            .expect("docs/reference.md has an error-code table");
        section
            .lines()
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                let code = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
                let exit = cells.get(2)?.parse().ok()?;
                Some((code.to_owned(), exit))
            })
            .collect()
    }

    #[test]
    fn every_variant_matches_the_documented_table() {
        let documented = documented_codes();
        let samples = Error::samples();
        assert_eq!(
            documented.len(),
            samples.len(),
            "the documented table and the enum differ in size"
        );
        for sample in &samples {
            assert!(
                documented.contains(&(sample.code().to_owned(), sample.exit_code())),
                "`{}` with exit {} is not documented",
                sample.code(),
                sample.exit_code()
            );
            assert!(!sample.hint().is_empty());
        }
    }

    #[test]
    fn exit_codes_are_in_the_documented_range() {
        for sample in Error::samples() {
            assert!(matches!(
                sample.exit_code(),
                EXIT_FAILURE | EXIT_UNSAFE | EXIT_ENVIRONMENT
            ));
        }
        assert_eq!(EXIT_USAGE, 2);
    }

    #[test]
    fn io_errors_keep_only_the_kind() {
        let error = Error::io(
            "read",
            "/store/config.toml",
            &io::Error::new(io::ErrorKind::NotFound, "ghp_secret in an OS message"),
        );
        assert!(!error.to_string().contains("ghp_secret"));
        assert_eq!(error.code(), "io-error");
        assert_eq!(error.exit_code(), EXIT_ENVIRONMENT);
    }

    #[test]
    fn harness_hints_name_positions_only() {
        let invalid = Error::HarnessConfigInvalid {
            path: PathBuf::from("/c/config.toml"),
            reason: "not valid TOML",
            position: Some((4, 2)),
        };
        assert_eq!(
            invalid.hint(),
            "/c/config.toml is not valid TOML at line 4, column 2; fix it by hand, d-mcp will not edit it"
        );
        let regular = Error::HarnessConfigInvalid {
            path: PathBuf::from("/c/config.toml"),
            reason: "not a regular file",
            position: None,
        };
        assert!(regular.hint().contains("not a regular file; fix"));
        let failed = Error::HarnessVerifyFailed {
            path: PathBuf::from("/c/config.toml"),
            restored: Some(false),
        };
        assert!(failed.hint().contains("FAILED"));
        let cli = Error::HarnessVerifyFailed {
            path: PathBuf::from("/c/.claude.json"),
            restored: None,
        };
        assert!(cli.hint().contains("left as the harness wrote it"));
        let failed = Error::HarnessCliFailed {
            program: "claude",
            subcommand: "add-json",
            status: "exit status 1".into(),
        };
        assert_eq!(
            failed.hint(),
            "`claude mcp add-json` failed (exit status 1); its output is not shown because it can contain secrets"
        );
        let neutral = Error::HarnessNoNeutralDir {
            harness: "claude",
            program: "claude",
            path: PathBuf::from("/tmp/d-mcp-x"),
        };
        assert_eq!(
            neutral.hint(),
            "no neutral working directory for `claude`: /tmp/d-mcp-x has a `.mcp.json` or a `projects` entry of the claude configuration at or above it, so nothing was run for claude; remove it or set TMPDIR to another directory"
        );
        assert_eq!(neutral.code(), "harness-no-neutral-dir");
        assert_eq!(neutral.exit_code(), EXIT_FAILURE);
        assert!(
            Error::SyncBlocked { count: 1 }
                .hint()
                .starts_with("1 entry is")
        );
        let state = Error::StateCorrupt {
            path: PathBuf::from("/s/state.toml"),
            line: 2,
            column: 1,
            field: String::new(),
        };
        assert!(state.hint().contains("line 2, column 1; fix"));
    }

    #[test]
    fn corrupt_config_hint_names_position_and_rule() {
        let error = Error::ConfigCorrupt {
            path: PathBuf::from("/s/config.toml"),
            line: 3,
            column: 7,
            field: String::new(),
            rule: Rule::TomlSyntax,
        };
        assert_eq!(
            error.hint(),
            "/s/config.toml is corrupt at line 3, column 7 (rule `toml-syntax`); fix it by hand, d-mcp will not rewrite it"
        );
    }
}
