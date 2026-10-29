//! Reading Claude Code's configuration file (`~/.claude.json`): read-only and
//! lazily parsed.
//!
//! The file belongs to a running Claude Code session and holds account data,
//! caches and per-project history next to the MCP servers. d-mcp never
//! writes it (writes go through the `claude` CLI) and keeps none of its
//! bytes: it deserializes only the user-scope `mcpServers.<name>` values, and
//! for every `projects.<path>` only the *names* of its local-scope
//! `mcpServers`; every other key is skipped with [`IgnoredAny`]. Parse errors
//! are reported by line and column only, never with serde's message.
//!
//! For project-scope discovery (WP4) it also reads two kinds of flags of a
//! project, names and booleans only: the workspace trust
//! (`hasTrustDialogAccepted`) and the legacy `.mcp.json` approval keys
//! (`enabledMcpjsonServers`, `disabledMcpjsonServers`,
//! `enableAllProjectMcpServers`), which Claude Code 2.1.312 moves into the
//! project's `.claude/settings.local.json` on its next run.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::canonical::tagged_hash;
use crate::error::{Error, Result};
use crate::store;

/// The domain tag of Claude Code entry hashes.
pub const ENTRY_HASH_TAG: &str = "d-mcp-claude-entry-v1";

/// The delay before the one retry of a parse: a running Claude Code session
/// can be in the middle of rewriting the file.
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// Claude Code's configuration file, opened for reading only. The type has
/// no way to write: it is created through [`ReadOnlyFile::open`], which
/// opens the target with read access only, and is consumed by
/// [`ReadOnlyFile::read_to_end`].
pub struct ReadOnlyFile {
    file: File,
}

impl ReadOnlyFile {
    /// Opens the file's target for reading only, without following a final
    /// symlink of the resolved target and without blocking on a FIFO. It
    /// must be a regular file.
    ///
    /// # Errors
    ///
    /// [`Error::HarnessConfigInvalid`] for a dangling link or a file that is
    /// not regular, [`Error::Io`] otherwise.
    pub fn open(path: &Path) -> Result<Self> {
        let target = fs::canonicalize(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                invalid(path, "not a regular file", None)
            } else {
                Error::io("resolve", path, &error)
            }
        })?;
        let mut options = OpenOptions::new();
        options.read(true);
        store::no_follow(&mut options, true);
        let file = options.open(&target).map_err(|error| {
            if fs::symlink_metadata(&target).is_ok_and(|metadata| !metadata.is_file()) {
                invalid(path, "not a regular file", None)
            } else {
                Error::io("read", path, &error)
            }
        })?;
        let metadata = file
            .metadata()
            .map_err(|error| Error::io("read", path, &error))?;
        if !metadata.is_file() {
            return Err(invalid(path, "not a regular file", None));
        }
        Ok(Self { file })
    }

    /// Reads the whole file.
    ///
    /// # Errors
    ///
    /// [`Error::Io`].
    pub fn read_to_end(mut self, path: &Path) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file
            .read_to_end(&mut bytes)
            .map_err(|error| Error::io("read", path, &error))?;
        Ok(bytes)
    }

    #[cfg(test)]
    pub(super) fn handle(&mut self) -> &mut File {
        &mut self.file
    }
}

pub(super) fn invalid(
    path: &Path,
    reason: &'static str,
    position: Option<(usize, usize)>,
) -> Error {
    Error::HarnessConfigInvalid {
        path: path.to_path_buf(),
        reason,
        position,
    }
}

/// What d-mcp reads from Claude Code's configuration file.
///
/// Its [`fmt::Debug`] output names the entries only: their values are
/// foreign content and can hold secrets.
#[derive(Clone, Default, PartialEq)]
pub struct Parsed {
    /// Whether `mcpServers` is absent or a JSON object.
    pub root_supported: bool,
    /// The user-scope entries by name, every key of each entry.
    pub entries: BTreeMap<String, Value>,
    /// The local-scope entry names, with the project paths that hold them.
    pub local: BTreeMap<String, BTreeSet<String>>,
    /// Every project path of the file.
    pub projects: BTreeSet<String>,
    /// The trust and legacy approval flags of every project, by path.
    pub flags: BTreeMap<String, ProjectFlags>,
}

/// The `.mcp.json` approval keys of one settings source: names and a flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approvals {
    /// `enabledMcpjsonServers`.
    pub enabled: BTreeSet<String>,
    /// `disabledMcpjsonServers`.
    pub disabled: BTreeSet<String>,
    /// `enableAllProjectMcpServers = true`.
    pub enable_all: bool,
}

/// What d-mcp reads of a project entry besides its local-scope names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectFlags {
    /// `hasTrustDialogAccepted = true`: the user trusted the workspace, so
    /// Claude Code honours the project's own settings files.
    pub trusted: bool,
    /// The legacy approval keys of the project entry.
    pub approvals: Approvals,
}

impl fmt::Debug for Parsed {
    /// The entry values are foreign and never printed; only their names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parsed")
            .field("root_supported", &self.root_supported)
            .field("entries", &self.entries.keys().collect::<Vec<_>>())
            .field("local", &self.local)
            .field("projects", &self.projects)
            .field("flags", &self.flags)
            .finish()
    }
}

/// Visitor methods that read any JSON value other than an object as empty,
/// skipping its contents.
macro_rules! non_maps_are_empty {
    ($ty:ty) => {
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<$ty, A::Error> {
            while seq.next_element::<IgnoredAny>()?.is_some() {}
            Ok(<$ty>::default())
        }
        fn visit_bool<E>(self, _: bool) -> std::result::Result<$ty, E> {
            Ok(<$ty>::default())
        }
        fn visit_i64<E>(self, _: i64) -> std::result::Result<$ty, E> {
            Ok(<$ty>::default())
        }
        fn visit_u64<E>(self, _: u64) -> std::result::Result<$ty, E> {
            Ok(<$ty>::default())
        }
        fn visit_f64<E>(self, _: f64) -> std::result::Result<$ty, E> {
            Ok(<$ty>::default())
        }
        fn visit_str<E>(self, _: &str) -> std::result::Result<$ty, E> {
            Ok(<$ty>::default())
        }
        fn visit_unit<E>(self) -> std::result::Result<$ty, E> {
            Ok(<$ty>::default())
        }
    };
}

/// The names of a JSON object's keys; any other JSON value has none.
#[derive(Default)]
struct Names(BTreeSet<String>);

impl<'de> Deserialize<'de> for Names {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct NamesVisitor;
        impl<'de> Visitor<'de> for NamesVisitor {
            type Value = Names;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Names, A::Error> {
                let mut names = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    map.next_value::<IgnoredAny>()?;
                    names.insert(key);
                }
                Ok(Names(names))
            }
            non_maps_are_empty!(Names);
        }
        deserializer.deserialize_any(NamesVisitor)
    }
}

/// A JSON value read as a flag: `true` only for the boolean `true`; any
/// other value is skipped and reads as `false`.
#[derive(Default)]
pub(super) struct Flag(pub(super) bool);

impl<'de> Deserialize<'de> for Flag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct FlagVisitor;
        impl<'de> Visitor<'de> for FlagVisitor {
            type Value = Flag;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E>(self, value: bool) -> std::result::Result<Flag, E> {
                Ok(Flag(value))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Flag, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Flag(false))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Flag, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Flag(false))
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Flag, E> {
                Ok(Flag(false))
            }
            fn visit_unit<E>(self) -> std::result::Result<Flag, E> {
                Ok(Flag(false))
            }
        }
        deserializer.deserialize_any(FlagVisitor)
    }
}

/// A string read leniently: any other JSON value is skipped unread (maps
/// and arrays are drained with [`IgnoredAny`], never built as values).
struct MaybeName(Option<String>);

impl<'de> Deserialize<'de> for MaybeName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct MaybeNameVisitor;
        impl<'de> Visitor<'de> for MaybeNameVisitor {
            type Value = MaybeName;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_str<E>(self, value: &str) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(Some(value.to_owned())))
            }
            fn visit_string<E>(self, value: String) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(Some(value)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<MaybeName, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(MaybeName(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<MaybeName, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(MaybeName(None))
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(None))
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(None))
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(None))
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(None))
            }
            fn visit_unit<E>(self) -> std::result::Result<MaybeName, E> {
                Ok(MaybeName(None))
            }
        }
        deserializer.deserialize_any(MaybeNameVisitor)
    }
}

/// A JSON array read as a set of names; anything else, and every item that
/// is not a string, is skipped.
#[derive(Default)]
pub(super) struct NameList(pub(super) BTreeSet<String>);

impl<'de> Deserialize<'de> for NameList {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct NameListVisitor;
        impl<'de> Visitor<'de> for NameListVisitor {
            type Value = NameList;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<NameList, A::Error> {
                let mut names = BTreeSet::new();
                while let Some(MaybeName(name)) = seq.next_element::<MaybeName>()? {
                    names.extend(name);
                }
                Ok(NameList(names))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<NameList, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(NameList::default())
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<NameList, E> {
                Ok(NameList::default())
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<NameList, E> {
                Ok(NameList::default())
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<NameList, E> {
                Ok(NameList::default())
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<NameList, E> {
                Ok(NameList::default())
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<NameList, E> {
                Ok(NameList::default())
            }
            fn visit_unit<E>(self) -> std::result::Result<NameList, E> {
                Ok(NameList::default())
            }
        }
        deserializer.deserialize_any(NameListVisitor)
    }
}

/// One project: the names of its local-scope servers and its flags.
#[derive(Default)]
struct Project {
    servers: BTreeSet<String>,
    flags: ProjectFlags,
}

impl<'de> Deserialize<'de> for Project {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ProjectVisitor;
        impl<'de> Visitor<'de> for ProjectVisitor {
            type Value = Project;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Project, A::Error> {
                let mut project = Project::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "mcpServers" => project.servers = map.next_value::<Names>()?.0,
                        "hasTrustDialogAccepted" => {
                            project.flags.trusted = map.next_value::<Flag>()?.0;
                        }
                        "enabledMcpjsonServers" => {
                            project.flags.approvals.enabled = map.next_value::<NameList>()?.0;
                        }
                        "disabledMcpjsonServers" => {
                            project.flags.approvals.disabled = map.next_value::<NameList>()?.0;
                        }
                        "enableAllProjectMcpServers" => {
                            project.flags.approvals.enable_all = map.next_value::<Flag>()?.0;
                        }
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(project)
            }
            non_maps_are_empty!(Project);
        }
        deserializer.deserialize_any(ProjectVisitor)
    }
}

/// The projects table; anything but an object has no projects.
#[derive(Default)]
struct Projects(BTreeMap<String, Project>);

impl<'de> Deserialize<'de> for Projects {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ProjectsVisitor;
        impl<'de> Visitor<'de> for ProjectsVisitor {
            type Value = Projects;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Projects, A::Error> {
                let mut projects = BTreeMap::new();
                while let Some(key) = map.next_key::<String>()? {
                    projects.insert(key, map.next_value::<Project>()?);
                }
                Ok(Projects(projects))
            }
            non_maps_are_empty!(Projects);
        }
        deserializer.deserialize_any(ProjectsVisitor)
    }
}

/// The top level: `mcpServers` and `projects`; every other key is skipped.
#[derive(Deserialize)]
struct Root {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: Option<Value>,
    #[serde(default)]
    projects: Option<Projects>,
}

/// Parses the file's bytes. The error is the 1-based line and column.
///
/// # Errors
///
/// The position of a syntax error, or of a top level that is not an object.
pub fn parse(bytes: &[u8]) -> std::result::Result<Parsed, (usize, usize)> {
    // serde would also accept a top-level array as the struct.
    if let Some(offset) = bytes.iter().position(|byte| !byte.is_ascii_whitespace())
        && bytes[offset] != b'{'
    {
        let before = &bytes[..offset];
        let line = 1 + before.iter().filter(|byte| **byte == b'\n').count();
        let column = 1 + before
            .iter()
            .rev()
            .take_while(|byte| **byte != b'\n')
            .count();
        return Err((line, column));
    }
    let root: Root =
        serde_json::from_slice(bytes).map_err(|error| (error.line(), error.column()))?;
    let mut parsed = Parsed {
        root_supported: true,
        ..Parsed::default()
    };
    match root.mcp_servers {
        None => {}
        Some(Value::Object(servers)) => parsed.entries = servers.into_iter().collect(),
        Some(_) => parsed.root_supported = false,
    }
    for (path, project) in root.projects.unwrap_or_default().0 {
        for name in project.servers {
            parsed.local.entry(name).or_default().insert(path.clone());
        }
        if project.flags != ProjectFlags::default() {
            parsed.flags.insert(path.clone(), project.flags);
        }
        parsed.projects.insert(path);
    }
    Ok(parsed)
}

/// Reads and parses the file at `path`; a missing file is empty. A parse
/// error is retried once after a short delay, because a running Claude Code
/// session can be in the middle of rewriting the file.
///
/// # Errors
///
/// [`Error::HarnessConfigInvalid`] (by position only) and [`Error::Io`].
pub fn read(path: &Path) -> Result<Parsed> {
    let attempt = || -> Result<std::result::Result<Parsed, (usize, usize)>> {
        if !crate::adapter::path_exists(path)? {
            return Ok(Ok(Parsed {
                root_supported: true,
                ..Parsed::default()
            }));
        }
        let bytes = ReadOnlyFile::open(path)?.read_to_end(path)?;
        Ok(parse(&bytes))
    };
    match attempt()? {
        Ok(parsed) => Ok(parsed),
        Err(_) => {
            thread::sleep(RETRY_DELAY);
            attempt()?.map_err(|position| invalid(path, "not valid JSON", Some(position)))
        }
    }
}

/// One number form: an integral float becomes an integer, so `30000.0` and
/// `30000` hash alike (Claude Code writes JavaScript numbers).
fn canonical_number(number: &Number) -> Number {
    match number.as_f64() {
        Some(float)
            if number.is_f64() && float.fract() == 0.0 && float.abs() < 9_007_199_254_740_992.0 =>
        {
            // The range check above makes the conversion exact.
            Number::from(float as i64)
        }
        _ => number.clone(),
    }
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Number(number) => Value::Number(canonical_number(number)),
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, item)| (key.clone(), canonical_value(item)))
                .collect::<Map<String, Value>>(),
        ),
        other => other.clone(),
    }
}

/// The canonical form of an entry, applied alike to a rendering and to an
/// entry read back. It absorbs Claude Code's normalization (verified against
/// 2.1.310): `type = "stdio"` is the default and is dropped, an empty `args`
/// (which Claude Code adds to stdio entries), `env` or `headers` is dropped,
/// and numbers get one form. Keys are sorted by the canonical writer.
#[must_use]
pub fn canonical_entry(value: &Value) -> Value {
    let mut value = canonical_value(value);
    if let Value::Object(object) = &mut value {
        if object.get("type").and_then(Value::as_str) == Some("stdio") {
            object.remove("type");
        }
        for key in ["args", "env", "headers"] {
            let empty = match object.get(key) {
                Some(Value::Array(items)) => items.is_empty(),
                Some(Value::Object(items)) => items.is_empty(),
                _ => false,
            };
            if empty {
                object.remove(key);
            }
        }
    }
    value
}

/// The semantic hash of an entry: [`ENTRY_HASH_TAG`] over the canonical JSON
/// of [`canonical_entry`]. Every key of the entry counts.
#[must_use]
pub fn entry_hash(value: &Value) -> String {
    tagged_hash(ENTRY_HASH_TAG, &canonical_entry(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_keeps_only_servers_and_local_names() {
        let parsed = parse(
            br#"{"oauthAccount":{"emailAddress":"CANARY"},"mcpServers":{"a":{"command":"x","env":{"T":"CANARY"}},"b":1},
                "projects":{"/p":{"history":["CANARY"],"mcpServers":{"a":{"env":{"T":"CANARY"}},"c":{}}},
                            "/q":{"mcpServers":[]},"/r":7}}"#,
        )
        .unwrap();
        assert!(parsed.root_supported);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.local["a"], BTreeSet::from(["/p".to_owned()]));
        assert_eq!(parsed.local["c"], BTreeSet::from(["/p".to_owned()]));
        assert_eq!(parsed.projects.len(), 3);
        let text = format!("{parsed:?}");
        assert_eq!(text.matches("CANARY").count(), 0, "{text}");
        assert!(text.contains("entries: [\"a\", \"b\"]"), "{text}");
        let odd =
            parse(br#"{"mcpServers":[],"projects":["x", true, null, 1.5, -1, 2, "s"]}"#).unwrap();
        assert!(!odd.root_supported);
        assert!(odd.projects.is_empty());
        let odd = parse(
            br#"{"projects":{"/a":true,"/b":1.5,"/c":-1,"/d":"s","/e":null,"/f":[1],
                 "/g":{"mcpServers":true},"/h":{"mcpServers":1.5},"/i":{"mcpServers":-1},
                 "/j":{"mcpServers":2},"/k":{"mcpServers":"s"},"/l":{"mcpServers":null},
                 "/m":{"mcpServers":[1]}},"mcpServers":null}"#,
        )
        .unwrap();
        assert!(odd.root_supported);
        assert_eq!(odd.projects.len(), 13);
        assert!(odd.local.is_empty());
        for scalar in [&b"true"[..], b"1", b"\"s\"", b"null", b"1.5", b"-1"] {
            let projects = format!(r#"{{"projects":{}}}"#, String::from_utf8_lossy(scalar));
            assert!(parse(projects.as_bytes()).unwrap().projects.is_empty());
        }
        assert_eq!(parse(b"{\n  \"a\": ,\n}").unwrap_err(), (2, 8));
        assert_eq!(parse(b"\n  []").unwrap_err(), (2, 3));
        assert!(parse(b"").is_err());
    }

    #[test]
    fn parse_keeps_trust_and_legacy_approvals_as_names_and_flags() {
        let parsed = parse(
            br#"{"projects":{"/p":{"hasTrustDialogAccepted":true,"enabledMcpjsonServers":["a",1,-1,1.5,true,null,[["CANARY"]],{"CANARY":[2]},"b"],
                "disabledMcpjsonServers":"CANARY","enableAllProjectMcpServers":"yes","history":["CANARY"]},
                "/q":{"hasTrustDialogAccepted":{"CANARY":[1]},"enableAllProjectMcpServers":true,
                "disabledMcpjsonServers":{"CANARY":1}},
                "/r":{"hasTrustDialogAccepted":[true],"enabledMcpjsonServers":[[1]],"x":1}}}"#,
        )
        .unwrap();
        let p = &parsed.flags["/p"];
        assert!(p.trusted);
        assert_eq!(
            p.approvals.enabled,
            BTreeSet::from(["a".to_owned(), "b".to_owned()])
        );
        assert!(p.approvals.disabled.is_empty());
        assert!(!p.approvals.enable_all);
        let q = &parsed.flags["/q"];
        assert!(!q.trusted);
        assert!(q.approvals.enable_all);
        assert!(!parsed.flags.contains_key("/r"));
        let text = format!("{parsed:?}");
        assert!(!text.contains("CANARY"), "{text}");
        for scalar in ["1", "1.5", "-1", "null", "\"s\""] {
            let text = format!(
                r#"{{"projects":{{"/p":{{"hasTrustDialogAccepted":{scalar},"enabledMcpjsonServers":{scalar}}}}}}}"#
            );
            assert!(parse(text.as_bytes()).unwrap().flags.is_empty(), "{scalar}");
        }
    }

    #[test]
    fn canonical_form_absorbs_claude_normalization() {
        let rendered = json!({"command": "/bin/x", "env": {"A": "${A:-}"}, "timeout": 30000.0});
        let written = json!({"type": "stdio", "timeout": 30000, "args": [], "command": "/bin/x", "env": {"A": "${A:-}"}});
        assert_eq!(entry_hash(&rendered), entry_hash(&written));
        let http = json!({"type": "http", "url": "https://x.test", "headers": {}});
        assert_eq!(
            entry_hash(&http),
            entry_hash(&json!({"url": "https://x.test", "type": "http"}))
        );
        assert_ne!(
            entry_hash(&json!({"type": "http", "url": "u"})),
            entry_hash(&json!({"url": "u"}))
        );
        assert_ne!(
            entry_hash(&json!({"command": "x", "timeout": 1.5})),
            entry_hash(&json!({"command": "x", "timeout": 1}))
        );
        assert_eq!(
            canonical_entry(&json!([1.0, {"n": 2.0}])),
            json!([1, {"n": 2}])
        );
        assert_eq!(canonical_entry(&json!(1e300)), json!(1e300));
        assert!(entry_hash(&json!({"command": "x"})).len() == 64);
    }

    #[test]
    fn reading_retries_and_reports_positions_only() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude.json");
        assert_eq!(
            read(&path).unwrap(),
            Parsed {
                root_supported: true,
                ..Parsed::default()
            }
        );
        fs::write(&path, "{\"mcpServers\": {\"a\": CANARY}}").unwrap();
        let error = read(&path).unwrap_err();
        assert_eq!(error.code(), "harness-config-invalid");
        assert!(error.hint().contains("not valid JSON at line 1, column 22"));
        assert!(!error.hint().contains("CANARY"));
        fs::write(&path, b"\xff").unwrap();
        assert_eq!(read(&path).unwrap_err().code(), "harness-config-invalid");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(read(&path).unwrap_err().code(), "harness-config-invalid");
        #[cfg(unix)]
        {
            let link = temp.path().join("link.json");
            std::os::unix::fs::symlink(temp.path().join("missing"), &link).unwrap();
            assert_eq!(read(&link).unwrap_err().code(), "harness-config-invalid");
        }
    }

    #[test]
    fn the_read_only_file_cannot_write() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude.json");
        fs::write(&path, "{}").unwrap();
        let mut file = ReadOnlyFile::open(&path).unwrap();
        assert!(file.handle().write_all(b"x").is_err());
        assert_eq!(file.read_to_end(&path).unwrap(), b"{}");
        assert_eq!(fs::read(&path).unwrap(), b"{}");
    }
}
