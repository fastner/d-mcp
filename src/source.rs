//! Source scanning: reads `servers/*.toml` of a source root into a
//! [`SourceSnapshot`] for the resolver.
//!
//! Only regular files are accepted; symlinks are not followed. Files over
//! 64 KiB are rejected. Hidden files, non-`.toml` files and subdirectories are
//! ignored and reported as notices. The conflict key is the file stem when it
//! passes the name rule, so a malformed `servers/foo.toml` still shadows
//! `foo`; a stem that breaks the name rule is a per-file problem and shadows
//! nothing.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::{Config, LOCAL_SOURCE_ID, SourceKind};
use crate::definition::{OverrideSites, is_valid_name, parse_definition_with_sites};
use crate::resolver::{FileEntry, FileProblem, Notice, SourceSnapshot};
use crate::rule::Rule;
use crate::store::StorePaths;

/// Maximum size of a definition file.
pub const MAX_DEFINITION_BYTES: u64 = 64 * 1024;

/// A source to scan: the built-in `local` source or a user source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// The id.
    pub id: String,
    /// The kind.
    pub kind: SourceKind,
    /// The source root; definitions live in `<root>/servers/`.
    pub root: PathBuf,
    /// The priority.
    pub priority: u32,
    /// Whether this is the implicit built-in `local` source.
    pub builtin: bool,
}

impl SourceSpec {
    /// The `servers/` directory.
    #[must_use]
    pub fn servers(&self) -> PathBuf {
        self.root.join("servers")
    }
}

/// All sources of a store: the built-in `local` source (priority 0, root
/// `<store>/local`) followed by the user sources from `config.toml`.
#[must_use]
pub fn source_specs(paths: &StorePaths, config: &Config) -> Vec<SourceSpec> {
    let mut specs = vec![SourceSpec {
        id: LOCAL_SOURCE_ID.to_owned(),
        kind: SourceKind::Directory,
        root: paths.local_root.clone(),
        priority: 0,
        builtin: true,
    }];
    specs.extend(config.sources.iter().map(|source| SourceSpec {
        id: source.id.clone(),
        kind: source.kind,
        root: source.path.clone(),
        priority: source.priority,
        builtin: false,
    }));
    specs
}

/// Whether the source root and its `servers/` directory are readable
/// directories.
#[must_use]
pub fn is_available(spec: &SourceSpec) -> bool {
    fs::metadata(&spec.root).is_ok_and(|metadata| metadata.is_dir())
        && fs::read_dir(spec.servers()).is_ok()
}

/// Scans a source. An unavailable source yields an empty snapshot with
/// `available = false`.
#[must_use]
pub fn scan(spec: &SourceSpec) -> SourceSnapshot {
    let mut snapshot = SourceSnapshot {
        id: spec.id.clone(),
        priority: spec.priority,
        available: false,
        files: Vec::new(),
        notices: Vec::new(),
    };
    if !fs::metadata(&spec.root).is_ok_and(|metadata| metadata.is_dir()) {
        return snapshot;
    }
    let servers = spec.servers();
    let Ok(entries) = fs::read_dir(&servers) else {
        return snapshot;
    };
    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return snapshot;
        };
        names.push(entry.file_name());
    }
    names.sort();
    snapshot.available = true;
    for name in names {
        let display = format!("servers/{}", name.to_string_lossy());
        let path = servers.join(&name);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            snapshot.files.push(FileEntry {
                file: display,
                key: None,
                outcome: Err(FileProblem::file(Rule::Unreadable)),
                override_sites: OverrideSites::default(),
            });
            continue;
        };
        let name_text = name.to_string_lossy();
        if name_text.starts_with('.') {
            snapshot.notices.push(Notice {
                file: display,
                rule: Rule::HiddenFile,
            });
            continue;
        }
        if metadata.is_dir() {
            snapshot.notices.push(Notice {
                file: display,
                rule: Rule::Subdirectory,
            });
            continue;
        }
        let Some(stem) = name_text.strip_suffix(".toml") else {
            snapshot.notices.push(Notice {
                file: display,
                rule: Rule::NonTomlFile,
            });
            continue;
        };
        // A lossy (non-UTF-8) name can never pass the name rule.
        let key = (name.to_str().is_some() && is_valid_name(stem)).then(|| stem.to_owned());
        let parsed = match &key {
            None => Err(FileProblem::file(Rule::InvalidStem)),
            Some(stem) => read_definition(&path, &metadata, stem),
        };
        let (outcome, override_sites) = match parsed {
            Ok((definition, sites)) => (Ok(definition), sites),
            Err(problem) => (Err(problem), OverrideSites::default()),
        };
        snapshot.files.push(FileEntry {
            file: display,
            key,
            outcome,
            override_sites,
        });
    }
    snapshot
}

/// The set of names a snapshot offers (valid or not).
#[must_use]
pub fn offered_names(snapshot: &SourceSnapshot) -> Vec<String> {
    snapshot
        .files
        .iter()
        .filter_map(|file| file.key.clone())
        .collect()
}

fn read_definition(
    path: &Path,
    metadata: &fs::Metadata,
    stem: &str,
) -> Result<(crate::definition::Definition, OverrideSites), FileProblem> {
    if !metadata.is_file() {
        return Err(FileProblem::file(Rule::NotRegularFile));
    }
    if metadata.len() > MAX_DEFINITION_BYTES {
        return Err(FileProblem::file(Rule::FileTooLarge));
    }
    // `O_NOFOLLOW | O_NONBLOCK`: a file swapped for a symlink after the first
    // check is not followed, and one swapped for a FIFO does not block.
    let mut options = fs::OpenOptions::new();
    options.read(true);
    crate::store::no_follow(&mut options, true);
    let file = options.open(path).map_err(|_| {
        // A refused symlink is reported as such; anything else is unreadable.
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            FileProblem::file(Rule::NotRegularFile)
        } else {
            FileProblem::file(Rule::Unreadable)
        }
    })?;
    // Re-check through the opened handle and cap the read, so a file swapped
    // or grown after the first check is still caught.
    let handle_metadata = file
        .metadata()
        .map_err(|_| FileProblem::file(Rule::Unreadable))?;
    if !handle_metadata.is_file() {
        return Err(FileProblem::file(Rule::NotRegularFile));
    }
    let mut bytes = Vec::new();
    file.take(MAX_DEFINITION_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FileProblem::file(Rule::Unreadable))?;
    if bytes.len() as u64 > MAX_DEFINITION_BYTES {
        return Err(FileProblem::file(Rule::FileTooLarge));
    }
    let text = String::from_utf8(bytes).map_err(|_| FileProblem::file(Rule::NotUtf8))?;
    parse_definition_with_sites(&text, stem).map_err(FileProblem::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n";

    fn spec(root: &Path) -> SourceSpec {
        SourceSpec {
            id: "team".to_owned(),
            kind: SourceKind::Directory,
            root: root.to_path_buf(),
            priority: 1,
            builtin: false,
        }
    }

    fn rule_of(snapshot: &SourceSnapshot, file: &str) -> Option<Rule> {
        snapshot
            .files
            .iter()
            .find(|entry| entry.file == file)
            .and_then(|entry| entry.outcome.as_ref().err().map(|problem| problem.rule))
    }

    #[test]
    fn missing_root_or_servers_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let missing = spec(&temp.path().join("missing"));
        assert!(!scan(&missing).available);
        assert!(!is_available(&missing));
        let no_servers = spec(temp.path());
        assert!(!scan(&no_servers).available);
        fs::create_dir(temp.path().join("servers")).unwrap();
        assert!(scan(&no_servers).available);
        assert!(is_available(&no_servers));
    }

    #[test]
    fn scan_classifies_entries() {
        let temp = tempfile::tempdir().unwrap();
        let servers = temp.path().join("servers");
        fs::create_dir(&servers).unwrap();
        fs::write(servers.join("github.toml"), VALID).unwrap();
        fs::write(servers.join("broken.toml"), "name = ").unwrap();
        fs::write(servers.join("Bad__Stem.toml"), VALID).unwrap();
        fs::write(servers.join(".hidden.toml"), VALID).unwrap();
        fs::write(servers.join("notes.md"), "x").unwrap();
        fs::create_dir(servers.join("nested")).unwrap();
        fs::write(
            servers.join("large.toml"),
            vec![b'#'; usize::try_from(MAX_DEFINITION_BYTES).unwrap() + 1],
        )
        .unwrap();
        fs::write(servers.join("binary.toml"), b"\xff\xfe").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(servers.join("github.toml"), servers.join("link.toml"))
                .unwrap();
        }
        let snapshot = scan(&spec(temp.path()));
        assert!(snapshot.available);
        let ok = snapshot
            .files
            .iter()
            .find(|entry| entry.file == "servers/github.toml")
            .unwrap();
        assert!(ok.outcome.is_ok());
        assert_eq!(ok.key.as_deref(), Some("github"));
        assert_eq!(
            rule_of(&snapshot, "servers/broken.toml"),
            Some(Rule::TomlSyntax)
        );
        assert_eq!(
            rule_of(&snapshot, "servers/Bad__Stem.toml"),
            Some(Rule::InvalidStem)
        );
        assert_eq!(
            rule_of(&snapshot, "servers/large.toml"),
            Some(Rule::FileTooLarge)
        );
        assert_eq!(
            rule_of(&snapshot, "servers/binary.toml"),
            Some(Rule::NotUtf8)
        );
        #[cfg(unix)]
        assert_eq!(
            rule_of(&snapshot, "servers/link.toml"),
            Some(Rule::NotRegularFile)
        );
        let notices: Vec<(&str, Rule)> = snapshot
            .notices
            .iter()
            .map(|notice| (notice.file.as_str(), notice.rule))
            .collect();
        assert_eq!(
            notices,
            [
                ("servers/.hidden.toml", Rule::HiddenFile),
                ("servers/nested", Rule::Subdirectory),
                ("servers/notes.md", Rule::NonTomlFile),
            ]
        );
        let mut offered = offered_names(&snapshot);
        offered.sort();
        #[cfg(unix)]
        assert_eq!(offered, ["binary", "broken", "github", "large", "link"]);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_not_a_regular_file() {
        let temp = tempfile::tempdir().unwrap();
        let servers = temp.path().join("servers");
        fs::create_dir(&servers).unwrap();
        let fifo = servers.join("pipe.toml");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !status.is_ok_and(|status| status.success()) {
            return;
        }
        let snapshot = scan(&spec(temp.path()));
        assert_eq!(
            rule_of(&snapshot, "servers/pipe.toml"),
            Some(Rule::NotRegularFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn swapped_symlink_or_fifo_is_neither_followed_nor_waited_on() {
        let temp = tempfile::tempdir().unwrap();
        let regular = temp.path().join("github.toml");
        fs::write(&regular, VALID).unwrap();
        let metadata = fs::symlink_metadata(&regular).unwrap();
        // The first check saw a regular file; the path is a symlink by the
        // time it is opened.
        let link = temp.path().join("link.toml");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        assert_eq!(
            read_definition(&link, &metadata, "github")
                .unwrap_err()
                .rule,
            Rule::NotRegularFile
        );
        let fifo = temp.path().join("pipe.toml");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        if status.is_ok_and(|status| status.success()) {
            // Without `O_NONBLOCK` this open would wait for a writer forever.
            assert_eq!(
                read_definition(&fifo, &metadata, "github")
                    .unwrap_err()
                    .rule,
                Rule::NotRegularFile
            );
        }
        let missing = temp.path().join("missing.toml");
        assert_eq!(
            read_definition(&missing, &metadata, "github")
                .unwrap_err()
                .rule,
            Rule::Unreadable
        );
        assert!(read_definition(&regular, &metadata, "github").is_ok());
    }

    #[test]
    fn specs_start_with_the_builtin_local_source() {
        let paths = StorePaths::new(PathBuf::from("/store"));
        let config = Config {
            sources: vec![crate::config::SourceEntry {
                id: "team".to_owned(),
                kind: SourceKind::Directory,
                path: PathBuf::from("/srv/team"),
                priority: 3,
            }],
            selections: Vec::new(),
        };
        let specs = source_specs(&paths, &config);
        assert_eq!(specs[0].id, "local");
        assert_eq!(specs[0].priority, 0);
        assert!(specs[0].builtin);
        assert_eq!(specs[0].root, PathBuf::from("/store/local"));
        assert_eq!(specs[0].servers(), PathBuf::from("/store/local/servers"));
        assert_eq!(specs[1].id, "team");
        assert!(!specs[1].builtin);
    }
}
