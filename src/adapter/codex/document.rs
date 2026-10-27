//! The Codex `config.toml` document: locating `mcp_servers.<name>` entries,
//! their semantic hashes, and byte-preserving edits.
//!
//! `toml_edit` re-encodes line endings and a missing trailing newline, so
//! d-mcp never re-encodes the whole file. It uses `toml_edit` to find the
//! byte region of each managed entry and splices new text into the original
//! bytes; every byte outside the managed regions stays as it was.
//!
//! An entry's region starts at its `[mcp_servers.<name>]` header and ends where
//! the next foreign header's prefix (blank lines and comments above it)
//! begins, or, for the last table of the file, where the document's trailing
//! blank lines and comments begin. Comments above an entry's own header
//! belong to that header's prefix and comments after the last table belong
//! to the document; both stay in place on Update and Remove. The entry and
//! its sub-tables must be contiguous standard tables; inline or dotted
//! entries are reported as unsupported.
//!
//! Harness-local preserved keys ([`PRESERVED_KEYS`]) are skipped while an
//! entry is converted into JSON, so their values never enter a `Value`.
//!
//! No `toml_edit` error text is ever kept: a syntax error is reduced to its
//! position.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use serde_json::{Map, Number, Value};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

use crate::canonical::tagged_hash;
use crate::spanned::{self, Node, NodeKind};

/// Domain tag of the Codex entry hash (sent and read back).
pub const ENTRY_HASH_TAG: &str = "d-mcp-codex-entry-v1";
/// The root table of MCP server entries.
pub const ROOT: &str = "mcp_servers";
/// The preserved OAuth client secret (see [`PRESERVED_KEYS`]).
pub const PRESERVED_CLIENT_SECRET: &str = "oauth.client_secret";
/// The sub-table that holds the preserved keys.
const PRESERVED_TABLE: &str = "oauth";
/// Harness-local preserved keys of the `oauth` sub-table: never written,
/// read into a model, hashed or shown by d-mcp, and kept verbatim on Update
/// while they fit the new rendering (`preserved_key_fits`).
/// Codex writes `oauth.callback_url` itself next to a client secret given to
/// `codex mcp add` (verified against Codex 0.165.0); `callback_port` is the
/// other local callback field of Codex's OAuth client configuration.
pub const PRESERVED_KEYS: &[&str] = &[
    PRESERVED_CLIENT_SECRET,
    "oauth.callback_url",
    "oauth.callback_port",
];

/// Whether a preserved key still fits a rendered entry, so an Update may
/// carry it over. Codex refuses to load its whole configuration when an
/// `oauth` table sits on a stdio entry or when `oauth.client_secret` has no
/// `oauth.client_id` (verified against Codex 0.165.0), so:
///
/// - `oauth.client_secret` fits only next to a rendered `oauth.client_id`;
/// - `oauth.callback_url` and `oauth.callback_port` fit only an http entry
///   that authenticates through OAuth: neither `bearer_token_env_var` nor
///   `env_http_headers` is rendered. Codex accepts them without a client ID
///   (dynamic client registration uses the callback too); on a bearer or
///   header entry it accepts but never uses them.
#[must_use]
pub fn preserved_key_fits(key: &str, entry: &[(String, Value)]) -> bool {
    let has = |name: &str| entry.iter().any(|(key, _)| key == name);
    if key == PRESERVED_CLIENT_SECRET {
        entry.iter().any(|(key, value)| {
            key == PRESERVED_TABLE
                && value
                    .as_object()
                    .is_some_and(|oauth| oauth.contains_key("client_id"))
        })
    } else {
        has("url") && !has("bearer_token_env_var") && !has("env_http_headers")
    }
}

/// The key of a preserved key inside the `oauth` sub-table.
fn preserved_sub_key(key: &str) -> &str {
    key.strip_prefix("oauth.").unwrap_or(key)
}

/// A parsed entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedEntry {
    /// The semantic hash.
    pub hash: String,
    /// The byte region, when the entry is in a style d-mcp can edit.
    pub region: Option<Range<usize>>,
    /// Preserved keys present in the entry.
    pub preserved: BTreeSet<&'static str>,
}

/// A parsed Codex config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The entries by name.
    pub entries: BTreeMap<String, ParsedEntry>,
    /// Whether `mcp_servers` is absent or a standard table.
    pub root_supported: bool,
    /// The file's line ending.
    pub eol: &'static str,
}

/// A table header in the document.
struct Header {
    /// The key path.
    path: Vec<String>,
    /// Start of the header's prefix (blank lines and comments above it).
    prefix_start: usize,
    /// Start of the `[` of the header.
    start: usize,
}

/// The line ending of a text: CRLF when its first line ends with CRLF.
#[must_use]
pub fn line_ending(text: &str) -> &'static str {
    match text.find('\n') {
        Some(index) if index > 0 && text.as_bytes()[index - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

/// Parses a Codex config.
///
/// # Errors
///
/// The 1-based line and column of a syntax error.
pub fn parse(text: &str) -> Result<Parsed, (usize, usize)> {
    let position = |offset: Option<usize>| spanned::line_column(text, offset.unwrap_or(0));
    let document = toml_edit::Document::parse(text)
        .map_err(|error| position(error.span().map(|span| span.start)))?;
    let nodes = spanned::parse_document(text).map_err(position)?;
    let eol = line_ending(text);
    let mut parsed = Parsed {
        entries: BTreeMap::new(),
        root_supported: true,
        eol,
    };
    let root_table = match document.get(ROOT) {
        None => return Ok(parsed),
        Some(Item::Table(table)) if !table.is_dotted() => table,
        Some(_) => {
            parsed.root_supported = false;
            return Ok(parsed);
        }
    };
    // The document's trailing blank lines and comments start right after
    // the last line of the last table; they never belong to an entry.
    let tail = document
        .trailing()
        .span()
        .map_or(text.len(), |span| span.start);
    let mut headers = Vec::new();
    collect_headers(document.as_table(), &mut Vec::new(), &mut headers);
    headers.sort_by_key(|header| header.start);
    let root_nodes = nodes
        .iter()
        .find(|entry| entry.key == ROOT)
        .and_then(|entry| match &entry.value.kind {
            NodeKind::Table(entries) => Some(entries),
            _ => None,
        });
    for (name, item) in root_table.iter() {
        let Some(node) = root_nodes.and_then(|entries| entries.iter().find(|e| e.key == name))
        else {
            continue;
        };
        let (value, preserved) = entry_json(&node.value);
        let standard = matches!(item, Item::Table(table)
            if !table.is_dotted() && !table.is_implicit() && table.span().is_some());
        let region = if standard {
            region(&headers, name, tail)
        } else {
            None
        };
        parsed.entries.insert(
            name.to_owned(),
            ParsedEntry {
                hash: entry_hash(&value),
                region,
                preserved,
            },
        );
    }
    Ok(parsed)
}

fn collect_headers(table: &Table, path: &mut Vec<String>, headers: &mut Vec<Header>) {
    if !path.is_empty()
        && !table.is_dotted()
        && !table.is_implicit()
        && let Some(span) = table.span()
    {
        let prefix_start = table
            .decor()
            .prefix()
            .and_then(toml_edit::RawString::span)
            .map_or(span.start, |prefix| prefix.start.min(span.start));
        headers.push(Header {
            path: path.clone(),
            prefix_start,
            start: span.start,
        });
    }
    for (key, item) in table.iter() {
        match item {
            Item::Table(child) => {
                path.push(key.to_owned());
                collect_headers(child, path, headers);
                path.pop();
            }
            Item::ArrayOfTables(array) => {
                path.push(key.to_owned());
                for child in array.iter() {
                    collect_headers(child, path, headers);
                }
                path.pop();
            }
            _ => {}
        }
    }
}

/// The region of a standard entry: its own header first, then only its own
/// sub-table headers, contiguously. It ends before the next header's prefix,
/// or at `tail` (the start of the document's trailing decor) for the last
/// table.
fn region(headers: &[Header], name: &str, tail: usize) -> Option<Range<usize>> {
    let belongs = |header: &Header| {
        header.path.len() >= 2 && header.path[0] == ROOT && header.path[1] == name
    };
    let first = headers.iter().position(belongs)?;
    if headers[first].path.len() != 2 {
        return None;
    }
    let count = headers.iter().filter(|header| belongs(header)).count();
    let last = first + count - 1;
    if !headers[first..=last].iter().all(belongs) {
        return None;
    }
    let end = headers.get(last + 1).map_or(tail, |next| next.prefix_start);
    Some(headers[first].start..end)
}

/// Converts a spanned node into JSON. Numbers get one canonical form later.
fn node_json(node: &Node) -> Value {
    match &node.kind {
        NodeKind::String(text) | NodeKind::Datetime(text) => Value::String(text.clone()),
        NodeKind::Integer(number) => Value::from(*number),
        NodeKind::Float(number) => Number::from_f64(*number)
            .map_or_else(|| Value::String(number.to_string()), Value::Number),
        NodeKind::Boolean(flag) => Value::Bool(*flag),
        NodeKind::Array(items) => Value::Array(items.iter().map(node_json).collect()),
        NodeKind::Table(entries) => Value::Object(
            entries
                .iter()
                .map(|entry| (entry.key.clone(), node_json(&entry.value)))
                .collect(),
        ),
    }
}

/// Converts an entry into JSON, skipping the preserved keys of its `oauth`
/// sub-table; returns the names of the preserved keys that were present.
fn entry_json(node: &Node) -> (Value, BTreeSet<&'static str>) {
    let mut preserved = BTreeSet::new();
    let NodeKind::Table(entries) = &node.kind else {
        return (node_json(node), preserved);
    };
    let mut object = Map::new();
    for entry in entries {
        let value = match (&entry.value.kind, entry.key == PRESERVED_TABLE) {
            (NodeKind::Table(sub), true) => {
                let mut kept = Map::new();
                for item in sub {
                    match PRESERVED_KEYS
                        .iter()
                        .find(|key| preserved_sub_key(key) == item.key)
                    {
                        Some(key) => {
                            preserved.insert(*key);
                        }
                        None => {
                            kept.insert(item.key.clone(), node_json(&item.value));
                        }
                    }
                }
                Value::Object(kept)
            }
            _ => node_json(&entry.value),
        };
        object.insert(entry.key.clone(), value);
    }
    (Value::Object(object), preserved)
}

/// Gives every number one canonical form: an `f64`. Codex may rewrite `30`
/// as `30.0`; both hash the same.
fn unify_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            if let Some(unified) = number.as_f64().and_then(Number::from_f64) {
                *number = unified;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(unify_numbers),
        Value::Object(object) => object.values_mut().for_each(unify_numbers),
        _ => {}
    }
}

/// Keys whose value Codex treats as its default and drops when it re-saves
/// its file (verified against Codex 0.165.0), with that default. An empty
/// array or table counts as the default of `args`, `env_vars`, `env`,
/// `env_http_headers`, `scopes` and `disabled_tools`. `enabled_tools = []`
/// is not a default (it allows no tool); `validate` rejects it.
#[must_use]
pub fn is_default(key: &str, value: &Value) -> bool {
    match (key, value) {
        ("enabled", Value::Bool(true)) | ("required", Value::Bool(false)) => true,
        ("args" | "env_vars" | "scopes" | "disabled_tools", Value::Array(items)) => {
            items.is_empty()
        }
        ("env" | "env_http_headers", Value::Object(object)) => object.is_empty(),
        _ => false,
    }
}

/// The canonical form of an entry: preserved keys removed (and an emptied
/// `oauth` table with them), default-valued keys removed, `env_vars` strings
/// as `{name, source = "local"}`, `startup_timeout_ms` as
/// `startup_timeout_sec`, and one number form.
#[must_use]
pub fn canonical_entry(value: &Value) -> Value {
    let mut value = value.clone();
    if let Value::Object(object) = &mut value {
        if let Some(Value::Object(oauth)) = object.get_mut(PRESERVED_TABLE) {
            for key in PRESERVED_KEYS {
                oauth.remove(preserved_sub_key(key));
            }
            if oauth.is_empty() {
                object.remove(PRESERVED_TABLE);
            }
        }
        object.retain(|key, item| !is_default(key, item));
        if let Some(Value::Array(vars)) = object.get_mut("env_vars") {
            for var in vars.iter_mut() {
                if let Value::String(name) = var {
                    let mut table = Map::new();
                    table.insert("name".to_owned(), Value::String(name.clone()));
                    table.insert("source".to_owned(), Value::String("local".to_owned()));
                    *var = Value::Object(table);
                }
            }
        }
        if !object.contains_key("startup_timeout_sec")
            && let Some(milliseconds) = object.remove("startup_timeout_ms")
        {
            let seconds = milliseconds
                .as_f64()
                .and_then(|ms| Number::from_f64(ms / 1000.0))
                .map_or(milliseconds, Value::Number);
            object.insert("startup_timeout_sec".to_owned(), seconds);
        }
    }
    unify_numbers(&mut value);
    value
}

/// The semantic hash of an entry value.
#[must_use]
pub fn entry_hash(value: &Value) -> String {
    tagged_hash(ENTRY_HASH_TAG, &canonical_entry(value))
}

/// Converts a JSON value into a TOML value. Objects become inline tables.
fn toml_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::String(text) => text.as_str().into(),
        Value::Bool(flag) => (*flag).into(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.into(),
            None => number.as_f64().unwrap_or_default().into(),
        },
        Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(toml_value(item));
            }
            array.into()
        }
        Value::Object(object) => {
            let mut table = InlineTable::new();
            for (key, item) in object {
                table.insert(key, toml_value(item));
            }
            table.into()
        }
        // Rendered entries never hold null.
        Value::Null => "".into(),
    }
}

/// Renders an entry as standard tables: `[mcp_servers.<name>]` with its
/// scalars and arrays, then one sub-table per object value. Preserved
/// `oauth` keys are carried over verbatim; the caller passes only keys that
/// fit the entry ([`preserved_key_fits`]).
#[must_use]
pub fn entry_text(
    name: &str,
    entry: &[(String, Value)],
    preserved: Vec<(&'static str, toml_edit::Value)>,
    eol: &str,
) -> String {
    let mut document = DocumentMut::new();
    let mut root = Table::new();
    root.set_implicit(true);
    let mut table = Table::new();
    for (key, value) in entry {
        match value {
            Value::Object(object) => {
                let mut sub = Table::new();
                for (sub_key, item) in object {
                    sub.insert(sub_key, Item::Value(toml_value(item)));
                }
                table.insert(key, Item::Table(sub));
            }
            other => {
                table.insert(key, Item::Value(toml_value(other)));
            }
        }
    }
    for (key, mut kept) in preserved {
        kept.decor_mut().clear();
        if !matches!(table.get(PRESERVED_TABLE), Some(Item::Table(_))) {
            table.insert(PRESERVED_TABLE, Item::Table(Table::new()));
        }
        if let Some(Item::Table(oauth)) = table.get_mut(PRESERVED_TABLE) {
            oauth.insert(preserved_sub_key(key), Item::Value(kept));
        }
    }
    root.insert(name, Item::Table(table));
    document.insert(ROOT, Item::Table(root));
    let text = document.to_string();
    if eol == "\n" {
        text
    } else {
        text.replace('\n', eol)
    }
}

/// Reads the preserved `oauth` values of an entry, in [`PRESERVED_KEYS`]
/// order, to carry them over verbatim. They are never inspected, hashed or
/// shown.
#[must_use]
pub fn preserved_values(text: &str, name: &str) -> Vec<(&'static str, toml_edit::Value)> {
    let Ok(document) = text.parse::<DocumentMut>() else {
        return Vec::new();
    };
    let Some(oauth) = document
        .get(ROOT)
        .and_then(|root| root.get(name))
        .and_then(|entry| entry.get(PRESERVED_TABLE))
    else {
        return Vec::new();
    };
    PRESERVED_KEYS
        .iter()
        .filter_map(|key| {
            let value = oauth.get(preserved_sub_key(key))?.as_value()?.clone();
            Some((*key, value))
        })
        .collect()
}

/// An edit of one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Append a new entry.
    Add(String),
    /// Replace an entry's region.
    Replace(String),
    /// Delete an entry's region.
    Remove,
}

/// Applies edits to the text. Replace and Remove need the entry's region;
/// edits without one are skipped (the planner never produces them).
#[must_use]
pub fn apply_edits(text: &str, parsed: &Parsed, edits: &[(String, Edit)]) -> String {
    let mut spliced: Vec<(Range<usize>, &str)> = edits
        .iter()
        .filter_map(|(name, edit)| {
            let region = parsed.entries.get(name)?.region.clone()?;
            match edit {
                Edit::Replace(new) => Some((region, new.as_str())),
                Edit::Remove => Some((region, "")),
                Edit::Add(_) => None,
            }
        })
        .collect();
    spliced.sort_by_key(|(region, _)| region.start);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (region, replacement) in spliced {
        out.push_str(&text[cursor..region.start]);
        out.push_str(replacement);
        cursor = region.end;
    }
    out.push_str(&text[cursor..]);
    let eol = parsed.eol;
    for (_, edit) in edits {
        if let Edit::Add(new) = edit {
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push_str(eol);
                }
                if !out.ends_with(&format!("{eol}{eol}")) {
                    out.push_str(eol);
                }
            }
            out.push_str(new);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = "model = \"o3\"\n\n# above a\n[mcp_servers.a]\ncommand = \"npx\"\n\n\
                          # above env\n[mcp_servers.a.env]\nX = \"1\"\n\n# above b\n\
                          [mcp_servers.b]\nurl = \"https://x.test/mcp\"\n\n[profiles.p]\nm = 1\n";

    #[test]
    fn regions_start_at_the_header_and_end_before_the_next_prefix() {
        let parsed = parse(SAMPLE).unwrap();
        assert!(parsed.root_supported);
        assert_eq!(parsed.eol, "\n");
        let a = parsed.entries["a"].region.clone().unwrap();
        assert!(SAMPLE[a.clone()].starts_with("[mcp_servers.a]\n"));
        assert!(SAMPLE[a].ends_with("X = \"1\"\n"));
        let b = parsed.entries["b"].region.clone().unwrap();
        assert_eq!(
            &SAMPLE[b],
            "[mcp_servers.b]\nurl = \"https://x.test/mcp\"\n"
        );
    }

    #[test]
    fn remove_keeps_comments_above_the_header() {
        let parsed = parse(SAMPLE).unwrap();
        let out = apply_edits(SAMPLE, &parsed, &[("a".to_owned(), Edit::Remove)]);
        assert_eq!(
            out,
            "model = \"o3\"\n\n# above a\n\n# above b\n[mcp_servers.b]\nurl = \"https://x.test/mcp\"\n\n[profiles.p]\nm = 1\n"
        );
    }

    #[test]
    fn styles_and_roots() {
        let inline = parse("[mcp_servers]\na = { command = \"x\" }\n").unwrap();
        assert!(inline.root_supported);
        assert_eq!(inline.entries["a"].region, None);
        let dotted_entry = parse("[mcp_servers]\na.command = \"x\"\n").unwrap();
        assert_eq!(dotted_entry.entries["a"].region, None);
        assert!(
            !parse("mcp_servers.a.command = \"x\"\n")
                .unwrap()
                .root_supported
        );
        assert!(
            !parse("mcp_servers = { a = { command = \"x\" } }\n")
                .unwrap()
                .root_supported
        );
        assert!(!parse("mcp_servers = 1\n").unwrap().root_supported);
        // A foreign table between an entry and its sub-table.
        let split = parse(
            "[mcp_servers.a]\ncommand = \"x\"\n[other]\nk = 1\n[mcp_servers.a.env]\nX = \"1\"\n",
        )
        .unwrap();
        assert_eq!(split.entries["a"].region, None);
        // A sub-table without its own entry header.
        let headless = parse("[mcp_servers.a.env]\nX = \"1\"\n").unwrap();
        assert_eq!(headless.entries["a"].region, None);
        // The same content hashes the same in every style.
        assert_eq!(inline.entries["a"].hash, dotted_entry.entries["a"].hash);
        assert_eq!(parse("").unwrap().entries.len(), 0);
        assert_eq!(parse("a = \"x\nb").unwrap_err(), (2, 2));
    }

    #[test]
    fn canonicalization() {
        let a = json!({"command": "x", "env_vars": ["A", {"name": "B", "source": "local"}],
                       "startup_timeout_ms": 30000, "tool_timeout_sec": 30,
                       "oauth": {"client_secret": "s"}});
        let b = json!({"command": "x", "env_vars": [{"name": "A", "source": "local"}, "B"],
                       "startup_timeout_sec": 30.0, "tool_timeout_sec": 30.0});
        assert_eq!(entry_hash(&a), entry_hash(&b));
        let c = json!({"command": "x", "oauth": {"client_id": "c", "client_secret": "s"}});
        let d = json!({"command": "x", "oauth": {"client_id": "c"}});
        assert_eq!(entry_hash(&c), entry_hash(&d));
        let both = json!({"startup_timeout_ms": 1, "startup_timeout_sec": 2});
        assert_eq!(
            canonical_entry(&both),
            json!({"startup_timeout_ms": 1.0, "startup_timeout_sec": 2.0})
        );
        assert_ne!(entry_hash(&json!({"a": 1})), entry_hash(&json!({"a": "1"})));
        // Every preserved key is excluded.
        let local = json!({"url": "u", "oauth": {"client_id": "c", "client_secret": "s",
                           "callback_url": "http://127.0.0.1/cb", "callback_port": 5555}});
        assert_eq!(
            entry_hash(&local),
            entry_hash(&json!({"url": "u", "oauth": {"client_id": "c"}}))
        );
    }

    #[test]
    fn default_valued_keys_hash_like_absent_ones() {
        let with_defaults = json!({"command": "x", "args": [], "env_vars": [], "env": {},
                                   "enabled": true, "required": false, "disabled_tools": [],
                                   "scopes": [], "env_http_headers": {}});
        assert_eq!(
            entry_hash(&with_defaults),
            entry_hash(&json!({"command": "x"}))
        );
        // Non-default values count.
        for changed in [
            json!({"command": "x", "enabled": false}),
            json!({"command": "x", "required": true}),
            json!({"command": "x", "args": ["a"]}),
            json!({"command": "x", "disabled_tools": ["t"]}),
            // `enabled_tools = []` allows no tool; it is not a default.
            json!({"command": "x", "enabled_tools": []}),
        ] {
            assert_ne!(
                entry_hash(&changed),
                entry_hash(&json!({"command": "x"})),
                "{changed}"
            );
        }
    }

    #[test]
    fn preserved_keys_never_enter_the_json() {
        let parsed = parse(
            "[mcp_servers.x]\nurl = \"u\"\n[mcp_servers.x.oauth]\nclient_id = \"c\"\n\
             client_secret = \"CANARY_SECRET\"\ncallback_url = \"http://127.0.0.1/cb\"\n",
        )
        .unwrap();
        let entry = &parsed.entries["x"];
        assert_eq!(
            entry.preserved,
            BTreeSet::from([PRESERVED_CLIENT_SECRET, "oauth.callback_url"])
        );
        assert_eq!(
            entry.hash,
            entry_hash(&json!({"url": "u", "oauth": {"client_id": "c"}}))
        );
        let nodes = spanned::parse_document(
            "url = \"u\"\n[oauth]\nclient_secret = \"CANARY_SECRET\"\ncallback_port = 1\n",
        )
        .unwrap();
        let node = Node {
            kind: NodeKind::Table(nodes),
            span: None,
        };
        let (value, preserved) = entry_json(&node);
        assert!(!value.to_string().contains("CANARY_SECRET"), "{value}");
        assert_eq!(value, json!({"url": "u", "oauth": {}}));
        assert_eq!(preserved.len(), 2);
    }

    #[test]
    fn trailing_comments_after_the_last_table_stay_on_update_and_remove() {
        for eol in ["\n", "\r\n"] {
            let text = "model = \"o3\"\n\n[mcp_servers.a]\ncommand = \"x\"\n\n\
                        [mcp_servers.a.env]\nX = \"1\" # same line\n\n# trailing 1\n\n# trailing 2\n\n"
                .replace('\n', eol);
            let parsed = parse(&text).unwrap();
            let region = parsed.entries["a"].region.clone().unwrap();
            assert!(text[region.clone()].ends_with(&format!("X = \"1\" # same line{eol}")));
            let tail = &text[region.end..];
            assert_eq!(
                tail,
                format!("{eol}# trailing 1{eol}{eol}# trailing 2{eol}{eol}")
            );
            let removed = apply_edits(&text, &parsed, &[("a".to_owned(), Edit::Remove)]);
            assert_eq!(removed, format!("model = \"o3\"{eol}{eol}{tail}"));
            let new = entry_text("a", &[("command".to_owned(), json!("y"))], Vec::new(), eol);
            let updated = apply_edits(
                &text,
                &parsed,
                &[("a".to_owned(), Edit::Replace(new.clone()))],
            );
            assert_eq!(updated, format!("model = \"o3\"{eol}{eol}{new}{tail}"));
        }
        // Without trailing decor the region still runs to the end.
        let bare = "[mcp_servers.a]\ncommand = \"x\"";
        assert_eq!(
            parse(bare).unwrap().entries["a"].region,
            Some(0..bare.len())
        );
    }

    #[test]
    fn entry_text_renders_standard_tables_and_keeps_the_secret() {
        let entry = vec![
            ("url".to_owned(), json!("https://x.test/mcp")),
            ("scopes".to_owned(), json!(["read"])),
            ("oauth".to_owned(), json!({"client_id": "cid"})),
        ];
        let secret = preserved_values(
            "[mcp_servers.x]\nurl = \"u\"\n[mcp_servers.x.oauth]\nclient_secret = \"canary\" # c\n\
             callback_url = \"http://127.0.0.1/cb\"\nclient_id = \"old\"\n",
            "x",
        );
        assert_eq!(secret.len(), 2);
        let text = entry_text("x", &entry, secret, "\r\n");
        assert_eq!(
            text,
            "[mcp_servers.x]\r\nurl = \"https://x.test/mcp\"\r\nscopes = [\"read\"]\r\n\r\n\
             [mcp_servers.x.oauth]\r\nclient_id = \"cid\"\r\nclient_secret = \"canary\"\r\n\
             callback_url = \"http://127.0.0.1/cb\"\r\n"
        );
        let plain = entry_text(
            "y",
            &[
                ("command".to_owned(), json!("npx")),
                ("n".to_owned(), json!(1.5)),
                ("i".to_owned(), json!(2)),
                ("ok".to_owned(), json!(true)),
                ("list".to_owned(), json!([{"k": "v"}])),
                ("none".to_owned(), Value::Null),
            ],
            preserved_values("[mcp_servers.y]\noauth = { client_secret = \"s\" }\n", "y"),
            "\n",
        );
        assert!(plain.contains("n = 1.5\ni = 2\nok = true\nlist = [{ k = \"v\" }]\nnone = \"\"\n"));
        assert!(plain.ends_with("[mcp_servers.y.oauth]\nclient_secret = \"s\"\n"));
        assert!(preserved_values("not toml [", "y").is_empty());
        assert!(preserved_values("[mcp_servers.y]\ncommand = \"x\"\n", "y").is_empty());
    }

    #[test]
    fn preserved_keys_fit_only_a_compatible_rendering() {
        let callback = ["oauth.callback_url", "oauth.callback_port"];
        let oauth_with_id = [
            ("url".to_owned(), json!("https://x.test/mcp")),
            ("oauth".to_owned(), json!({"client_id": "cid"})),
        ];
        assert!(preserved_key_fits(PRESERVED_CLIENT_SECRET, &oauth_with_id));
        assert!(
            callback
                .iter()
                .all(|key| preserved_key_fits(key, &oauth_with_id))
        );
        // OAuth without a client ID: Codex needs the ID next to a secret, but
        // dynamic client registration still uses the callback.
        let oauth_without_id = [("url".to_owned(), json!("https://x.test/mcp"))];
        assert!(!preserved_key_fits(
            PRESERVED_CLIENT_SECRET,
            &oauth_without_id
        ));
        assert!(
            callback
                .iter()
                .all(|key| preserved_key_fits(key, &oauth_without_id))
        );
        let stdio = [("command".to_owned(), json!("npx"))];
        let bearer = [
            ("url".to_owned(), json!("https://x.test/mcp")),
            ("bearer_token_env_var".to_owned(), json!("TOKEN")),
        ];
        let header = [
            ("url".to_owned(), json!("https://x.test/mcp")),
            ("env_http_headers".to_owned(), json!({"X-Key": "KEY"})),
        ];
        for entry in [&stdio[..], &bearer[..], &header[..]] {
            for key in PRESERVED_KEYS {
                assert!(!preserved_key_fits(key, entry), "{key} {entry:?}");
            }
        }
    }

    #[test]
    fn adds_use_the_file_line_ending_and_separate_with_a_blank_line() {
        let crlf = "# c\r\n[other]\r\nk = 1";
        let parsed = parse(crlf).unwrap();
        assert_eq!(parsed.eol, "\r\n");
        let new = entry_text(
            "n",
            &[("command".to_owned(), json!("x"))],
            Vec::new(),
            parsed.eol,
        );
        let out = apply_edits(crlf, &parsed, &[("n".to_owned(), Edit::Add(new))]);
        assert_eq!(
            out,
            "# c\r\n[other]\r\nk = 1\r\n\r\n[mcp_servers.n]\r\ncommand = \"x\"\r\n"
        );
        let empty = parse("").unwrap();
        let out = apply_edits(
            "",
            &empty,
            &[("n".to_owned(), Edit::Add("[x]\n".to_owned()))],
        );
        assert_eq!(out, "[x]\n");
        assert_eq!(line_ending("\nfoo\r\n"), "\n");
    }
}
