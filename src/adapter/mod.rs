//! The adapter contract and the adapter registry.
//!
//! An adapter connects d-mcp to one coding-agent harness. It declares its
//! capabilities (apply mode, scopes, override keys, reserved names, ...),
//! reads the harness's entries, renders resolved definitions into native
//! entries with [`Gap`]s, and applies a [`HarnessPlan`]. The sync engine in
//! [`crate::sync`] depends only on this contract.
//!
//! A native-to-neutral import function is part of the contract as well; it
//! is implemented in WP4, where adoption and duplicate detection need it, so
//! the default [`Adapter::import`] returns `None`.

pub mod codex;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::config::Harness;
use crate::definition::Definition;
use crate::error::Result;
use crate::process::ProcessRunner;
use crate::store::{Environment, StoreWarning};

/// How an adapter writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// d-mcp edits the harness file itself and batches every operation into
    /// one atomic persist (Codex).
    DirectEdit,
    /// d-mcp runs the harness's own CLI, one operation at a time (WP3).
    HarnessCli,
}

/// A configuration scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    /// The user's global configuration.
    User,
}

impl Scope {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
        }
    }

    /// Parses the stable string form.
    #[must_use]
    pub fn from_code(text: &str) -> Option<Self> {
        (text == "user").then_some(Scope::User)
    }
}

/// How serious a [`Gap`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapSeverity {
    /// Informational; the entry is written.
    Note,
    /// The entry is blocked and not written.
    Important,
}

/// Something a definition carries that a harness cannot express, or a manual
/// step it needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gap {
    /// The definition field, such as `auth` or `harness.codex.disabled_tools`,
    /// or a harness-local preserved key an Update removes, such as
    /// `oauth.client_secret`.
    pub field: String,
    /// The severity.
    pub severity: GapSeverity,
    /// A human-readable reason, built from names only.
    pub reason: String,
    /// A harness-local preserved key that satisfies this gap when present in
    /// the existing entry (the manual step was done). Not serialized.
    #[serde(skip)]
    pub unless_present: Option<&'static str>,
}

/// A rendered native entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// The native entry: keys in writing order; object values become
    /// sub-tables or nested objects.
    pub entry: Vec<(String, Value)>,
    /// The hash of the entry as sent.
    pub sent_hash: String,
    /// The gaps of the rendering.
    pub gaps: Vec<Gap>,
}

/// The largest timeout override, in seconds: 365 days. Larger values (such
/// as `1e300`) cannot be converted into a duration by Codex, which then
/// refuses to load its whole configuration.
pub const MAX_TIMEOUT_SECONDS: u32 = 31_536_000;

/// The type a `harness.<id>` override key must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrideType {
    /// An absolute path string, not starting with `~`.
    AbsolutePath,
    /// A boolean.
    Boolean,
    /// A finite, non-negative integer or float of seconds, at most
    /// [`MAX_TIMEOUT_SECONDS`].
    Seconds,
    /// An array of strings.
    StringArray,
    /// A non-empty array of strings.
    NonEmptyStringArray,
}

impl OverrideType {
    /// A short description for messages and docs.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            OverrideType::AbsolutePath => "absolute path string",
            OverrideType::Boolean => "boolean",
            OverrideType::Seconds => "non-negative number of seconds, at most 31536000",
            OverrideType::StringArray => "array of strings",
            OverrideType::NonEmptyStringArray => "non-empty array of strings",
        }
    }
}

/// An allowed override key and its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverrideKey {
    /// The key.
    pub key: &'static str,
    /// Its type.
    pub kind: OverrideType,
    /// Whether the key is only valid for a stdio transport.
    pub stdio_only: bool,
}

/// The login state of a harness entry, as reported by the harness itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoginState {
    /// OAuth tokens are present.
    LoggedIn,
    /// OAuth is configured but the user has not logged in.
    NeedsLogin,
    /// The entry does not use OAuth (stdio, bearer token, ...).
    NotApplicable,
    /// The state could not be determined.
    Unknown,
}

/// Where a harness keeps its configuration for one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The scope.
    pub scope: Scope,
    /// The configuration file, as resolved *before* following symlinks.
    pub file: PathBuf,
    /// Whether the file exists (as any file-system entry, a dangling link
    /// included).
    pub file_exists: bool,
    /// Whether the harness binary was found on `PATH` (not executed).
    pub binary_found: bool,
}

impl Location {
    /// Whether the harness counts as installed: its binary or its file
    /// exists.
    #[must_use]
    pub fn present(&self) -> bool {
        self.file_exists || self.binary_found
    }
}

/// An entry as read from the harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualEntry {
    /// The semantic hash of the entry as read back. Never shown for an entry
    /// d-mcp does not own.
    pub hash: String,
    /// Whether the entry is written in a style d-mcp can edit.
    pub supported_style: bool,
    /// Harness-local preserved keys present in the entry (names only).
    pub preserved: BTreeSet<&'static str>,
}

/// What an adapter read from one harness location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessRead {
    /// The location.
    pub location: Location,
    /// The entries by name.
    pub entries: BTreeMap<String, ActualEntry>,
    /// Whether the root that holds the entries can be edited; `false` makes
    /// the whole harness unsupported for writing.
    pub root_supported: bool,
    /// Adapter-private state needed to apply a plan (for a direct-edit
    /// adapter: the original bytes, the resolved target and its mode). It
    /// never leaves memory.
    pub private: PrivateRead,
}

/// Adapter-private read state of a direct-edit adapter.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PrivateRead {
    /// The file the bytes were read from, after following symlinks; `None`
    /// when the file does not exist yet.
    pub target: Option<PathBuf>,
    /// The original bytes.
    pub bytes: Vec<u8>,
    /// The original permission bits.
    pub mode: Option<u32>,
    /// The original owner and group ids (Unix).
    pub owner: Option<(u32, u32)>,
}

impl std::fmt::Debug for PrivateRead {
    /// The bytes can hold foreign secrets; they are never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateRead")
            .field("target", &self.target)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .finish()
    }
}

/// The kind of a write operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteKind {
    /// Add a new entry.
    Add,
    /// Replace an owned entry.
    Update,
    /// Remove an owned entry.
    Remove,
}

impl WriteKind {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WriteKind::Add => "add",
            WriteKind::Update => "update",
            WriteKind::Remove => "remove",
        }
    }
}

/// One write operation of a [`HarnessPlan`].
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOp {
    /// The entry name.
    pub name: String,
    /// The kind.
    pub kind: WriteKind,
    /// The rendering, for Add and Update.
    pub rendered: Option<Rendered>,
}

/// The write operations for one harness location.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessPlan {
    /// The operations, sorted by name.
    pub ops: Vec<WriteOp>,
}

/// The intent recorded before a write, so a crash cannot lose ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// The entry name.
    pub name: String,
    /// The kind.
    pub kind: WriteKind,
    /// The expected read-back hash after the write; `None` for Remove.
    pub expected_readback: Option<String>,
}

/// Records intents durably before an adapter persists anything.
pub trait Journal {
    /// Writes `pending` records for the intents.
    ///
    /// # Errors
    ///
    /// Any store error; the adapter then writes nothing.
    fn intend(&mut self, intents: &[Intent]) -> Result<()>;
}

/// The verified result of one write operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryResult {
    /// The entry name.
    pub name: String,
    /// The read-back hash after the write; `None` after a Remove.
    pub readback: Option<String>,
}

/// The result of [`Adapter::apply`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyResult {
    /// One result per operation.
    pub entries: Vec<EntryResult>,
    /// Non-fatal conditions to report.
    pub warnings: Vec<StoreWarning>,
    /// Whether the harness file was created.
    pub created: bool,
}

/// Test seams of [`Adapter::apply`], for failure injection. Each runs at the
/// named step and can abort it with an error. Not part of the stable API.
#[doc(hidden)]
#[derive(Default)]
pub struct Hooks<'a> {
    /// Rewrites the spliced text before it is checked, to exercise the
    /// pre-write checks of a direct-edit adapter.
    pub after_splice: Option<&'a dyn Fn(&str) -> String>,
    /// Runs inside the persist seam, before the concurrency re-check.
    pub before_persist: Option<&'a dyn Fn() -> Result<()>>,
    /// Runs after the persist and before verification. An error stops the
    /// run at once, as a crash would.
    pub after_persist: Option<&'a dyn Fn() -> Result<()>>,
    /// Makes the read-back verification fail, to exercise the restore.
    pub force_verify_failure: bool,
}

/// The adapter contract.
pub trait Adapter: Sync {
    /// The harness this adapter writes.
    fn harness(&self) -> Harness;
    /// The apply mode.
    fn apply_mode(&self) -> ApplyMode;
    /// The scopes it can discover.
    fn discover_scopes(&self) -> &'static [Scope];
    /// The scopes it can write.
    fn write_scopes(&self) -> &'static [Scope];
    /// Whether the rendered (sent) hash and the read-back hash of the same
    /// entry are comparable.
    fn hashes_comparable(&self) -> bool;
    /// The allowed `harness.<id>` override keys with their types.
    fn override_keys(&self) -> &'static [OverrideKey];
    /// Harness keys deliberately excluded from the override allow-list.
    fn excluded_override_keys(&self) -> &'static [&'static str];
    /// Keys of *other* harnesses' override blocks that are security-relevant
    /// for this harness: when this harness cannot honour them, rendering
    /// reports an `important` gap.
    fn foreign_security_keys(&self) -> &'static [(Harness, &'static str)];
    /// Entry names the harness reserves.
    fn reserved_names(&self) -> &'static [&'static str];
    /// Whether entries can disappear without anyone removing them.
    fn entries_can_vanish(&self) -> bool;
    /// The harness versions this adapter was verified against.
    fn verified_versions(&self) -> &'static [&'static str];
    /// Resolves where the harness keeps the scope's configuration. Never
    /// creates anything.
    ///
    /// # Errors
    ///
    /// Environment errors, such as an unset `HOME`.
    fn locate(&self, env: &Environment, scope: Scope) -> Result<Location>;
    /// Reads the entries at a location.
    ///
    /// # Errors
    ///
    /// The harness's configuration errors.
    fn read(&self, location: &Location) -> Result<HarnessRead>;
    /// The harness-local preserved keys among `present` that no longer fit
    /// `rendered`: an Update removes them instead of carrying them over, and
    /// the sync engine reports each as a `note` gap. Names only.
    fn dropped_preserved(
        &self,
        _rendered: &Rendered,
        _present: &BTreeSet<&'static str>,
    ) -> Vec<&'static str> {
        Vec::new()
    }
    /// Renders a resolved definition into a native entry. Gaps for foreign
    /// security keys are added by the sync engine.
    fn render(&self, definition: &Definition) -> Rendered;
    /// Applies a plan: records intents in the journal, writes, verifies and
    /// returns one result per operation.
    ///
    /// # Errors
    ///
    /// Concurrency, persist and verification errors. On an error nothing
    /// has been changed, or the original was restored, unless the error
    /// says otherwise.
    fn apply(
        &self,
        read: &HarnessRead,
        plan: &HarnessPlan,
        journal: &mut dyn Journal,
        hooks: &Hooks<'_>,
    ) -> Result<ApplyResult>;
    /// Reads the login state of the named entries from the harness's own
    /// listing. Never reads token storage; `sync` never calls it.
    fn login_state(
        &self,
        env: &Environment,
        runner: &ProcessRunner,
        names: &[String],
    ) -> BTreeMap<String, LoginState>;
    /// Imports a native entry as a neutral definition (WP4).
    fn import(&self, _read: &HarnessRead, _name: &str) -> Option<Definition> {
        None
    }
}

/// The registry of adapters, in a stable order.
#[must_use]
pub fn registry() -> &'static [&'static dyn Adapter] {
    static ADAPTERS: [&dyn Adapter; 1] = [&codex::CodexAdapter];
    &ADAPTERS
}

/// Looks an adapter up by harness.
#[must_use]
pub fn adapter_for(harness: Harness) -> Option<&'static dyn Adapter> {
    registry()
        .iter()
        .copied()
        .find(|adapter| adapter.harness() == harness)
}

/// The important gaps an adapter reports for security-relevant keys of other
/// harnesses' override blocks that the definition sets.
#[must_use]
pub fn foreign_security_gaps(adapter: &dyn Adapter, definition: &Definition) -> Vec<Gap> {
    adapter
        .foreign_security_keys()
        .iter()
        .filter(|(harness, key)| {
            definition
                .overrides
                .get(harness.as_str())
                .is_some_and(|table| table.contains_key(*key))
        })
        .map(|(harness, key)| Gap {
            field: format!("harness.{}.{key}", harness.as_str()),
            severity: GapSeverity::Important,
            reason: format!(
                "{} cannot enforce `{key}` from the {} override block",
                adapter.harness().as_str(),
                harness.as_str()
            ),
            unless_present: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::parse_definition;

    #[test]
    fn registry_and_lookup() {
        assert_eq!(registry().len(), 1);
        assert!(adapter_for(Harness::Codex).is_some());
        assert!(adapter_for(Harness::Claude).is_none());
        assert_eq!(Scope::from_code("user"), Some(Scope::User));
        assert_eq!(Scope::from_code("project"), None);
        assert_eq!(Scope::User.as_str(), "user");
        assert_eq!(WriteKind::Remove.as_str(), "remove");
        assert_eq!(OverrideType::StringArray.describe(), "array of strings");
        assert_eq!(
            OverrideType::NonEmptyStringArray.describe(),
            "non-empty array of strings"
        );
        assert!(OverrideType::Seconds.describe().contains("31536000"));
        assert_eq!(
            OverrideType::Seconds.describe(),
            format!("non-negative number of seconds, at most {MAX_TIMEOUT_SECONDS}")
        );
        let location = Location {
            scope: Scope::User,
            file: PathBuf::from("/c"),
            file_exists: false,
            binary_found: false,
        };
        assert!(!location.present());
    }

    #[test]
    fn private_read_debug_hides_bytes() {
        let read = PrivateRead {
            target: None,
            bytes: b"ghp_secret".to_vec(),
            mode: None,
            owner: None,
        };
        let text = format!("{read:?}");
        assert!(!text.contains("ghp_secret"), "{text}");
        assert!(text.contains("<10 bytes>"));
    }

    #[test]
    fn codex_imports_nothing_until_wp4() {
        let adapter = adapter_for(Harness::Codex).unwrap();
        let read = HarnessRead {
            location: Location {
                scope: Scope::User,
                file: PathBuf::from("/c"),
                file_exists: false,
                binary_found: false,
            },
            entries: BTreeMap::new(),
            root_supported: true,
            private: PrivateRead::default(),
        };
        assert!(adapter.import(&read, "x").is_none());
        let definition = parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n",
            "a",
        )
        .unwrap();
        assert!(foreign_security_gaps(adapter, &definition).is_empty());
    }
}
