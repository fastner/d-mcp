//! The `config.toml` model: user sources and selections.
//!
//! `config.toml` is validated strictly on load. Any deviation (unknown keys,
//! wrong types, duplicates, invalid names) makes the file *corrupt*, which is
//! an unsafe state (exit 3); d-mcp never rewrites a corrupt file. The built-in
//! `local` source is implicit and never persisted here. This module does no
//! I/O; [`crate::store`] reads and writes the file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml_edit::{ArrayOfTables, DocumentMut, Table, value};

use crate::definition::is_valid_name;
use crate::rule::Rule;
use crate::spanned::{self, Entry, Node, NodeKind};

/// The only supported `schema_version` of persisted files.
pub const SCHEMA_VERSION: i64 = 1;

/// Id of the built-in local source.
pub const LOCAL_SOURCE_ID: &str = "local";

/// The parsed `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// User sources; the built-in `local` source is not part of this list.
    pub sources: Vec<SourceEntry>,
    /// Selected servers, sorted by name.
    pub selections: Vec<Selection>,
}

/// The kind of a source. WP5 adds Git sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// A local directory.
    Directory,
}

impl SourceKind {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Directory => "directory",
        }
    }
}

/// A registered user source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    /// The source id; follows the neutral name rule.
    pub id: String,
    /// The source kind.
    pub kind: SourceKind,
    /// Absolute canonical path of the source root.
    pub path: PathBuf,
    /// Priority, at least 1 and unique. Higher wins.
    pub priority: u32,
}

/// A harness d-mcp can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl Harness {
    /// Every known harness, in a stable order.
    pub const ALL: [Harness; 2] = [Harness::Claude, Harness::Codex];

    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    fn from_str(text: &str) -> Option<Self> {
        match text {
            "claude" => Some(Harness::Claude),
            "codex" => Some(Harness::Codex),
            _ => None,
        }
    }
}

/// The harnesses of a selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessSet {
    /// Every harness, including adapters added later. Stored as `["all"]`.
    All,
    /// An explicit, non-empty set.
    Only(BTreeSet<Harness>),
}

impl HarnessSet {
    /// The stable list form: `["all"]` or the sorted harness names.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        match self {
            HarnessSet::All => vec!["all"],
            HarnessSet::Only(set) => set.iter().map(|harness| harness.as_str()).collect(),
        }
    }
}

impl Serialize for HarnessSet {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        self.names().serialize(serializer)
    }
}

/// A selected server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The server name.
    pub server: String,
    /// Target harnesses. The scope is always `user` in v1.
    pub harnesses: HarnessSet,
}

/// Why `config.toml` could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The file breaks a rule; it is reported by position only.
    Corrupt {
        /// The broken rule.
        rule: Rule,
        /// The field path.
        field: String,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
    },
    /// The file declares a `schema_version` other than 1.
    UnsupportedSchema {
        /// The declared version.
        found: i64,
    },
}

type Result<T> = std::result::Result<T, ConfigError>;

impl Config {
    /// Parses and validates `config.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnsupportedSchema`] for another schema version
    /// and [`ConfigError::Corrupt`] for any other deviation.
    pub fn parse(text: &str) -> Result<Self> {
        Parser { text }.config()
    }

    /// Renders the canonical `config.toml` text: sources sorted by id,
    /// selections sorted by server name.
    #[must_use]
    pub fn render(&self) -> String {
        let mut document = DocumentMut::new();
        document["schema_version"] = value(SCHEMA_VERSION);
        let mut sources: Vec<&SourceEntry> = self.sources.iter().collect();
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        if !sources.is_empty() {
            let mut array = ArrayOfTables::new();
            for source in sources {
                let mut table = Table::new();
                table["id"] = value(source.id.as_str());
                table["kind"] = value(source.kind.as_str());
                table["path"] = value(source.path.to_string_lossy().as_ref());
                table["priority"] = value(i64::from(source.priority));
                array.push(table);
            }
            document["sources"] = toml_edit::Item::ArrayOfTables(array);
        }
        let mut selections: Vec<&Selection> = self.selections.iter().collect();
        selections.sort_by(|a, b| a.server.cmp(&b.server));
        if !selections.is_empty() {
            let mut array = ArrayOfTables::new();
            for selection in selections {
                let mut table = Table::new();
                table["server"] = value(selection.server.as_str());
                let mut harnesses = toml_edit::Array::new();
                for name in selection.harnesses.names() {
                    harnesses.push(name);
                }
                table["harnesses"] = value(harnesses);
                array.push(table);
            }
            document["selections"] = toml_edit::Item::ArrayOfTables(array);
        }
        document.to_string()
    }

    /// Looks a user source up by id.
    #[must_use]
    pub fn source(&self, id: &str) -> Option<&SourceEntry> {
        self.sources.iter().find(|source| source.id == id)
    }

    /// Looks a selection up by server name.
    #[must_use]
    pub fn selection(&self, server: &str) -> Option<&Selection> {
        self.selections
            .iter()
            .find(|selection| selection.server == server)
    }

    /// Inserts or replaces a selection, keeping the list sorted. Returns
    /// whether an existing selection was replaced.
    pub fn select(&mut self, selection: Selection) -> bool {
        let replaced = self.unselect(&selection.server);
        self.selections.push(selection);
        self.selections.sort_by(|a, b| a.server.cmp(&b.server));
        replaced
    }

    /// Removes a selection. Returns whether it existed.
    pub fn unselect(&mut self, server: &str) -> bool {
        let before = self.selections.len();
        self.selections
            .retain(|selection| selection.server != server);
        self.selections.len() != before
    }
}

struct Parser<'t> {
    text: &'t str,
}

fn find<'e>(entries: &'e [Entry], key: &str) -> Option<&'e Entry> {
    entries.iter().find(|entry| entry.key == key)
}

impl Parser<'_> {
    fn corrupt(&self, rule: Rule, field: &str, offset: Option<usize>) -> ConfigError {
        let (line, column) = spanned::line_column(self.text, offset.unwrap_or(0));
        ConfigError::Corrupt {
            rule,
            field: field.to_owned(),
            line,
            column,
        }
    }

    fn check_keys(&self, entries: &[Entry], path: &str, allowed: &[&str]) -> Result<()> {
        match entries
            .iter()
            .find(|entry| !allowed.contains(&entry.key.as_str()))
        {
            // The key is not echoed: the field path names the table only.
            Some(entry) => Err(self.corrupt(Rule::UnknownKey, path, entry.key_offset())),
            None => Ok(()),
        }
    }

    fn required<'e>(
        &self,
        entries: &'e [Entry],
        field: &str,
        key: &str,
        parent: Option<usize>,
    ) -> Result<&'e Entry> {
        find(entries, key).ok_or_else(|| self.corrupt(Rule::MissingField, field, parent))
    }

    fn string<'n>(&self, node: &'n Node, field: &str) -> Result<&'n str> {
        match &node.kind {
            NodeKind::String(text) => Ok(text),
            _ => Err(self.corrupt(Rule::WrongType, field, node.offset())),
        }
    }

    fn tables<'n>(&self, node: &'n Node, field: &str) -> Result<Vec<(&'n [Entry], Option<usize>)>> {
        let NodeKind::Array(items) = &node.kind else {
            return Err(self.corrupt(Rule::WrongType, field, node.offset()));
        };
        items
            .iter()
            .enumerate()
            .map(|(position, item)| match &item.kind {
                NodeKind::Table(entries) => Ok((entries.as_slice(), item.offset())),
                _ => Err(self.corrupt(
                    Rule::WrongType,
                    &format!("{field}[{position}]"),
                    item.offset(),
                )),
            })
            .collect()
    }

    fn config(&self) -> Result<Config> {
        let root = spanned::parse_document(self.text)
            .map_err(|offset| self.corrupt(Rule::TomlSyntax, "", offset))?;
        let version_entry = self.required(&root, "schema_version", "schema_version", None)?;
        let NodeKind::Integer(version) = version_entry.value.kind else {
            return Err(self.corrupt(
                Rule::WrongType,
                "schema_version",
                version_entry.value_offset(),
            ));
        };
        if version != SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema { found: version });
        }
        self.check_keys(&root, "", &["schema_version", "sources", "selections"])?;
        let sources = match find(&root, "sources") {
            Some(entry) => self.sources(&entry.value)?,
            None => Vec::new(),
        };
        let mut selections = match find(&root, "selections") {
            Some(entry) => self.selections(&entry.value)?,
            None => Vec::new(),
        };
        selections.sort_by(|a, b| a.server.cmp(&b.server));
        Ok(Config {
            sources,
            selections,
        })
    }

    fn sources(&self, node: &Node) -> Result<Vec<SourceEntry>> {
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        let mut priorities = BTreeSet::new();
        let mut sources = Vec::new();
        for (position, (entries, offset)) in self.tables(node, "sources")?.into_iter().enumerate() {
            let base = format!("sources[{position}]");
            self.check_keys(entries, &base, &["id", "kind", "path", "priority"])?;
            let field = |key: &str| format!("{base}.{key}");

            let id_entry = self.required(entries, &field("id"), "id", offset)?;
            let id = self.string(&id_entry.value, &field("id"))?;
            if !is_valid_name(id) || id == LOCAL_SOURCE_ID {
                return Err(self.corrupt(Rule::NameRule, &field("id"), id_entry.value_offset()));
            }
            if !ids.insert(id.to_owned()) {
                return Err(self.corrupt(
                    Rule::DuplicateEntry,
                    &field("id"),
                    id_entry.value_offset(),
                ));
            }

            let kind_entry = self.required(entries, &field("kind"), "kind", offset)?;
            if self.string(&kind_entry.value, &field("kind"))? != "directory" {
                return Err(self.corrupt(
                    Rule::InvalidValue,
                    &field("kind"),
                    kind_entry.value_offset(),
                ));
            }

            let path_entry = self.required(entries, &field("path"), "path", offset)?;
            let path = self.string(&path_entry.value, &field("path"))?;
            if !Path::new(path).is_absolute() {
                return Err(self.corrupt(
                    Rule::InvalidValue,
                    &field("path"),
                    path_entry.value_offset(),
                ));
            }
            if !paths.insert(path.to_owned()) {
                return Err(self.corrupt(
                    Rule::DuplicateEntry,
                    &field("path"),
                    path_entry.value_offset(),
                ));
            }

            let priority_entry = self.required(entries, &field("priority"), "priority", offset)?;
            let NodeKind::Integer(priority) = priority_entry.value.kind else {
                return Err(self.corrupt(
                    Rule::WrongType,
                    &field("priority"),
                    priority_entry.value_offset(),
                ));
            };
            let priority = u32::try_from(priority)
                .ok()
                .filter(|priority| *priority >= 1)
                .ok_or_else(|| {
                    self.corrupt(
                        Rule::InvalidValue,
                        &field("priority"),
                        priority_entry.value_offset(),
                    )
                })?;
            if !priorities.insert(priority) {
                return Err(self.corrupt(
                    Rule::DuplicateEntry,
                    &field("priority"),
                    priority_entry.value_offset(),
                ));
            }

            sources.push(SourceEntry {
                id: id.to_owned(),
                kind: SourceKind::Directory,
                path: PathBuf::from(path),
                priority,
            });
        }
        Ok(sources)
    }

    fn selections(&self, node: &Node) -> Result<Vec<Selection>> {
        let mut servers = BTreeSet::new();
        let mut selections = Vec::new();
        for (position, (entries, offset)) in
            self.tables(node, "selections")?.into_iter().enumerate()
        {
            let base = format!("selections[{position}]");
            self.check_keys(entries, &base, &["server", "harnesses"])?;
            let server_field = format!("{base}.server");
            let server_entry = self.required(entries, &server_field, "server", offset)?;
            let server = self.string(&server_entry.value, &server_field)?;
            if !is_valid_name(server) {
                return Err(self.corrupt(
                    Rule::NameRule,
                    &server_field,
                    server_entry.value_offset(),
                ));
            }
            if !servers.insert(server.to_owned()) {
                return Err(self.corrupt(
                    Rule::DuplicateEntry,
                    &server_field,
                    server_entry.value_offset(),
                ));
            }
            let harness_field = format!("{base}.harnesses");
            let harness_entry = self.required(entries, &harness_field, "harnesses", offset)?;
            let harnesses = self.harnesses(&harness_entry.value, &harness_field)?;
            selections.push(Selection {
                server: server.to_owned(),
                harnesses,
            });
        }
        Ok(selections)
    }

    fn harnesses(&self, node: &Node, field: &str) -> Result<HarnessSet> {
        let NodeKind::Array(items) = &node.kind else {
            return Err(self.corrupt(Rule::WrongType, field, node.offset()));
        };
        let names = items
            .iter()
            .map(|item| self.string(item, field))
            .collect::<Result<Vec<_>>>()?;
        if names == ["all"] {
            return Ok(HarnessSet::All);
        }
        let mut set = BTreeSet::new();
        for (name, item) in names.iter().zip(items) {
            let harness = Harness::from_str(name)
                .ok_or_else(|| self.corrupt(Rule::InvalidValue, field, item.offset()))?;
            if !set.insert(harness) {
                return Err(self.corrupt(Rule::DuplicateEntry, field, item.offset()));
            }
        }
        if set.is_empty() {
            return Err(self.corrupt(Rule::InvalidValue, field, node.offset()));
        }
        Ok(HarnessSet::Only(set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corrupt_rule(text: &str) -> Rule {
        match Config::parse(text) {
            Err(ConfigError::Corrupt { rule, .. }) => rule,
            other => panic!("expected a corrupt config, got {other:?}"),
        }
    }

    #[test]
    fn empty_config_round_trips() {
        let config = Config::default();
        let text = config.render();
        assert_eq!(text, "schema_version = 1\n");
        assert_eq!(Config::parse(&text).unwrap(), config);
    }

    #[test]
    fn full_config_round_trips_sorted() {
        let mut config = Config {
            sources: vec![
                SourceEntry {
                    id: "team".to_owned(),
                    kind: SourceKind::Directory,
                    path: PathBuf::from("/srv/team \"defs\""),
                    priority: 10,
                },
                SourceEntry {
                    id: "alpha".to_owned(),
                    kind: SourceKind::Directory,
                    path: PathBuf::from("/srv/alpha"),
                    priority: 5,
                },
            ],
            selections: Vec::new(),
        };
        assert!(!config.select(Selection {
            server: "zeta".to_owned(),
            harnesses: HarnessSet::All,
        }));
        assert!(!config.select(Selection {
            server: "github".to_owned(),
            harnesses: HarnessSet::Only([Harness::Codex, Harness::Claude].into()),
        }));
        let text = config.render();
        let parsed = Config::parse(&text).unwrap();
        assert_eq!(parsed.selections, config.selections);
        assert_eq!(parsed.sources[0].id, "alpha");
        assert_eq!(parsed.sources[1].path, PathBuf::from("/srv/team \"defs\""));
        assert!(text.contains("harnesses = [\"claude\", \"codex\"]"));
        assert_eq!(parsed.source("team").unwrap().priority, 10);
        assert!(parsed.source("nope").is_none());
        assert_eq!(parsed.selection("zeta").unwrap().harnesses, HarnessSet::All);
        let mut parsed = parsed;
        assert!(parsed.select(Selection {
            server: "zeta".to_owned(),
            harnesses: HarnessSet::Only([Harness::Claude].into()),
        }));
        assert!(parsed.unselect("zeta"));
        assert!(!parsed.unselect("zeta"));
    }

    #[test]
    fn schema_version_is_checked_first() {
        assert_eq!(
            Config::parse("schema_version = 2\nbogus = 1\n"),
            Err(ConfigError::UnsupportedSchema { found: 2 })
        );
        assert_eq!(corrupt_rule("bogus = 1\n"), Rule::MissingField);
        assert_eq!(corrupt_rule("schema_version = \"1\"\n"), Rule::WrongType);
        assert_eq!(corrupt_rule("schema_version = \n"), Rule::TomlSyntax);
    }

    #[test]
    fn corrupt_configs() {
        let v = "schema_version = 1\n";
        let source = |body: &str| format!("{v}[[sources]]\n{body}\n");
        let ok = "id = \"team\"\nkind = \"directory\"\npath = \"/srv/team\"\npriority = 1";
        assert!(Config::parse(&source(ok)).is_ok());
        let cases = [
            (format!("{v}extra = 1\n"), Rule::UnknownKey),
            (format!("{v}sources = 1\n"), Rule::WrongType),
            (format!("{v}sources = [1]\n"), Rule::WrongType),
            (source(&format!("{ok}\nextra = 1")), Rule::UnknownKey),
            (source("kind = \"directory\""), Rule::MissingField),
            (source(&ok.replace("\"team\"", "1")), Rule::WrongType),
            (source(&ok.replace("\"team\"", "\"Team\"")), Rule::NameRule),
            (source(&ok.replace("\"team\"", "\"local\"")), Rule::NameRule),
            (source(&ok.replace("directory", "git")), Rule::InvalidValue),
            (
                source(&ok.replace("/srv/team", "srv/team")),
                Rule::InvalidValue,
            ),
            (
                source(&ok.replace("priority = 1", "priority = 0")),
                Rule::InvalidValue,
            ),
            (
                source(&ok.replace("priority = 1", "priority = -3")),
                Rule::InvalidValue,
            ),
            (
                source(&ok.replace("priority = 1", "priority = \"1\"")),
                Rule::WrongType,
            ),
            (
                format!("{}[[sources]]\n{ok}\n", source(ok)),
                Rule::DuplicateEntry,
            ),
            (
                format!(
                    "{}[[sources]]\n{}\n",
                    source(ok),
                    ok.replace("\"team\"", "\"other\"")
                ),
                Rule::DuplicateEntry,
            ),
            (
                format!(
                    "{}[[sources]]\n{}\n",
                    source(ok),
                    ok.replace("\"team\"", "\"other\"")
                        .replace("/srv/team", "/srv/other")
                ),
                Rule::DuplicateEntry,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\n"),
                Rule::MissingField,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\nharnesses = \"all\"\n"),
                Rule::WrongType,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\nharnesses = [1]\n"),
                Rule::WrongType,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\nharnesses = [\"gemini\"]\n"),
                Rule::InvalidValue,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\nharnesses = []\n"),
                Rule::InvalidValue,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\nharnesses = [\"codex\", \"codex\"]\n"),
                Rule::DuplicateEntry,
            ),
            (
                format!("{v}[[selections]]\nserver = \"A\"\nharnesses = [\"all\"]\n"),
                Rule::NameRule,
            ),
            (
                format!(
                    "{v}[[selections]]\nserver = \"a\"\nharnesses = [\"all\"]\n\
                     [[selections]]\nserver = \"a\"\nharnesses = [\"all\"]\n"
                ),
                Rule::DuplicateEntry,
            ),
            (
                format!("{v}[[selections]]\nserver = \"a\"\nharnesses = [\"all\"]\nx = 1\n"),
                Rule::UnknownKey,
            ),
        ];
        for (text, rule) in cases {
            assert_eq!(corrupt_rule(&text), rule, "{text}");
        }
    }

    #[test]
    fn names_and_serialization() {
        assert_eq!(HarnessSet::All.names(), ["all"]);
        assert_eq!(
            serde_json::to_string(&HarnessSet::Only([Harness::Codex].into())).unwrap(),
            "[\"codex\"]"
        );
        assert_eq!(SourceKind::Directory.as_str(), "directory");
        assert_eq!(
            serde_json::to_string(&SourceKind::Directory).unwrap(),
            "\"directory\""
        );
    }
}
