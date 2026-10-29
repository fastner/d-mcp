//! The write state in `<store>/state.toml`: which harness entries d-mcp owns,
//! with their hashes, and the intents of writes in flight.
//!
//! A record is keyed by harness, scope and name. It is either `owned` or
//! `pending`. A `pending` record is written under the store lock before a
//! harness file is persisted; it holds the expected hashes and the previous
//! owned hashes, so a crash between the harness write and the state write
//! never loses ownership. The next sync promotes or reverts it.
//!
//! Like `config.toml`, the file is validated strictly; any deviation makes it
//! corrupt (`state-corrupt`, exit 3), and d-mcp never rewrites a corrupt file.
//! This module reads `state.toml` itself and writes it only through
//! [`crate::store`] (atomic, under the store lock).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Table, value};

use crate::adapter::{Scope, WriteKind};
use crate::canonical::is_digest;
use crate::config::{Harness, SCHEMA_VERSION};
use crate::error::{Error, Result};
use crate::spanned::{self, Entry, NodeKind};
use crate::store::{self, StoreLock, StorePaths, StoreWarning};

/// The key of a record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    /// The harness.
    pub harness: Harness,
    /// The scope.
    pub scope: Scope,
    /// The entry name.
    pub name: String,
}

/// The hashes of an owned entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashes {
    /// The hash of the entry as sent (rendered).
    pub sent: String,
    /// The semantic hash of the entry as read back.
    pub readback: String,
    /// The hash of the source definition (WP5 binds approvals to it).
    pub definition_hash: String,
    /// Provenance: whether d-mcp itself wrote the entry with these hashes.
    /// Set on every promoted d-mcp write; cleared by a take-over (and by WP4
    /// adoption). A harness-CLI adapter rolls back only entries d-mcp wrote,
    /// so foreign content never reaches a command line.
    pub written: bool,
}

/// The status of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordState {
    /// d-mcp owns the entry.
    Owned(Hashes),
    /// A write is in flight.
    Pending {
        /// The write.
        op: WriteKind,
        /// The expected hashes after the write; `None` for Remove.
        expected: Option<Hashes>,
        /// The previous owned hashes; `None` for an Add of a new entry.
        previous: Option<Hashes>,
    },
}

/// The scope an adopted entry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginScope {
    /// The harness's user scope (the write scope).
    User,
    /// The project scope of a repository; the origin entry is left there.
    Project {
        /// The absolute repository root.
        root: PathBuf,
    },
}

/// Where `d-mcp adopt` took an entry from (WP4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedFrom {
    /// The harness of the origin entry.
    pub harness: Harness,
    /// The scope of the origin entry.
    pub scope: OriginScope,
}

impl AdoptedFrom {
    /// `<harness>:<scope>`, such as `codex:user`.
    #[must_use]
    pub fn label(&self) -> String {
        let scope = match self.scope {
            OriginScope::User => "user",
            OriginScope::Project { .. } => "project",
        };
        format!("{}:{scope}", self.harness.as_str())
    }
}

/// A native field whose literal adoption replaced with a variable reference.
/// Names only: the literal is never stored.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReplacedField {
    /// The native field, such as `env.TOKEN` or `headers.Authorization`.
    pub field: String,
    /// The variable the entry reads instead.
    pub var: String,
}

/// What a record keeps across its hash changes: the adoption provenance and
/// the vanish history. Carried over by every take-over, refresh, write and
/// settlement; lost only when the record is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecordMeta {
    /// Where `d-mcp adopt` took the entry from; `None` for entries d-mcp
    /// added or `sync --take-over` accepted.
    pub adopted_from: Option<AdoptedFrom>,
    /// The fields whose literals adoption replaced, sorted. Only with
    /// `adopted_from`.
    pub replaced_fields: Vec<ReplacedField>,
    /// How often a `vanished` re-add of the entry was promoted. Never reset.
    pub vanish_count: u64,
    /// When the last `vanished` re-add was promoted, in Unix epoch seconds;
    /// set exactly when `vanish_count` is not 0.
    pub last_vanish: Option<u64>,
}

impl RecordMeta {
    /// Counts one promoted `vanished` re-add, now.
    pub fn count_vanish(&mut self) {
        self.vanish_count = self.vanish_count.saturating_add(1);
        self.last_vanish = Some(now());
    }
}

/// A write-state record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The file, as resolved before following symlinks.
    pub file: PathBuf,
    /// The time of the last change, in Unix epoch seconds.
    pub time: u64,
    /// The d-mcp version that wrote the record.
    pub version: String,
    /// The status.
    pub state: RecordState,
    /// The adoption provenance and the vanish history.
    pub meta: RecordMeta,
}

impl Record {
    /// A new owned record stamped with the current time and version, without
    /// provenance or history.
    #[must_use]
    pub fn owned(file: PathBuf, hashes: Hashes) -> Self {
        Self::owned_with(file, hashes, RecordMeta::default())
    }

    /// A new owned record stamped with the current time and version that
    /// keeps `meta` (of the record it replaces).
    #[must_use]
    pub fn owned_with(file: PathBuf, hashes: Hashes, meta: RecordMeta) -> Self {
        Self {
            file,
            time: now(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            state: RecordState::Owned(hashes),
            meta,
        }
    }

    /// The owned hashes, if the record is owned.
    #[must_use]
    pub fn hashes(&self) -> Option<&Hashes> {
        match &self.state {
            RecordState::Owned(hashes) => Some(hashes),
            RecordState::Pending { .. } => None,
        }
    }
}

/// The current time in Unix epoch seconds.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// The write state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    /// The records.
    pub records: BTreeMap<RecordKey, Record>,
}

/// How a pending record was settled against the actual entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// The write took effect; the record is owned with the expected hashes.
    Promoted,
    /// The write did not take effect; the previous owned record is back.
    Reverted,
    /// An Add that never took effect, or a completed Remove; the record is
    /// gone.
    Dropped,
}

/// Settles a pending record against the actual read-back hash of its entry
/// (`None` when the entry is absent). Returns the new record, if any, and
/// what happened. A promoted `vanished` re-add (an Add with previous hashes)
/// counts in the record's vanish history.
#[must_use]
pub fn settle(record: &Record, actual: Option<&str>) -> (Option<Record>, Settled) {
    let RecordState::Pending {
        op,
        expected,
        previous,
    } = &record.state
    else {
        return (Some(record.clone()), Settled::Promoted);
    };
    let with = |hashes: &Hashes| Record {
        state: RecordState::Owned(hashes.clone()),
        ..record.clone()
    };
    let took_effect = match (op, expected, actual) {
        (WriteKind::Remove, _, None) => return (None, Settled::Dropped),
        (_, Some(expected), Some(actual)) => expected.readback == actual,
        _ => false,
    };
    if took_effect && let Some(expected) = expected {
        let mut promoted = with(expected);
        if *op == WriteKind::Add && previous.is_some() {
            promoted.meta.count_vanish();
        }
        return (Some(promoted), Settled::Promoted);
    }
    match previous {
        Some(previous) => (Some(with(previous)), Settled::Reverted),
        // An Add of a new entry that did not take effect drops the record:
        // an absent entry is added again, and a same-name entry with other
        // content was not written by d-mcp, so it is reported as a
        // Collision, never owned.
        None => (None, Settled::Dropped),
    }
}

/// Why `state.toml` could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StateError {
    Corrupt {
        field: String,
        offset: Option<usize>,
    },
    Schema(i64),
}

type Parse<T> = std::result::Result<T, StateError>;

fn corrupt<T>(field: &str, offset: Option<usize>) -> Parse<T> {
    Err(StateError::Corrupt {
        field: field.to_owned(),
        offset,
    })
}

const RECORD_KEYS: &[&str] = &[
    "harness",
    "scope",
    "name",
    "file",
    "status",
    "op",
    "sent",
    "readback",
    "definition_hash",
    "prev_sent",
    "prev_readback",
    "prev_definition_hash",
    "written",
    "prev_written",
    "adopted_from",
    "replaced_fields",
    "vanish_count",
    "last_vanish",
    "time",
    "version",
];

/// The keys of an `adopted_from` table.
const ORIGIN_KEYS: &[&str] = &["harness", "scope", "root"];
/// The keys of a `replaced_fields` item.
const REPLACED_KEYS: &[&str] = &["field", "var"];

fn find<'e>(entries: &'e [Entry], key: &str) -> Option<&'e Entry> {
    entries.iter().find(|entry| entry.key == key)
}

fn string<'e>(
    entries: &'e [Entry],
    key: &str,
    field: &str,
    parent: Option<usize>,
) -> Parse<&'e str> {
    let Some(entry) = find(entries, key) else {
        return corrupt(field, parent);
    };
    match &entry.value.kind {
        NodeKind::String(text) => Ok(text),
        _ => corrupt(field, entry.value_offset()),
    }
}

fn optional_hashes(
    entries: &[Entry],
    prefix: &str,
    field: &str,
    parent: Option<usize>,
) -> Parse<Option<Hashes>> {
    let keys = ["sent", "readback", "definition_hash"].map(|key| format!("{prefix}{key}"));
    let present = keys
        .iter()
        .filter(|key| find(entries, key).is_some())
        .count();
    let written_key = format!("{prefix}written");
    let written = match find(entries, &written_key) {
        None => false,
        Some(entry) => match entry.value.kind {
            // Only `true` is ever written; `false` is the absence.
            NodeKind::Boolean(true) if present > 0 => true,
            _ => return corrupt(&format!("{field}.{written_key}"), entry.value_offset()),
        },
    };
    if present == 0 {
        return Ok(None);
    }
    let mut values = Vec::new();
    for key in &keys {
        let text = string(entries, key, &format!("{field}.{key}"), parent)?;
        if !is_digest(text) {
            return corrupt(
                &format!("{field}.{key}"),
                find(entries, key).and_then(Entry::value_offset),
            );
        }
        values.push(text.to_owned());
    }
    let [sent, readback, definition_hash] =
        <[String; 3]>::try_from(values).map_err(|_| StateError::Corrupt {
            field: field.to_owned(),
            offset: parent,
        })?;
    Ok(Some(Hashes {
        sent,
        readback,
        definition_hash,
        written,
    }))
}

fn non_negative(entries: &[Entry], key: &str, field: &str) -> Parse<Option<u64>> {
    match find(entries, key) {
        None => Ok(None),
        Some(entry) => match entry.value.kind {
            NodeKind::Integer(number) => u64::try_from(number).map(Some).map_or_else(
                |_| corrupt(&format!("{field}.{key}"), entry.value_offset()),
                Ok,
            ),
            _ => corrupt(&format!("{field}.{key}"), entry.value_offset()),
        },
    }
}

fn parse_origin(entry: &Entry, field: &str) -> Parse<AdoptedFrom> {
    let field = format!("{field}.adopted_from");
    let offset = entry.value_offset();
    let NodeKind::Table(items) = &entry.value.kind else {
        return corrupt(&field, offset);
    };
    if let Some(item) = items
        .iter()
        .find(|item| !ORIGIN_KEYS.contains(&item.key.as_str()))
    {
        return corrupt(&field, item.key_offset());
    }
    let harness = match string(items, "harness", &format!("{field}.harness"), offset)? {
        "codex" => Harness::Codex,
        "claude" => Harness::Claude,
        _ => return corrupt(&format!("{field}.harness"), offset),
    };
    let root = match find(items, "root") {
        None => None,
        Some(_) => Some(PathBuf::from(string(
            items,
            "root",
            &format!("{field}.root"),
            offset,
        )?)),
    };
    let scope = match (
        string(items, "scope", &format!("{field}.scope"), offset)?,
        root,
    ) {
        ("user", None) => OriginScope::User,
        ("project", Some(root)) if root.is_absolute() => OriginScope::Project { root },
        _ => return corrupt(&format!("{field}.scope"), offset),
    };
    Ok(AdoptedFrom { harness, scope })
}

fn parse_replaced(entry: &Entry, field: &str) -> Parse<Vec<ReplacedField>> {
    let field = format!("{field}.replaced_fields");
    let NodeKind::Array(items) = &entry.value.kind else {
        return corrupt(&field, entry.value_offset());
    };
    let mut replaced = Vec::new();
    for (position, item) in items.iter().enumerate() {
        let item_field = format!("{field}[{position}]");
        let NodeKind::Table(pairs) = &item.kind else {
            return corrupt(&item_field, item.offset());
        };
        if pairs
            .iter()
            .any(|pair| !REPLACED_KEYS.contains(&pair.key.as_str()))
        {
            return corrupt(&item_field, item.offset());
        }
        let name = string(
            pairs,
            "field",
            &format!("{item_field}.field"),
            item.offset(),
        )?;
        let var = string(pairs, "var", &format!("{item_field}.var"), item.offset())?;
        if name.is_empty() || !crate::definition::is_valid_variable_name(var) {
            return corrupt(&item_field, item.offset());
        }
        replaced.push(ReplacedField {
            field: name.to_owned(),
            var: var.to_owned(),
        });
    }
    // Written sorted and non-empty; anything else is not d-mcp's.
    if replaced.is_empty() || !replaced.is_sorted() {
        return corrupt(&field, entry.value_offset());
    }
    Ok(replaced)
}

fn parse_meta(entries: &[Entry], field: &str, offset: Option<usize>) -> Parse<RecordMeta> {
    let adopted_from = find(entries, "adopted_from")
        .map(|entry| parse_origin(entry, field))
        .transpose()?;
    let replaced_fields = match find(entries, "replaced_fields") {
        None => Vec::new(),
        // Replaced fields belong to an adoption.
        Some(entry) if adopted_from.is_some() => parse_replaced(entry, field)?,
        Some(entry) => return corrupt(&format!("{field}.replaced_fields"), entry.key_offset()),
    };
    let vanish_count = non_negative(entries, "vanish_count", field)?;
    let last_vanish = non_negative(entries, "last_vanish", field)?;
    let (vanish_count, last_vanish) = match (vanish_count, last_vanish) {
        (None, None) => (0, None),
        // Only a positive count is written, always with its time.
        (Some(count), Some(time)) if count > 0 => (count, Some(time)),
        _ => return corrupt(&format!("{field}.vanish_count"), offset),
    };
    Ok(RecordMeta {
        adopted_from,
        replaced_fields,
        vanish_count,
        last_vanish,
    })
}

fn parse_record(
    entries: &[Entry],
    field: &str,
    offset: Option<usize>,
) -> Parse<(RecordKey, Record)> {
    if let Some(entry) = entries
        .iter()
        .find(|e| !RECORD_KEYS.contains(&e.key.as_str()))
    {
        return corrupt(field, entry.key_offset());
    }
    let position = |key: &str| find(entries, key).and_then(Entry::value_offset);
    let harness = match string(entries, "harness", &format!("{field}.harness"), offset)? {
        "codex" => Harness::Codex,
        "claude" => Harness::Claude,
        _ => return corrupt(&format!("{field}.harness"), position("harness")),
    };
    let scope = Scope::from_code(string(entries, "scope", &format!("{field}.scope"), offset)?)
        .map_or_else(|| corrupt(&format!("{field}.scope"), position("scope")), Ok)?;
    let name = string(entries, "name", &format!("{field}.name"), offset)?.to_owned();
    if !crate::definition::is_valid_name(&name) {
        return corrupt(&format!("{field}.name"), position("name"));
    }
    let file = PathBuf::from(string(entries, "file", &format!("{field}.file"), offset)?);
    if !file.is_absolute() {
        return corrupt(&format!("{field}.file"), position("file"));
    }
    let version = string(entries, "version", &format!("{field}.version"), offset)?.to_owned();
    let time = match find(entries, "time").map(|entry| &entry.value.kind) {
        Some(NodeKind::Integer(time)) => u64::try_from(*time)
            .map_or_else(|_| corrupt(&format!("{field}.time"), position("time")), Ok)?,
        _ => return corrupt(&format!("{field}.time"), position("time").or(offset)),
    };
    let current = optional_hashes(entries, "", field, offset)?;
    let previous = optional_hashes(entries, "prev_", field, offset)?;
    let meta = parse_meta(entries, field, offset)?;
    let status = string(entries, "status", &format!("{field}.status"), offset)?;
    let op = match find(entries, "op") {
        None => None,
        Some(_) => Some(
            match string(entries, "op", &format!("{field}.op"), offset)? {
                "add" => WriteKind::Add,
                "update" => WriteKind::Update,
                "remove" => WriteKind::Remove,
                _ => return corrupt(&format!("{field}.op"), position("op")),
            },
        ),
    };
    let state = match (status, op) {
        ("owned", None) if previous.is_none() => match current {
            Some(hashes) => RecordState::Owned(hashes),
            None => return corrupt(&format!("{field}.sent"), offset),
        },
        ("pending", Some(op)) => {
            let shape_ok = match op {
                // An Add with previous hashes re-adds a vanished entry.
                WriteKind::Add => current.is_some(),
                WriteKind::Update => current.is_some() && previous.is_some(),
                WriteKind::Remove => current.is_none() && previous.is_some(),
            };
            if !shape_ok {
                return corrupt(&format!("{field}.op"), position("op"));
            }
            RecordState::Pending {
                op,
                expected: current,
                previous,
            }
        }
        _ => return corrupt(&format!("{field}.status"), position("status")),
    };
    Ok((
        RecordKey {
            harness,
            scope,
            name,
        },
        Record {
            file,
            time,
            version,
            state,
            meta,
        },
    ))
}

fn parse_state(text: &str) -> Parse<State> {
    let root = spanned::parse_document(text).map_err(|offset| StateError::Corrupt {
        field: String::new(),
        offset,
    })?;
    let Some(version) = find(&root, "schema_version") else {
        return corrupt("schema_version", None);
    };
    let NodeKind::Integer(found) = version.value.kind else {
        return corrupt("schema_version", version.value_offset());
    };
    if found != SCHEMA_VERSION {
        return Err(StateError::Schema(found));
    }
    if let Some(entry) = root
        .iter()
        .find(|entry| !["schema_version", "records"].contains(&entry.key.as_str()))
    {
        return corrupt("", entry.key_offset());
    }
    let mut state = State::default();
    let Some(records) = find(&root, "records") else {
        return Ok(state);
    };
    let NodeKind::Array(items) = &records.value.kind else {
        return corrupt("records", records.value_offset());
    };
    for (position, item) in items.iter().enumerate() {
        let field = format!("records[{position}]");
        let NodeKind::Table(entries) = &item.kind else {
            return corrupt(&field, item.offset());
        };
        let (key, record) = parse_record(entries, &field, item.offset())?;
        if state.records.insert(key, record).is_some() {
            return corrupt(&field, item.offset());
        }
    }
    Ok(state)
}

fn put_meta(table: &mut Table, meta: &RecordMeta) {
    if let Some(origin) = &meta.adopted_from {
        let mut inline = InlineTable::new();
        inline.insert("harness", origin.harness.as_str().into());
        match &origin.scope {
            OriginScope::User => {
                inline.insert("scope", "user".into());
            }
            OriginScope::Project { root } => {
                inline.insert("scope", "project".into());
                // `save` refuses a non-UTF-8 root, so this is never lossy.
                inline.insert("root", root.to_string_lossy().as_ref().into());
            }
        }
        table["adopted_from"] = value(inline);
    }
    if !meta.replaced_fields.is_empty() {
        let mut array = Array::new();
        for replaced in &meta.replaced_fields {
            let mut inline = InlineTable::new();
            inline.insert("field", replaced.field.as_str().into());
            inline.insert("var", replaced.var.as_str().into());
            array.push(inline);
        }
        table["replaced_fields"] = value(array);
    }
    if let (count @ 1.., Some(time)) = (meta.vanish_count, meta.last_vanish) {
        table["vanish_count"] = value(i64::try_from(count).unwrap_or(i64::MAX));
        table["last_vanish"] = value(i64::try_from(time).unwrap_or(i64::MAX));
    }
}

fn put_hashes(table: &mut Table, prefix: &str, hashes: &Hashes) {
    table[&format!("{prefix}sent")] = value(hashes.sent.as_str());
    table[&format!("{prefix}readback")] = value(hashes.readback.as_str());
    table[&format!("{prefix}definition_hash")] = value(hashes.definition_hash.as_str());
    if hashes.written {
        table[&format!("{prefix}written")] = value(true);
    }
}

impl State {
    /// Parses `state.toml` text.
    ///
    /// # Errors
    ///
    /// [`Error::StateCorrupt`] or [`Error::SchemaUnsupported`], naming `path`.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        parse_state(text).map_err(|error| match error {
            StateError::Corrupt { field, offset } => {
                let (line, column) = spanned::line_column(text, offset.unwrap_or(0));
                Error::StateCorrupt {
                    path: path.to_path_buf(),
                    line,
                    column,
                    field,
                }
            }
            StateError::Schema(found) => Error::SchemaUnsupported {
                path: path.to_path_buf(),
                found,
            },
        })
    }

    /// Renders the canonical text, records sorted by harness, scope and name.
    #[must_use]
    pub fn render(&self) -> String {
        let mut document = DocumentMut::new();
        document["schema_version"] = value(SCHEMA_VERSION);
        if !self.records.is_empty() {
            let mut array = ArrayOfTables::new();
            for (key, record) in &self.records {
                let mut table = Table::new();
                table["harness"] = value(key.harness.as_str());
                table["scope"] = value(key.scope.as_str());
                table["name"] = value(key.name.as_str());
                // `save` refuses a non-UTF-8 path, so this is never lossy.
                table["file"] = value(record.file.to_string_lossy().as_ref());
                match &record.state {
                    RecordState::Owned(hashes) => {
                        table["status"] = value("owned");
                        put_hashes(&mut table, "", hashes);
                    }
                    RecordState::Pending {
                        op,
                        expected,
                        previous,
                    } => {
                        table["status"] = value("pending");
                        table["op"] = value(op.as_str());
                        if let Some(expected) = expected {
                            put_hashes(&mut table, "", expected);
                        }
                        if let Some(previous) = previous {
                            put_hashes(&mut table, "prev_", previous);
                        }
                    }
                }
                put_meta(&mut table, &record.meta);
                table["time"] = value(i64::try_from(record.time).unwrap_or(i64::MAX));
                table["version"] = value(record.version.as_str());
                array.push(table);
            }
            document["records"] = toml_edit::Item::ArrayOfTables(array);
        }
        document.to_string()
    }

    /// The records of one harness.
    pub fn of(&self, harness: Harness) -> impl Iterator<Item = (&RecordKey, &Record)> {
        self.records
            .iter()
            .filter(move |(key, _)| key.harness == harness)
    }
}

/// Loads `state.toml`; a missing file is an empty state.
///
/// # Errors
///
/// [`Error::StateCorrupt`] (also for a file that is not UTF-8),
/// [`Error::SchemaUnsupported`] and [`Error::Io`].
pub fn load(paths: &StorePaths) -> Result<State> {
    let bytes = match std::fs::read(&paths.state) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(State::default());
        }
        Err(error) => return Err(Error::io("read", &paths.state, &error)),
    };
    let text = String::from_utf8(bytes).map_err(|_| Error::StateCorrupt {
        path: paths.state.clone(),
        line: 1,
        column: 1,
        field: String::new(),
    })?;
    State::parse(&text, &paths.state)
}

/// Writes `state.toml` atomically. Call only while holding the lock.
///
/// # Errors
///
/// [`Error::HarnessPathNotUtf8`] when a record's file is not valid UTF-8
/// (paths are never stored lossily), and [`Error::Io`] when the write fails;
/// the old file then stays intact in both cases.
pub fn save(paths: &StorePaths, _lock: &StoreLock, state: &State) -> Result<Option<StoreWarning>> {
    let lossy = |record: &Record| {
        record.file.to_str().is_none()
            || matches!(
                &record.meta.adopted_from,
                Some(AdoptedFrom { scope: OriginScope::Project { root }, .. }) if root.to_str().is_none()
            )
    };
    if let Some((key, _)) = state.records.iter().find(|(_, record)| lossy(record)) {
        return Err(Error::HarnessPathNotUtf8 {
            harness: key.harness.as_str(),
        });
    }
    store::write_atomic(&paths.state, state.render().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(seed: char) -> Hashes {
        Hashes {
            sent: seed.to_string().repeat(64),
            readback: seed.to_string().repeat(64),
            definition_hash: "f".repeat(64),
            written: seed != 'a',
        }
    }

    fn key(name: &str) -> RecordKey {
        RecordKey {
            harness: Harness::Codex,
            scope: Scope::User,
            name: name.to_owned(),
        }
    }

    fn adopted() -> RecordMeta {
        RecordMeta {
            adopted_from: Some(AdoptedFrom {
                harness: Harness::Claude,
                scope: OriginScope::Project {
                    root: PathBuf::from("/work/app"),
                },
            }),
            replaced_fields: vec![
                ReplacedField {
                    field: "env.A".to_owned(),
                    var: "A".to_owned(),
                },
                ReplacedField {
                    field: "headers.Authorization".to_owned(),
                    var: "API_TOKEN".to_owned(),
                },
            ],
            vanish_count: 2,
            last_vanish: Some(9),
        }
    }

    fn sample() -> State {
        let mut state = State::default();
        state.records.insert(
            key("a"),
            Record::owned_with(PathBuf::from("/c/config.toml"), hashes('a'), adopted()),
        );
        for (name, op, expected, previous) in [
            ("b", WriteKind::Add, Some(hashes('b')), None),
            ("c", WriteKind::Update, Some(hashes('c')), Some(hashes('d'))),
            ("d", WriteKind::Remove, None, Some(hashes('e'))),
        ] {
            state.records.insert(
                key(name),
                Record {
                    file: PathBuf::from("/c/config.toml"),
                    time: 7,
                    version: "0.1.0".to_owned(),
                    state: RecordState::Pending {
                        op,
                        expected,
                        previous,
                    },
                    meta: RecordMeta::default(),
                },
            );
        }
        let mut user = adopted();
        user.adopted_from = Some(AdoptedFrom {
            harness: Harness::Codex,
            scope: OriginScope::User,
        });
        user.replaced_fields.clear();
        user.vanish_count = 0;
        user.last_vanish = None;
        state.records.insert(
            key("e"),
            Record::owned_with(PathBuf::from("/c/config.toml"), hashes('e'), user),
        );
        state
    }

    #[test]
    fn round_trip() {
        let state = sample();
        let text = state.render();
        assert!(text.starts_with("schema_version = 1\n\n[[records]]\nharness = \"codex\""));
        assert_eq!(State::parse(&text, Path::new("/s")).unwrap(), state);
        assert_eq!(
            State::parse("schema_version = 1\n", Path::new("/s")).unwrap(),
            State::default()
        );
        assert_eq!(State::default().render(), "schema_version = 1\n");
    }

    #[test]
    fn corrupt_and_unsupported() {
        let path = Path::new("/s/state.toml");
        let good = sample().render();
        let cases = [
            "x = ".to_owned(),
            "records = []\n".to_owned(),
            "schema_version = \"1\"\n".to_owned(),
            "schema_version = 1\nother = 1\n".to_owned(),
            "schema_version = 1\nrecords = 1\n".to_owned(),
            "schema_version = 1\nrecords = [1]\n".to_owned(),
            good.replacen("harness = \"codex\"", "harness = \"x\"", 1),
            good.replacen("scope = \"user\"", "scope = \"project\"", 1),
            good.replacen("name = \"a\"", "name = \"A\"", 1),
            good.replacen("file = \"/c/config.toml\"", "file = \"rel\"", 1),
            good.replacen("status = \"owned\"", "status = \"other\"", 1),
            good.replacen("op = \"add\"", "op = \"move\"", 1),
            good.replacen("op = \"add\"", "op = \"remove\"", 1),
            good.replacen("op = \"update\"", "op = \"add\"", 1)
                .replacen("status = \"owned\"", "status = \"owned\"\nop = \"add\"", 1),
            good.replacen("version = ", "extra = 1\nversion = ", 1),
            good.replacen(&"a".repeat(64), "short", 1),
            good.replacen("name = \"b\"", "name = \"a\"", 1),
            good.replacen("name = \"a\"", "name = 1", 1),
            good.replacen("written = true", "written = false", 1),
            good.replacen("written = true", "written = 1", 1),
            good.replacen("prev_written = true", "prev_written = \"yes\"", 1),
            // A provenance marker without hashes.
            good.replacen(
                "version = \"0.1.0\"",
                "version = \"0.1.0\"\nprev_written = true",
                1,
            ),
            good.replacen("harness = \"claude\"", "harness = \"x\"", 1),
            good.replacen("scope = \"project\"", "scope = \"user\"", 1),
            good.replacen("root = \"/work/app\"", "root = \"rel\"", 1),
            good.replacen("root = \"/work/app\"", "other = \"x\"", 1),
            good.replacen("adopted_from = {", "adopted_from = 1\nx = {", 1),
            good.replacen("adopted_from = {", "adopted_from = { extra = 1, ", 1),
            good.replacen("var = \"A\"", "var = \"1A\"", 1),
            good.replacen("field = \"env.A\"", "field = \"\"", 1),
            good.replacen("field = \"env.A\"", "field = \"z\"", 1),
            good.replacen("var = \"A\" }", "var = \"A\", x = 1 }", 1),
            good.replacen("replaced_fields = [", "replaced_fields = [1, ", 1),
            good.replacen("replaced_fields = [", "replaced_fields = 1\nx = [", 1),
            good.replacen("vanish_count = 2", "vanish_count = 0", 1),
            good.replacen("vanish_count = 2", "vanish_count = -1", 1),
            good.replacen("vanish_count = 2", "vanish_count = \"2\"", 1),
            good.replacen("last_vanish = 9\n", "", 1),
            // Replaced fields without an adoption.
            good.replacen(
                "adopted_from = { harness = \"codex\", scope = \"user\" }\n",
                "replaced_fields = [{ field = \"env.A\", var = \"A\" }]\n",
                1,
            ),
        ];
        for text in &cases {
            let error = State::parse(text, path).unwrap_err();
            assert!(
                matches!(error, Error::StateCorrupt { .. }),
                "{text}: {error:?}"
            );
            assert_eq!(error.exit_code(), 3);
        }
        let mut missing_time = good.clone();
        missing_time = missing_time.replacen("time = ", "times = ", 1);
        assert!(State::parse(&missing_time, path).is_err());
        let negative = good.replacen("time = 7", "time = -7", 1);
        assert!(State::parse(&negative, path).is_err());
        assert!(matches!(
            State::parse("schema_version = 2\n", path),
            Err(Error::SchemaUnsupported { found: 2, .. })
        ));
    }

    #[test]
    fn settling_pending_records() {
        let state = sample();
        let add = &state.records[&key("b")];
        let update = &state.records[&key("c")];
        let remove = &state.records[&key("d")];
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        let d = "d".repeat(64);
        let e = "e".repeat(64);
        assert_eq!(settle(add, Some(&b)).1, Settled::Promoted);
        assert_eq!(settle(add, None), (None, Settled::Dropped));
        // An Add whose entry has other content is not owned.
        assert_eq!(settle(add, Some(&c)), (None, Settled::Dropped));
        let (promoted, outcome) = settle(update, Some(&c));
        assert_eq!(outcome, Settled::Promoted);
        assert_eq!(promoted.unwrap().hashes().unwrap().readback, c);
        let (reverted, outcome) = settle(update, Some(&d));
        assert_eq!(outcome, Settled::Reverted);
        assert_eq!(reverted.unwrap().hashes().unwrap().readback, d);
        assert_eq!(settle(update, None).1, Settled::Reverted);
        assert_eq!(settle(remove, None), (None, Settled::Dropped));
        assert_eq!(settle(remove, Some(&e)).1, Settled::Reverted);
        let owned = &state.records[&key("a")];
        assert_eq!(settle(owned, None).0.as_ref(), Some(owned));
        assert_eq!(state.of(Harness::Codex).count(), 5);
        assert_eq!(state.of(Harness::Claude).count(), 0);
    }

    #[test]
    fn promoted_vanish_re_adds_are_counted_and_kept() {
        let mut readd = Record {
            file: PathBuf::from("/c/config.toml"),
            time: 7,
            version: "0.1.0".to_owned(),
            state: RecordState::Pending {
                op: WriteKind::Add,
                expected: Some(hashes('b')),
                previous: Some(hashes('c')),
            },
            meta: adopted(),
        };
        let (promoted, outcome) = settle(&readd, Some(&"b".repeat(64)));
        assert_eq!(outcome, Settled::Promoted);
        let promoted = promoted.unwrap();
        assert_eq!(promoted.meta.vanish_count, 3);
        assert!(promoted.meta.last_vanish.unwrap() > 9);
        assert_eq!(promoted.meta.adopted_from, adopted().adopted_from);
        // A re-add that did not take effect is not counted.
        let (reverted, _) = settle(&readd, None);
        assert_eq!(reverted.unwrap().meta.vanish_count, 2);
        // An Add of a new entry is not a re-add.
        readd.state = RecordState::Pending {
            op: WriteKind::Add,
            expected: Some(hashes('b')),
            previous: None,
        };
        readd.meta = RecordMeta::default();
        let (added, _) = settle(&readd, Some(&"b".repeat(64)));
        assert_eq!(added.unwrap().meta, RecordMeta::default());
        let mut text = State::default();
        let mut counted = RecordMeta::default();
        counted.count_vanish();
        text.records.insert(
            key("v"),
            Record::owned_with(PathBuf::from("/c"), hashes('b'), counted.clone()),
        );
        let rendered = text.render();
        assert!(
            rendered.contains("vanish_count = 1\nlast_vanish = "),
            "{rendered}"
        );
        assert_eq!(
            State::parse(&rendered, Path::new("/s")).unwrap().records[&key("v")].meta,
            counted
        );
        assert_eq!(
            AdoptedFrom {
                harness: Harness::Codex,
                scope: OriginScope::User
            }
            .label(),
            "codex:user"
        );
    }

    #[test]
    fn load_and_save() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        assert_eq!(load(&paths).unwrap(), State::default());
        let lock = StoreLock::acquire(&paths).unwrap();
        save(&paths, &lock, &sample()).unwrap();
        assert_eq!(load(&paths).unwrap(), sample());
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut lossy = sample();
            lossy.records.insert(
                key("z"),
                Record::owned(
                    PathBuf::from(std::ffi::OsStr::from_bytes(b"/c/\xff/config.toml")),
                    hashes('a'),
                ),
            );
            let error = save(&paths, &lock, &lossy).unwrap_err();
            assert_eq!(error.code(), "harness-path-not-utf8");
            assert_eq!(error.exit_code(), 4);
            assert_eq!(load(&paths).unwrap(), sample());
            let mut lossy_root = sample();
            let mut meta = adopted();
            meta.adopted_from = Some(AdoptedFrom {
                harness: Harness::Claude,
                scope: OriginScope::Project {
                    root: PathBuf::from(std::ffi::OsStr::from_bytes(b"/w/\xff")),
                },
            });
            lossy_root.records.insert(
                key("z"),
                Record::owned_with(PathBuf::from("/c"), hashes('a'), meta),
            );
            assert_eq!(
                save(&paths, &lock, &lossy_root).unwrap_err().code(),
                "harness-path-not-utf8"
            );
        }
        std::fs::write(&paths.state, b"\xff").unwrap();
        assert!(matches!(load(&paths), Err(Error::StateCorrupt { .. })));
        std::fs::remove_file(&paths.state).unwrap();
        std::fs::create_dir(&paths.state).unwrap();
        assert!(matches!(load(&paths), Err(Error::Io { .. })));
    }
}
