//! Discovery and import types of the adapter contract (WP4).
//!
//! Writes and the write state stay at [`super::Scope::User`]. Discovery reads
//! more scopes: the user scope, Claude Code's local scope (entry names and
//! project paths only, see ADR 0005), and the project scope of the current
//! repository (`.mcp.json` for Claude Code, `.codex/config.toml` for Codex).
//! Each scope is read on its own into a [`ScopeRead`]; an unreadable file is
//! that scope's error and never aborts the discovery of the others.
//!
//! Foreign entry content is held in [`RawEntry`] and [`Literal`], which have
//! names-only [`fmt::Debug`] forms and implement neither `Serialize` nor
//! `Display`; output goes through [`crate::redact`]. An [`Imported`]
//! candidate is not validated yet, so it never uses `Definition`'s derived
//! `Debug` either.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::HarnessRead;
use crate::config::Harness;
use crate::definition::{Definition, is_valid_variable_name};
use crate::error::{Error, Result};
use crate::store::{CappedReadError, read_regular_capped};

/// The largest repository file a harness adapter reads (`.mcp.json`,
/// `.codex/config.toml`, Claude Code settings files): 1 MiB.
pub const MAX_REPOSITORY_FILE_BYTES: u64 = 1024 * 1024;

/// A scope an adapter discovers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiscoveredScope {
    /// The user's global configuration (the write scope).
    User,
    /// Claude Code's local scope of one project: `projects.<path>.mcpServers`
    /// in its configuration file. Names only.
    Local {
        /// The project path, as written in the file.
        project: PathBuf,
    },
    /// The project scope of the current repository.
    Project {
        /// The repository root.
        root: PathBuf,
    },
}

impl DiscoveredScope {
    /// The stable string form: `user`, `local` or `project`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            DiscoveredScope::User => "user",
            DiscoveredScope::Local { .. } => "local",
            DiscoveredScope::Project { .. } => "project",
        }
    }

    /// The project path or repository root, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            DiscoveredScope::User => None,
            DiscoveredScope::Local { project } => Some(project),
            DiscoveredScope::Project { root } => Some(root),
        }
    }
}

/// Whether a harness loads a project scope at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// The harness loads the scope.
    Trusted,
    /// The harness ignores the scope until the user trusts the project.
    Untrusted,
    /// d-mcp cannot tell.
    UnknownTrust,
}

impl Trust {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
            Trust::UnknownTrust => "unknown-trust",
        }
    }
}

/// The approval of a Claude Code project-scope (`.mcp.json`) entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Approval {
    /// Enabled by name or by `enableAllProjectMcpServers`.
    Approved,
    /// Listed in `disabledMcpjsonServers`; the harness does not load it.
    Disabled,
    /// Neither: Claude Code asks before it loads the entry.
    Pending,
    /// A settings file could not be read.
    Unknown,
}

impl Approval {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Approval::Approved => "approved",
            Approval::Disabled => "disabled",
            Approval::Pending => "pending",
            Approval::Unknown => "unknown",
        }
    }
}

/// A foreign entry value as read from a harness file. It can hold secrets:
/// its [`fmt::Debug`] form lists the top-level key names only, and it has no
/// `Serialize` or `Display`. Codex's preserved keys never enter it.
#[derive(Clone, PartialEq, Eq)]
pub struct RawEntry(Value);

impl RawEntry {
    /// Wraps a value read from a harness file.
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self(value)
    }

    /// The value. Never print it; render it through [`crate::redact`].
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// The top-level key names, when the value is an object.
    #[must_use]
    pub fn keys(&self) -> Vec<&str> {
        self.0
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

impl fmt::Debug for RawEntry {
    /// The values are foreign and never printed; only the key names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawEntry")
            .field("keys", &self.keys())
            .finish()
    }
}

/// One discovered entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredEntry {
    /// The value; `None` for a names-only scope (Claude Code's local scope).
    pub raw: Option<RawEntry>,
    /// The adapter's semantic entry hash of the value; `None` without one.
    /// Never shown for an entry d-mcp does not own (see
    /// [`crate::hashkey`]).
    pub hash: Option<String>,
    /// Whether the entry is written in a style d-mcp can edit.
    pub supported_style: bool,
    /// Harness-local preserved keys present in the entry (names only).
    pub preserved: BTreeSet<&'static str>,
    /// The approval of a Claude Code `.mcp.json` entry; `None` elsewhere.
    pub approval: Option<Approval>,
}

impl DiscoveredEntry {
    /// An entry known by name only.
    #[must_use]
    pub fn name_only() -> Self {
        Self {
            raw: None,
            hash: None,
            supported_style: true,
            preserved: BTreeSet::new(),
            approval: None,
        }
    }
}

/// What an adapter read from one discovered scope.
#[derive(Debug, Clone)]
pub struct ScopeRead {
    /// The harness.
    pub harness: Harness,
    /// The scope.
    pub scope: DiscoveredScope,
    /// The file the scope lives in, as resolved before following symlinks.
    pub file: PathBuf,
    /// Whether the scope exists: the file exists (for the user scope, also
    /// when only the harness binary is on `PATH`).
    pub present: bool,
    /// Whether the harness loads a project scope; `None` for other scopes.
    pub trust: Option<Trust>,
    /// Local scope: the project path no longer exists.
    pub orphaned: bool,
    /// The entries by name.
    pub entries: BTreeMap<String, DiscoveredEntry>,
    /// Why the scope could not be read; its entries are then empty.
    pub error: Option<Error>,
    /// The user scope's read, for planning against the write state.
    pub read: Option<HarnessRead>,
}

impl ScopeRead {
    /// An empty scope read.
    #[must_use]
    pub fn new(harness: Harness, scope: DiscoveredScope, file: PathBuf) -> Self {
        Self {
            harness,
            scope,
            file,
            present: false,
            trust: None,
            orphaned: false,
            entries: BTreeMap::new(),
            error: None,
            read: None,
        }
    }

    /// The same scope read with an error and no entries.
    #[must_use]
    pub fn failed(mut self, error: Error) -> Self {
        self.entries.clear();
        self.error = Some(error);
        self
    }
}

/// A literal value of a foreign entry that adoption replaces with a variable
/// reference. It is never printed: its [`fmt::Debug`] form is redacted, and
/// it has no `Serialize` or `Display`. Adoption needs it in memory only (the
/// variable gate compares it with the variable's value).
#[derive(Clone, PartialEq, Eq)]
pub struct Literal(String);

impl Literal {
    /// Wraps a literal value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value. Never print or persist it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Literal(<redacted>)")
    }
}

/// What kind of literal a [`Replacement`] replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplacementKind {
    /// An env literal; it becomes a forward of the same key name.
    Env,
    /// A literal `Authorization: Bearer <token>` header; it becomes
    /// `bearer_env`.
    BearerHeader,
    /// Another literal header; it becomes `header_env`.
    Header,
}

impl ReplacementKind {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReplacementKind::Env => "env",
            ReplacementKind::BearerHeader => "bearer-header",
            ReplacementKind::Header => "header",
        }
    }
}

/// A literal of the native entry that the candidate definition holds as a
/// variable reference instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    /// The native field, such as `env.TOKEN` or `headers.Authorization`.
    pub field: String,
    /// The kind.
    pub kind: ReplacementKind,
    /// The variable the candidate reads instead: the env key name, or a
    /// proposal for a header (`<SERVER>_TOKEN`, `<SERVER>_<HEADER>`).
    pub var: String,
    /// The replaced value, in memory only.
    pub literal: Literal,
}

/// A difference between the native entry and the rendering of the candidate
/// that adoption declares instead of treating it as a loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NormalizationKind {
    /// A Claude Code forward `${VAR}` is rendered as `${VAR:-}`.
    EmptyDefault,
    /// A Claude Code header reference `${VAR:-}` is rendered as `${VAR}`.
    DefaultDropped,
    /// OAuth scopes are rendered sorted and deduplicated.
    ScopesSorted,
    /// Codex `env_vars` are rendered as names, sorted and deduplicated.
    EnvVarsSorted,
}

impl NormalizationKind {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NormalizationKind::EmptyDefault => "empty-default",
            NormalizationKind::DefaultDropped => "default-dropped",
            NormalizationKind::ScopesSorted => "scopes-sorted",
            NormalizationKind::EnvVarsSorted => "env-vars-sorted",
        }
    }
}

/// A declared normalization of one native field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Normalization {
    /// The native field.
    pub field: String,
    /// The kind.
    pub kind: NormalizationKind,
}

/// A native entry imported as a neutral candidate definition. The candidate
/// is not validated: adoption serializes it and runs the definition parser
/// and `validate_definition` on it.
#[derive(Clone, PartialEq)]
pub struct Imported {
    /// The candidate; every env and header literal is already a reference
    /// (see `replacements`).
    pub definition: Definition,
    /// The literals the candidate holds as references, in field order.
    pub replacements: Vec<Replacement>,
    /// The declared normalizations, in field order.
    pub normalizations: Vec<Normalization>,
}

impl fmt::Debug for Imported {
    /// Names only: the candidate can hold foreign `command`, `args` and URL
    /// values.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Imported")
            .field("name", &self.definition.name)
            .field("transport", &self.definition.transport_kind())
            .field(
                "replacements",
                &self
                    .replacements
                    .iter()
                    .map(|replacement| replacement.field.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("normalizations", &self.normalizations)
            .finish()
    }
}

impl Imported {
    /// Whether two imports describe the same server: equal `core_hash` and
    /// the same replaced literal per variable. Compared in memory only;
    /// neither the hash nor a literal is ever output.
    #[must_use]
    pub fn same_core(&self, other: &Imported) -> bool {
        let literals = |imported: &Imported| -> BTreeMap<String, String> {
            imported
                .replacements
                .iter()
                .map(|replacement| {
                    (
                        replacement.var.clone(),
                        replacement.literal.expose().to_owned(),
                    )
                })
                .collect()
        };
        self.definition.core_hash() == other.definition.core_hash()
            && literals(self) == literals(other)
    }
}

/// Why a native entry cannot be imported losslessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportGapReason {
    /// A Claude Code local-scope entry: d-mcp reads names only.
    LocalScope,
    /// The entry's scope could not be read.
    ScopeUnreadable,
    /// No such entry in the scope.
    NotFound,
    /// The entry is not an object or table.
    NotAnObject,
    /// The entry is written in an inline or dotted style (Codex).
    UnsupportedStyle,
    /// A native key outside the adapter mapping.
    UnknownKey,
    /// A native key d-mcp deliberately does not manage (`headersHelper`,
    /// `http_headers_helper`, approval modes, `experimental_environment`).
    ExcludedKey,
    /// A value of the wrong type or shape.
    InvalidValue,
    /// Neither a command nor a URL.
    MissingTransport,
    /// An SSE transport, which neutral definitions do not model.
    SseTransport,
    /// More than one header: a definition carries exactly one.
    MultipleHeaders,
    /// A header next to OAuth settings: a definition carries one auth.
    HeaderWithOauth,
    /// A forward of another variable (`${OTHER}` under key `NAME`).
    RenamedForward,
    /// A forward with a non-empty default (`${VAR:-default}`).
    ForwardDefault,
    /// Variable syntax inside a longer value.
    EmbeddedVariable,
    /// A Codex `env_vars` entry whose source is not `local`.
    EnvSource,
    /// A name the definition would carry is not shown as it is
    /// ([`crate::redact::safe_key`]): an env key (of a literal that would
    /// become a variable reference, or of a forward) or a variable (that a
    /// header or bearer token reads) that is not a valid variable name or
    /// looks like a secret, or a header name (of a literal or a referenced
    /// header) that looks like a secret. Adoption never takes over a name it
    /// cannot show.
    InvalidName,
}

impl ImportGapReason {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ImportGapReason::LocalScope => "local-scope",
            ImportGapReason::ScopeUnreadable => "scope-unreadable",
            ImportGapReason::NotFound => "not-found",
            ImportGapReason::NotAnObject => "not-an-object",
            ImportGapReason::UnsupportedStyle => "unsupported-style",
            ImportGapReason::UnknownKey => "unknown-key",
            ImportGapReason::ExcludedKey => "excluded-key",
            ImportGapReason::InvalidValue => "invalid-value",
            ImportGapReason::MissingTransport => "missing-transport",
            ImportGapReason::SseTransport => "sse-transport",
            ImportGapReason::MultipleHeaders => "multiple-headers",
            ImportGapReason::HeaderWithOauth => "header-with-oauth",
            ImportGapReason::RenamedForward => "renamed-forward",
            ImportGapReason::ForwardDefault => "forward-default",
            ImportGapReason::EmbeddedVariable => "embedded-variable",
            ImportGapReason::EnvSource => "env-source",
            ImportGapReason::InvalidName => "invalid-name",
        }
    }
}

/// Why one field of a native entry cannot be imported. Names only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportGap {
    /// The native field path; a key name that does not look like a name is
    /// replaced by [`crate::redact::REDACTED_KEY`].
    pub field: String,
    /// The reason.
    pub reason: ImportGapReason,
}

impl ImportGap {
    /// A gap on a field path built from known names.
    #[must_use]
    pub fn new(field: impl Into<String>, reason: ImportGapReason) -> Self {
        Self {
            field: field.into(),
            reason,
        }
    }

    /// The gaps for an entry that cannot be imported at all, by the scope it
    /// was found in: a names-only local scope, an unreadable scope, or no
    /// entry of that name.
    #[must_use]
    pub fn for_scope(read: &ScopeRead, name: &str) -> Option<Vec<ImportGap>> {
        let reason = if matches!(read.scope, DiscoveredScope::Local { .. }) {
            ImportGapReason::LocalScope
        } else if read.error.is_some() {
            ImportGapReason::ScopeUnreadable
        } else if !read.entries.contains_key(name) {
            ImportGapReason::NotFound
        } else {
            return None;
        };
        Some(vec![ImportGap::new("", reason)])
    }
}

/// The variable proposed for a literal header of `server`: `<SERVER>_TOKEN`
/// for a bearer token, `<SERVER>_<HEADER>` otherwise, in upper case with
/// `-` (and any other character outside `[A-Za-z0-9_]`) turned into `_`.
/// Adoption refuses a proposal that is not a valid variable name or is a
/// credential name, and `--var` overrides it.
#[must_use]
pub fn proposed_variable(server: &str, header: Option<&str>) -> String {
    let suffix = header.unwrap_or("TOKEN");
    format!("{server}_{suffix}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether a native key name is shown as it is by
/// [`crate::redact::safe_key`].
pub(crate) fn is_shown_name(name: &str) -> bool {
    crate::redact::safe_key(name) == name
}

/// What a name the definition would carry names, for [`carried_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NameKind {
    /// An env key, or the variable a header, bearer token or client secret
    /// reads: it must also be a valid variable name.
    Variable,
    /// A header name.
    Header,
}

/// The one check on every name an import puts into the definition, whether
/// it comes from a literal, a forward or a reference: an env key (the key
/// name an env literal is replaced by is its own key), a header name, and
/// the variable a header, bearer token or client secret reads. The name
/// must be shown as it is ([`crate::redact::safe_key`]), and a
/// [`NameKind::Variable`] must be a valid variable name. `Err` is the
/// [`ImportGapReason::InvalidName`] gap on `field`: the definition, and with
/// it `local/servers/*.toml`, would carry the name.
pub(crate) fn carried_name(
    field: &str,
    name: &str,
    kind: NameKind,
) -> std::result::Result<(), ImportGap> {
    let valid = match kind {
        NameKind::Variable => is_valid_variable_name(name),
        NameKind::Header => true,
    };
    if valid && is_shown_name(name) {
        Ok(())
    } else {
        Err(ImportGap::new(field, ImportGapReason::InvalidName))
    }
}

/// Maps a literal header value to the auth that reads it from a proposed
/// variable, and the replacement that records it: an `Authorization` value
/// `Bearer <token>` becomes `bearer_env`, any other header `header_env`.
///
/// A header name that is not shown as it is ([`crate::redact::safe_key`])
/// is the [`ImportGapReason::InvalidName`] gap on `field`: the definition
/// would carry it. A proposed variable that looks like a secret is kept;
/// adoption refuses it (`proposal-invalid`, `--var` chooses another) and
/// every output shows it as [`crate::redact::REDACTED_KEY`].
pub(crate) fn literal_header(
    server: &str,
    field: String,
    header: &str,
    value: &str,
) -> std::result::Result<(crate::definition::Auth, Replacement), ImportGap> {
    use crate::definition::Auth;
    carried_name(&field, header, NameKind::Header)?;
    if header.eq_ignore_ascii_case("authorization")
        && let Some(token) = value.strip_prefix("Bearer ")
    {
        let var = proposed_variable(server, None);
        let replacement = Replacement {
            field,
            kind: ReplacementKind::BearerHeader,
            var: var.clone(),
            literal: Literal::new(token),
        };
        let auth = Auth::BearerEnv {
            var,
            oauth_supported: false,
        };
        return Ok((auth, replacement));
    }
    let var = proposed_variable(server, Some(header));
    let replacement = Replacement {
        field,
        kind: ReplacementKind::Header,
        var: var.clone(),
        literal: Literal::new(value),
    };
    let auth = Auth::HeaderEnv {
        header: header.to_owned(),
        var,
    };
    Ok((auth, replacement))
}

/// The current repository: the nearest ancestor of `cwd` (itself included)
/// that contains a `.git` directory or file, falling back to `cwd`. No `git`
/// process is run. Codex 0.165.0 uses the same root for its project trust.
#[must_use]
pub fn repository_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
        .unwrap_or(cwd)
        .to_path_buf()
}

/// Whether an error of `symlink_metadata` means that the path does not
/// exist: `NotFound`, or `NotADirectory` for a parent that is a file. Any
/// other error (such as a directory that cannot be searched) is not
/// absence.
pub(crate) fn is_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `path` exists (a symlink is not followed). `Ok(false)` when it
/// does not (see [`is_absent`]).
///
/// # Errors
///
/// [`Error::Io`] when its metadata cannot be read for another reason.
pub(crate) fn path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if is_absent(&error) => Ok(false),
        Err(error) => Err(Error::io("read", path, &error)),
    }
}

/// Reads a repository file of a harness through the size-capped, no-follow
/// open used for definitions. `Ok(None)` when it does not exist.
///
/// # Errors
///
/// [`Error::HarnessConfigInvalid`] for a symlink, a non-regular file or a
/// file over [`MAX_REPOSITORY_FILE_BYTES`], [`Error::Io`] otherwise, also
/// when its metadata cannot be read for another reason than absence.
pub fn read_repository_file(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path_exists(path)? {
        return Ok(None);
    }
    let invalid = |reason| Error::HarnessConfigInvalid {
        path: path.to_path_buf(),
        reason,
        position: None,
    };
    read_regular_capped(path, MAX_REPOSITORY_FILE_BYTES)
        .map(Some)
        .map_err(|error| match error {
            CappedReadError::NotRegular => invalid("not a regular file"),
            CappedReadError::TooLarge => invalid("larger than 1 MiB"),
            CappedReadError::Io(error) => Error::io("read", path, &error),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn debug_forms_hide_values() {
        let raw = RawEntry::new(json!({"command": "CANARY_COMMAND", "env": {"T": "CANARY"}}));
        let text = format!("{raw:?}");
        assert!(!text.contains("CANARY"), "{text}");
        assert!(text.contains("command"));
        assert_eq!(
            format!("{:?}", Literal::new("CANARY")),
            "Literal(<redacted>)"
        );
        let definition = crate::definition::parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"CANARYCMD\"\nargs = [\"CANARYARG\"]\n",
            "a",
        )
        .unwrap();
        let imported = Imported {
            definition,
            replacements: vec![Replacement {
                field: "env.T".to_owned(),
                kind: ReplacementKind::Env,
                var: "T".to_owned(),
                literal: Literal::new("CANARYLIT"),
            }],
            normalizations: Vec::new(),
        };
        let text = format!("{imported:?}");
        assert!(!text.contains("CANARY"), "{text}");
        assert!(text.contains("env.T"));
        assert_eq!(RawEntry::new(json!(1)).keys(), Vec::<&str>::new());
    }

    #[test]
    fn same_core_compares_replaced_literals_too() {
        let definition = crate::definition::parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n[env]\nT = { forward = true }\n",
            "a",
        )
        .unwrap();
        let with = |literal: &str| Imported {
            definition: definition.clone(),
            replacements: vec![Replacement {
                field: "env.T".to_owned(),
                kind: ReplacementKind::Env,
                var: "T".to_owned(),
                literal: Literal::new(literal),
            }],
            normalizations: Vec::new(),
        };
        assert!(with("a").same_core(&with("a")));
        assert!(!with("a").same_core(&with("b")));
    }

    #[test]
    fn literal_headers_get_proposed_variables() {
        use crate::definition::Auth;
        assert_eq!(proposed_variable("my-api", None), "MY_API_TOKEN");
        assert_eq!(proposed_variable("x", Some("X-Api-Key")), "X_X_API_KEY");
        let (auth, replacement) = literal_header(
            "gh",
            "headers.Authorization".to_owned(),
            "Authorization",
            "Bearer t",
        )
        .unwrap();
        assert_eq!(
            auth,
            Auth::BearerEnv {
                var: "GH_TOKEN".to_owned(),
                oauth_supported: false
            }
        );
        assert_eq!(replacement.kind, ReplacementKind::BearerHeader);
        assert_eq!(replacement.literal.expose(), "t");
        let (auth, replacement) = literal_header(
            "gh",
            "headers.authorization".to_owned(),
            "authorization",
            "Basic b",
        )
        .unwrap();
        assert_eq!(
            auth,
            Auth::HeaderEnv {
                header: "authorization".to_owned(),
                var: "GH_AUTHORIZATION".to_owned()
            }
        );
        assert_eq!(replacement.literal.expose(), "Basic b");
        // A header name that is not shown as it is never reaches a
        // definition or an output: it is a gap on the redacted field.
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let gap =
            literal_header("gh", "headers.<redacted-key>".to_owned(), secret, "v").unwrap_err();
        assert_eq!(
            gap,
            ImportGap::new("headers.<redacted-key>", ImportGapReason::InvalidName)
        );
        assert_eq!(ImportGapReason::InvalidName.as_str(), "invalid-name");
        assert!(carried_name("env.A_B", "A_B", NameKind::Variable).is_ok());
        for key in [secret, "my-key", "1A"] {
            assert_eq!(
                carried_name("env.x", key, NameKind::Variable)
                    .unwrap_err()
                    .reason,
                ImportGapReason::InvalidName,
                "{key}"
            );
        }
        assert!(carried_name("headers.X-Api-Key", "X-Api-Key", NameKind::Header).is_ok());
        assert_eq!(
            carried_name("headers.x", secret, NameKind::Header)
                .unwrap_err()
                .reason,
            ImportGapReason::InvalidName
        );
    }

    #[test]
    fn repository_root_is_the_nearest_git_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let deep = repo.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(repository_root(&deep), deep);
        std::fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(repository_root(&deep), repo);
        // A worktree or submodule has a `.git` file.
        std::fs::write(repo.join("a/.git"), "gitdir: x").unwrap();
        assert_eq!(repository_root(&deep), repo.join("a"));
    }

    #[test]
    fn repository_files_are_capped_and_never_followed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".mcp.json");
        assert_eq!(read_repository_file(&path).unwrap(), None);
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(read_repository_file(&path).unwrap().unwrap(), b"{}");
        std::fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
        assert!(
            read_repository_file(&path)
                .unwrap_err()
                .hint()
                .contains("is larger than 1 MiB")
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            read_repository_file(&path).unwrap_err().code(),
            "harness-config-invalid"
        );
        #[cfg(unix)]
        {
            let link = temp.path().join("link.json");
            std::fs::write(temp.path().join("target.json"), "{}").unwrap();
            std::os::unix::fs::symlink(temp.path().join("target.json"), &link).unwrap();
            assert_eq!(
                read_repository_file(&link).unwrap_err().code(),
                "harness-config-invalid"
            );
        }
    }
}
