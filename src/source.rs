//! Source scanning: reads `servers/*.toml` of a source root into a
//! [`SourceSnapshot`] for the resolver.
//!
//! A directory source is scanned at its root. A Git source is scanned at its
//! snapshot, `<store>/sources/<id>/snapshot` (see [`snapshot`]), and only
//! when the snapshot's stamp equals the pin in `lock.toml`; a missing
//! snapshot, a missing pin or a stamp that differs makes it unavailable. No
//! `git` runs here. [`load_resolution`] is the one pipeline every command
//! resolves through.
//!
//! Only regular files are accepted; symlinks are not followed. Files over
//! 64 KiB are rejected. Hidden files, non-`.toml` files and subdirectories are
//! ignored and reported as notices. The conflict key is the file stem when it
//! passes the name rule, so a malformed `servers/foo.toml` still shadows
//! `foo`; a stem that breaks the name rule is a per-file problem and shadows
//! nothing.

pub mod git_source;
mod load;
pub mod refresh;
pub mod snapshot;

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{Config, LOCAL_SOURCE_ID, SourceKind, SourceOrigin};
use crate::definition::{OverrideSites, is_valid_name, parse_definition_with_sites};
use crate::git::CommitId;
use crate::lockfile::LockFile;
use crate::resolver::{FileEntry, FileProblem, Notice, SourceSnapshot};
use crate::rule::Rule;
use crate::store::{CappedReadError, StorePaths, read_regular_capped};

pub use load::{
    ReadInputs, load_resolution, load_snapshots, orphaned_approvals, prepare_snapshots,
    resolve_snapshots,
};

/// Maximum size of a definition file.
pub const MAX_DEFINITION_BYTES: u64 = 64 * 1024;

/// A source to scan: the built-in `local` source or a user source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// The id.
    pub id: String,
    /// The kind.
    pub kind: SourceKind,
    /// The scanned root; definitions live in `<root>/servers/`. For a Git
    /// source this is its snapshot, `<store>/sources/<id>/snapshot`.
    pub root: PathBuf,
    /// The priority.
    pub priority: u32,
    /// Whether this is the implicit built-in `local` source.
    pub builtin: bool,
    /// The Git fields and the pin of a Git source.
    pub git: Option<GitSpec>,
}

/// The Git side of a [`SourceSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSpec {
    /// The remote URL.
    pub url: String,
    /// The followed ref.
    pub reference: String,
    /// Whether the source is trusted.
    pub trusted: bool,
    /// The pinned commit from `lock.toml`; `None` when none is recorded.
    pub pin: Option<CommitId>,
}

impl SourceSpec {
    /// The `servers/` directory.
    #[must_use]
    pub fn servers(&self) -> PathBuf {
        self.root.join("servers")
    }

    /// The location shown to the user: the root of a directory source, or
    /// `<store>/sources/<id>` of a Git source.
    #[must_use]
    pub fn location(&self) -> &Path {
        match &self.git {
            Some(_) => self.root.parent().unwrap_or(&self.root),
            None => &self.root,
        }
    }

    /// Whether a Git source's snapshot is stamped with its pin; always true
    /// for a directory source.
    fn snapshot_current(&self) -> bool {
        match &self.git {
            None => true,
            Some(git) => git
                .pin
                .as_ref()
                .is_some_and(|pin| snapshot::stamp_matches(&self.root, pin)),
        }
    }
}

/// All sources of a store: the built-in `local` source (priority 0, root
/// `<store>/local`) followed by the user sources from `config.toml`, Git
/// sources with their pin from `lock`.
#[must_use]
pub fn source_specs(paths: &StorePaths, config: &Config, lock: &LockFile) -> Vec<SourceSpec> {
    let mut specs = vec![SourceSpec {
        id: LOCAL_SOURCE_ID.to_owned(),
        kind: SourceKind::Directory,
        root: paths.local_root.clone(),
        priority: 0,
        builtin: true,
        git: None,
    }];
    specs.extend(config.sources.iter().map(|source| {
        let (root, git) = match &source.origin {
            SourceOrigin::Directory { path } => (path.clone(), None),
            SourceOrigin::Git(git) => (
                paths.git_source(&source.id).join(git_source::SNAPSHOT_DIR),
                Some(GitSpec {
                    url: git.url.clone(),
                    reference: git.reference.clone(),
                    trusted: git.trusted,
                    pin: lock.pin(&source.id).cloned(),
                }),
            ),
        };
        SourceSpec {
            id: source.id.clone(),
            kind: source.kind(),
            root,
            priority: source.priority,
            builtin: false,
            git,
        }
    }));
    specs
}

/// Whether the source root and its `servers/` directory are readable
/// directories and, for a Git source, the snapshot matches the pin.
#[must_use]
pub fn is_available(spec: &SourceSpec) -> bool {
    fs::metadata(&spec.root).is_ok_and(|metadata| metadata.is_dir())
        && fs::read_dir(spec.servers()).is_ok()
        && spec.snapshot_current()
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
        revoked: std::collections::BTreeSet::new(),
    };
    if !fs::metadata(&spec.root).is_ok_and(|metadata| metadata.is_dir()) || !spec.snapshot_current()
    {
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
                findings: Vec::new(),
                gate: None,
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
            findings: Vec::new(),
            gate: None,
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
    // check is not followed, and one swapped for a FIFO does not block. The
    // handle is re-checked and the read capped, so a file swapped or grown
    // after the first check is still caught.
    let bytes = read_regular_capped(path, MAX_DEFINITION_BYTES).map_err(|error| match error {
        CappedReadError::NotRegular => FileProblem::file(Rule::NotRegularFile),
        CappedReadError::TooLarge => FileProblem::file(Rule::FileTooLarge),
        CappedReadError::Io(_) => FileProblem::file(Rule::Unreadable),
    })?;
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
            git: None,
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
            sources: vec![
                crate::config::SourceEntry {
                    id: "team".to_owned(),
                    priority: 3,
                    origin: SourceOrigin::Directory {
                        path: PathBuf::from("/srv/team"),
                    },
                },
                crate::config::SourceEntry {
                    id: "personal".to_owned(),
                    priority: 4,
                    origin: SourceOrigin::Git(crate::config::GitOrigin {
                        url: "/srv/git/personal.git".to_owned(),
                        reference: "main".to_owned(),
                        trusted: false,
                    }),
                },
            ],
            selections: Vec::new(),
        };
        let mut lock = LockFile::default();
        let pin = CommitId::parse(&"c".repeat(40)).unwrap();
        lock.set_pin("personal", pin.clone(), "refs/heads/main".to_owned());
        let specs = source_specs(&paths, &config, &lock);
        assert_eq!(specs[0].id, "local");
        assert_eq!(specs[0].priority, 0);
        assert!(specs[0].builtin);
        assert_eq!(specs[0].root, PathBuf::from("/store/local"));
        assert_eq!(specs[0].servers(), PathBuf::from("/store/local/servers"));
        assert_eq!(specs[1].id, "team");
        assert!(!specs[1].builtin);
        assert_eq!(specs[1].location(), Path::new("/srv/team"));
        assert_eq!(specs[2].kind, SourceKind::Git);
        assert_eq!(
            specs[2].root,
            PathBuf::from("/store/sources/personal/snapshot")
        );
        assert_eq!(specs[2].location(), Path::new("/store/sources/personal"));
        assert_eq!(specs[2].git.as_ref().unwrap().pin, Some(pin));
    }

    #[test]
    fn git_snapshots_are_read_only_when_stamped_with_the_pin() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("snapshot");
        fs::create_dir_all(root.join("servers")).unwrap();
        fs::write(root.join("servers/github.toml"), VALID).unwrap();
        let pin = CommitId::parse(&"d".repeat(40)).unwrap();
        let mut spec = SourceSpec {
            kind: SourceKind::Git,
            git: Some(GitSpec {
                url: "/srv/x.git".to_owned(),
                reference: "main".to_owned(),
                trusted: false,
                pin: None,
            }),
            ..spec(&root)
        };
        // No pin recorded.
        assert!(!scan(&spec).available);
        assert!(!is_available(&spec));
        spec.git.as_mut().unwrap().pin = Some(pin.clone());
        // No stamp.
        assert!(!scan(&spec).available);
        fs::write(
            root.join(snapshot::STAMP_FILE),
            format!("{}\n", "e".repeat(40)),
        )
        .unwrap();
        assert!(!scan(&spec).available);
        fs::write(root.join(snapshot::STAMP_FILE), format!("{pin}\n")).unwrap();
        let scanned = scan(&spec);
        assert!(scanned.available);
        assert!(is_available(&spec));
        assert_eq!(offered_names(&scanned), ["github"]);
    }
}
