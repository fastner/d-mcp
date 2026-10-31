//! `lock.toml`: the pinned commit of every Git source.
//!
//! ```toml
//! schema_version = 1
//!
//! [[pins]]
//! source = "personal"
//! commit = "4f2a…"            # 40 or 64 lowercase hex characters
//! ref = "refs/heads/main"     # the followed ref, fully qualified
//! ```
//!
//! `ref` is the followed ref of the source's `config.toml` entry, qualified
//! when the source was added: `refs/heads/<ref>` for a branch,
//! `refs/tags/<ref>` for a tag. `source refresh` fetches exactly that ref,
//! so a tag that is pushed later with the name of a followed branch (or the
//! other way round) never replaces it. The key is optional within schema
//! version 1: a pin without it is qualified by the next refresh (a branch
//! wins over a tag of the same name, as in `git clone --branch`) and then
//! written with it.
//!
//! The file follows the `config.toml` rules: it is created on the first
//! write, written atomically under the store lock, and parsed strictly. Any
//! deviation makes it corrupt (`lock-corrupt`, exit 3), and d-mcp never
//! rewrites a corrupt file. A registered Git source always has a pin: it is
//! written before the source is added to `config.toml` and removed after the
//! source left it, so a crash in between leaves at most a stale pin of an
//! unregistered id, which the next `source add-git` of that id replaces.

use std::collections::BTreeMap;
use std::path::Path;

use toml_edit::{ArrayOfTables, DocumentMut, Table, value};

use crate::config::SCHEMA_VERSION;
use crate::definition::is_valid_name;
use crate::error::{Error, Result};
use crate::git::CommitId;
use crate::spanned::{self, Entry, NodeKind};
use crate::store::{StoreLock, StorePaths, StoreWarning, write_atomic};

/// The parsed `lock.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockFile {
    pins: BTreeMap<String, Pin>,
}

/// One pin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    commit: CommitId,
    /// The fully qualified followed ref; `None` in a pin written before it
    /// was recorded.
    reference: Option<String>,
}

/// Whether `reference` is a fully qualified branch or tag:
/// `refs/heads/<name>` or `refs/tags/<name>` with a `<name>` that passes
/// [`crate::git::validate_ref_syntax`].
#[must_use]
pub fn is_qualified_ref(reference: &str) -> bool {
    reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/tags/"))
        .is_some_and(|name| crate::git::validate_ref_syntax(name).is_ok())
}

/// A rule break: the field path and the byte offset.
struct Corrupt {
    field: String,
    offset: Option<usize>,
}

fn corrupt<T>(field: impl Into<String>, offset: Option<usize>) -> std::result::Result<T, Corrupt> {
    Err(Corrupt {
        field: field.into(),
        offset,
    })
}

fn find<'e>(entries: &'e [Entry], key: &str) -> Option<&'e Entry> {
    entries.iter().find(|entry| entry.key == key)
}

impl LockFile {
    /// Parses `lock.toml` text.
    ///
    /// # Errors
    ///
    /// [`Error::LockCorrupt`] or [`Error::SchemaUnsupported`], naming `path`.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        match parse(text) {
            Ok(lock) => Ok(lock),
            Err(Ok(found)) => Err(Error::SchemaUnsupported {
                path: path.to_path_buf(),
                found,
            }),
            Err(Err(Corrupt { field, offset })) => {
                let (line, column) = spanned::line_column(text, offset.unwrap_or(0));
                Err(Error::LockCorrupt {
                    path: path.to_path_buf(),
                    line,
                    column,
                    field,
                })
            }
        }
    }

    /// Renders the canonical text, pins sorted by source id.
    #[must_use]
    pub fn render(&self) -> String {
        let mut document = DocumentMut::new();
        document["schema_version"] = value(SCHEMA_VERSION);
        if !self.pins.is_empty() {
            let mut array = ArrayOfTables::new();
            for (source, pin) in &self.pins {
                let mut table = Table::new();
                table["source"] = value(source.as_str());
                table["commit"] = value(pin.commit.as_str());
                if let Some(reference) = &pin.reference {
                    table["ref"] = value(reference.as_str());
                }
                array.push(table);
            }
            document["pins"] = toml_edit::Item::ArrayOfTables(array);
        }
        document.to_string()
    }

    /// The pinned commit of a source.
    #[must_use]
    pub fn pin(&self, source: &str) -> Option<&CommitId> {
        self.pins.get(source).map(|pin| &pin.commit)
    }

    /// The fully qualified followed ref of a source, when it is recorded.
    #[must_use]
    pub fn followed_ref(&self, source: &str) -> Option<&str> {
        self.pins.get(source)?.reference.as_deref()
    }

    /// Sets the pin of a source and its fully qualified followed ref (which
    /// must pass [`is_qualified_ref`]); returns the previous commit.
    pub fn set_pin(
        &mut self,
        source: &str,
        commit: CommitId,
        reference: String,
    ) -> Option<CommitId> {
        debug_assert!(is_qualified_ref(&reference), "{reference}");
        self.pins
            .insert(
                source.to_owned(),
                Pin {
                    commit,
                    reference: Some(reference),
                },
            )
            .map(|pin| pin.commit)
    }

    /// Removes the pin of a source; returns its commit.
    pub fn remove_pin(&mut self, source: &str) -> Option<CommitId> {
        self.pins.remove(source).map(|pin| pin.commit)
    }

    /// Every pin, sorted by source id.
    pub fn pins(&self) -> impl Iterator<Item = (&str, &CommitId)> {
        self.pins
            .iter()
            .map(|(source, pin)| (source.as_str(), &pin.commit))
    }
}

/// `Err(Ok(version))` for an unsupported schema version.
fn parse(text: &str) -> std::result::Result<LockFile, std::result::Result<i64, Corrupt>> {
    let root = spanned::parse_document(text).map_err(|offset| {
        Err(Corrupt {
            field: String::new(),
            offset,
        })
    })?;
    let Some(version) = find(&root, "schema_version") else {
        return Err(corrupt("schema_version", None));
    };
    let NodeKind::Integer(found) = version.value.kind else {
        return Err(corrupt("schema_version", version.value_offset()));
    };
    if found != SCHEMA_VERSION {
        return Err(Ok(found));
    }
    parse_pins(&root).map_err(Err)
}

fn parse_pins(root: &[Entry]) -> std::result::Result<LockFile, Corrupt> {
    if let Some(entry) = root
        .iter()
        .find(|entry| !["schema_version", "pins"].contains(&entry.key.as_str()))
    {
        return corrupt("", entry.key_offset());
    }
    let mut lock = LockFile::default();
    let Some(pins) = find(root, "pins") else {
        return Ok(lock);
    };
    let NodeKind::Array(items) = &pins.value.kind else {
        return corrupt("pins", pins.value_offset());
    };
    for (position, item) in items.iter().enumerate() {
        let base = format!("pins[{position}]");
        let NodeKind::Table(entries) = &item.kind else {
            return corrupt(base, item.offset());
        };
        if let Some(entry) = entries
            .iter()
            .find(|entry| !["source", "commit", "ref"].contains(&entry.key.as_str()))
        {
            return corrupt(base, entry.key_offset());
        }
        let string = |key: &str| match find(entries, key) {
            Some(Entry {
                value:
                    spanned::Node {
                        kind: NodeKind::String(text),
                        ..
                    },
                ..
            }) => Ok(text.as_str()),
            Some(entry) => corrupt(format!("{base}.{key}"), entry.value_offset()),
            None => corrupt(format!("{base}.{key}"), item.offset()),
        };
        let source = string("source")?;
        if !is_valid_name(source) {
            return corrupt(
                format!("{base}.source"),
                find(entries, "source").and_then(Entry::value_offset),
            );
        }
        let Some(commit) = CommitId::parse(string("commit")?) else {
            return corrupt(
                format!("{base}.commit"),
                find(entries, "commit").and_then(Entry::value_offset),
            );
        };
        let reference = match find(entries, "ref") {
            None => None,
            Some(_) => {
                let reference = string("ref")?;
                if !is_qualified_ref(reference) {
                    return corrupt(
                        format!("{base}.ref"),
                        find(entries, "ref").and_then(Entry::value_offset),
                    );
                }
                Some(reference.to_owned())
            }
        };
        if lock
            .pins
            .insert(source.to_owned(), Pin { commit, reference })
            .is_some()
        {
            return corrupt(format!("{base}.source"), item.offset());
        }
    }
    Ok(lock)
}

/// Loads `lock.toml`; a missing file has no pins.
///
/// # Errors
///
/// [`Error::LockCorrupt`] (also for a file that is not UTF-8),
/// [`Error::SchemaUnsupported`] and [`Error::Io`].
pub fn load(paths: &StorePaths) -> Result<LockFile> {
    let bytes = match std::fs::read(&paths.pins) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LockFile::default());
        }
        Err(error) => return Err(Error::io("read", &paths.pins, &error)),
    };
    let text = String::from_utf8(bytes).map_err(|_| Error::LockCorrupt {
        path: paths.pins.clone(),
        line: 1,
        column: 1,
        field: String::new(),
    })?;
    LockFile::parse(&text, &paths.pins)
}

/// Writes `lock.toml` atomically. Call only while holding the lock.
///
/// # Errors
///
/// [`Error::Io`] when the write fails; the old file then stays intact.
pub fn save(
    paths: &StorePaths,
    _lock: &StoreLock,
    lock: &LockFile,
) -> Result<Option<StoreWarning>> {
    write_atomic(&paths.pins, lock.render().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(c: char) -> CommitId {
        CommitId::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn main() -> String {
        "refs/heads/main".to_owned()
    }

    fn field_of(text: &str) -> String {
        match LockFile::parse(text, Path::new("/s/lock.toml")) {
            Err(Error::LockCorrupt { field, .. }) => field,
            other => panic!("expected lock-corrupt, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_sorted() {
        let mut lock = LockFile::default();
        assert_eq!(lock.render(), "schema_version = 1\n");
        assert!(lock.set_pin("zeta", commit('b'), main()).is_none());
        assert!(
            lock.set_pin("alpha", commit('a'), "refs/tags/v1".to_owned())
                .is_none()
        );
        assert_eq!(lock.set_pin("zeta", commit('c'), main()), Some(commit('b')));
        let text = lock.render();
        assert!(text.find("alpha").unwrap() < text.find("zeta").unwrap());
        let parsed = LockFile::parse(&text, Path::new("/s/lock.toml")).unwrap();
        assert_eq!(parsed, lock);
        assert_eq!(parsed.pin("zeta"), Some(&commit('c')));
        assert_eq!(parsed.followed_ref("zeta"), Some("refs/heads/main"));
        assert_eq!(parsed.followed_ref("alpha"), Some("refs/tags/v1"));
        assert!(text.contains("ref = \"refs/tags/v1\""), "{text}");
        assert_eq!(parsed.pins().count(), 2);
        let mut parsed = parsed;
        assert_eq!(parsed.remove_pin("alpha"), Some(commit('a')));
        assert_eq!(parsed.remove_pin("alpha"), None);
        assert_eq!(parsed.pin("alpha"), None);
    }

    #[test]
    fn strict_parse() {
        let v = "schema_version = 1\n";
        let sha = "a".repeat(40);
        let pin = |body: &str| format!("{v}[[pins]]\n{body}\n");
        // A pin without `ref` (written before it was recorded) is valid.
        let legacy = LockFile::parse(
            &pin(&format!("source = \"p\"\ncommit = \"{sha}\"")),
            Path::new("x"),
        )
        .unwrap();
        assert_eq!(legacy.followed_ref("p"), None);
        assert_eq!(legacy.pin("p").map(CommitId::as_str), Some(sha.as_str()));
        // It renders without `ref` too.
        assert!(!legacy.render().contains("ref"));
        assert!(
            LockFile::parse(
                &pin(&format!(
                    "source = \"p\"\ncommit = \"{sha}\"\nref = \"refs/tags/v1.2\""
                )),
                Path::new("x")
            )
            .is_ok()
        );
        let cases = [
            (format!("{v}x = 1\n"), ""),
            ("schema_version = \"1\"\n".to_owned(), "schema_version"),
            ("pins = []\n".to_owned(), "schema_version"),
            ("schema_version = \n".to_owned(), ""),
            (format!("{v}pins = 1\n"), "pins"),
            (format!("{v}pins = [1]\n"), "pins[0]"),
            (
                pin(&format!("source = \"p\"\ncommit = \"{sha}\"\nx = 1")),
                "pins[0]",
            ),
            (pin(&format!("commit = \"{sha}\"")), "pins[0].source"),
            (
                pin(&format!("source = 1\ncommit = \"{sha}\"")),
                "pins[0].source",
            ),
            (
                pin(&format!("source = \"Bad\"\ncommit = \"{sha}\"")),
                "pins[0].source",
            ),
            (pin("source = \"p\"\ncommit = \"main\""), "pins[0].commit"),
            (
                pin(&format!(
                    "source = \"p\"\ncommit = \"{sha}\"\nref = \"main\""
                )),
                "pins[0].ref",
            ),
            (
                pin(&format!(
                    "source = \"p\"\ncommit = \"{sha}\"\nref = \"refs/remotes/o/main\""
                )),
                "pins[0].ref",
            ),
            (
                pin(&format!(
                    "source = \"p\"\ncommit = \"{sha}\"\nref = \"refs/heads/\""
                )),
                "pins[0].ref",
            ),
            (
                pin(&format!("source = \"p\"\ncommit = \"{sha}\"\nref = 1")),
                "pins[0].ref",
            ),
            (
                format!(
                    "{}[[pins]]\nsource = \"p\"\ncommit = \"{sha}\"\n",
                    pin(&format!("source = \"p\"\ncommit = \"{sha}\""))
                ),
                "pins[1].source",
            ),
        ];
        for (text, field) in cases {
            assert_eq!(field_of(&text), field, "{text}");
        }
        assert!(matches!(
            LockFile::parse("schema_version = 2\n", Path::new("x")),
            Err(Error::SchemaUnsupported { found: 2, .. })
        ));
    }

    #[test]
    fn load_and_save() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        assert_eq!(load(&paths).unwrap(), LockFile::default());
        let store_lock = StoreLock::acquire(&paths).unwrap();
        let mut lock = LockFile::default();
        lock.set_pin("p", commit('d'), main());
        save(&paths, &store_lock, &lock).unwrap();
        assert_eq!(load(&paths).unwrap(), lock);
        std::fs::write(&paths.pins, b"\xff").unwrap();
        assert!(matches!(load(&paths), Err(Error::LockCorrupt { .. })));
        std::fs::remove_file(&paths.pins).unwrap();
        std::fs::create_dir(&paths.pins).unwrap();
        assert!(matches!(load(&paths), Err(Error::Io { .. })));
    }
}
