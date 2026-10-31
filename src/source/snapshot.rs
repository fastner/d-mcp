//! Git snapshots: the definitions of a pinned commit, materialized into
//! `sources/<id>/snapshot/servers/` with a commit stamp, so the unchanged
//! directory scanner reads them at read time and no read-only command runs
//! `git`.
//!
//! The tree `<commit>:servers` is listed non-recursively and every entry is
//! written so that the scanner classifies it as it would classify the same
//! entry in a directory source:
//!
//! - a hidden entry, a non-`.toml` file or a `.toml` file whose stem breaks
//!   the name rule becomes an empty file (a `hidden-file` or `non-toml-file`
//!   notice, or an `invalid-stem` problem);
//! - a subdirectory becomes an empty directory (`subdirectory` notice);
//! - a symlink (`120000`), a gitlink (`160000`) or any other non-blob entry
//!   with a valid `.toml` name becomes a dangling symlink, which the scanner
//!   never follows (`not-regular-file`);
//! - a blob is sized from the listing (`ls-tree --long`) before it is read:
//!   over 64 KiB it becomes a sparse file of 64 KiB + 1 byte
//!   (`file-too-large`), otherwise its bytes are written as they are
//!   (non-UTF-8 stays `not-utf8`).
//!
//! The definitions are written first, then the placeholders of `.toml`
//! files with a valid stem (which carry a conflict key), then the rest. On
//! a case-insensitive file system a placeholder such as `GitHub.toml`
//! (`invalid-stem`) or `GITHUB.TOML` (`non-toml-file`) names the same file
//! as the definition `github.toml` (or the keyed placeholder of an
//! oversized `github.toml`); it is then skipped, so the keyed entry is kept
//! and only that placeholder's notice or problem is not reported.
//!
//! **Limits:** `servers/` may hold at most [`MAX_ENTRIES`] entries (more is
//! `git-failed`, nothing is written), so a snapshot starts at most
//! [`MAX_ENTRIES`] `git cat-file` calls, one per definition.
//!
//! Nothing outside the repository is read: blobs come from the object
//! store, never from a checkout. A commit without a `servers` tree gets a
//! snapshot without `servers/`, which makes the source unavailable.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::definition::is_valid_name;
use crate::error::{Error, Result};
use crate::git::{CommitId, Git, TreeEntry};
use crate::source::MAX_DEFINITION_BYTES;
use crate::store::{create_dir, read_regular_capped, write_atomic};

/// The stamp file in a snapshot: the commit it was materialized from.
pub const STAMP_FILE: &str = "commit";
/// The `servers` directory in a snapshot and in the repository.
const SERVERS: &str = "servers";
/// The target of the dangling symlink that stands for a non-regular entry.
const NOT_REGULAR_TARGET: &str = "d-mcp-not-a-regular-file";
/// The maximum size of a stamp file.
const STAMP_MAX: u64 = 160;
/// The maximum number of entries in the `servers` tree of a snapshot.
pub const MAX_ENTRIES: usize = 1000;

/// Whether the snapshot at `root` is stamped with `pin`.
#[must_use]
pub fn stamp_matches(root: &Path, pin: &CommitId) -> bool {
    read_regular_capped(&root.join(STAMP_FILE), STAMP_MAX)
        .is_ok_and(|bytes| bytes == format!("{pin}\n").as_bytes())
}

/// What a tree entry is written as.
enum Written {
    /// The blob's bytes.
    Bytes(Vec<u8>),
    /// An empty regular file.
    Empty,
    /// An empty directory.
    Directory,
    /// A dangling symlink.
    NotRegular,
    /// A sparse file one byte over the size cap.
    TooLarge,
}

/// Materializes `<commit>:servers` of the repository at `git_dir` into
/// `target`, which must not exist yet, and stamps it with `commit`. The
/// caller renames `target` into place.
///
/// # Errors
///
/// A git error, or [`Error::Io`] when a file cannot be written.
pub fn materialize(git: &Git, git_dir: &Path, commit: &CommitId, target: &Path) -> Result<()> {
    create_dir(target)?;
    // The root listing tells a missing `servers` tree from a failed call.
    let has_servers = git
        .ls_tree(git_dir, commit, "")?
        .iter()
        .any(|entry| entry.name == SERVERS.as_bytes() && entry.kind == "tree");
    if has_servers {
        let entries = git.ls_tree(git_dir, commit, SERVERS)?;
        if entries.len() > MAX_ENTRIES {
            return Err(Error::GitFailed {
                operation: "ls-tree",
                status: format!("servers/ has more than {MAX_ENTRIES} entries"),
            });
        }
        let servers = target.join(SERVERS);
        create_dir(&servers)?;
        let mut placeholders = Vec::new();
        for entry in entries {
            let Some(name) = safe_name(&entry.name) else {
                continue;
            };
            match classify(git, git_dir, &entry)? {
                Written::Bytes(bytes) => write_entry(&servers.join(name), Written::Bytes(bytes))?,
                placeholder => placeholders.push((servers.join(name), placeholder)),
            }
        }
        write_placeholders(placeholders)?;
    }
    write_atomic(&target.join(STAMP_FILE), format!("{commit}\n").as_bytes())?;
    Ok(())
}

/// The entry name as a path component; `None` for a name that cannot be
/// one (empty, `.`, `..`, or with `/` or NUL), which fsck never lets into
/// a tree anyway.
fn safe_name(name: &[u8]) -> Option<PathBuf> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(name)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(name).ok().map(PathBuf::from)
    }
}

/// Classifies an entry in the scanner's order: hidden, subdirectory,
/// non-`.toml`, invalid stem, then the file itself.
fn classify(git: &Git, git_dir: &Path, entry: &TreeEntry) -> Result<Written> {
    if entry.name.starts_with(b".") {
        return Ok(Written::Empty);
    }
    if entry.mode == "040000" || entry.kind == "tree" {
        return Ok(Written::Directory);
    }
    let Some(stem) = entry.name.strip_suffix(b".toml") else {
        return Ok(Written::Empty);
    };
    if !std::str::from_utf8(stem).is_ok_and(is_valid_name) {
        return Ok(Written::Empty);
    }
    if !matches!(entry.mode.as_str(), "100644" | "100755") || entry.kind != "blob" {
        return Ok(Written::NotRegular);
    }
    if entry.size.is_none_or(|size| size > MAX_DEFINITION_BYTES) {
        return Ok(Written::TooLarge);
    }
    let cap = usize::try_from(MAX_DEFINITION_BYTES).unwrap_or(usize::MAX);
    Ok(match git.read_blob(git_dir, &entry.oid, cap)? {
        Some(bytes) => Written::Bytes(bytes),
        None => Written::TooLarge,
    })
}

/// Writes one entry. A definition's bytes must be written; a placeholder
/// that cannot be created (a name the file system refuses, or a second
/// name that differs only in case on a case-insensitive file system) is
/// skipped, because it only carries a notice or a problem.
fn write_entry(path: &Path, written: Written) -> Result<()> {
    let placeholder = match written {
        Written::Bytes(bytes) => {
            return new_file(path)
                .and_then(|mut file| {
                    file.write_all(&bytes)?;
                    file.sync_all()
                })
                .map_err(|error| Error::io("write", path, &error));
        }
        Written::Empty => new_file(path).is_ok(),
        Written::Directory => create_dir(path).is_ok(),
        Written::TooLarge => {
            new_file(path).is_ok_and(|file| file.set_len(MAX_DEFINITION_BYTES + 1).is_ok())
        }
        Written::NotRegular => not_regular(path).is_ok(),
    };
    // A placeholder that could not be created is skipped (see above).
    let _created = placeholder;
    Ok(())
}

/// Writes the placeholders after every definition (see the module
/// documentation); one that names an existing entry is skipped. The
/// placeholders of a `.toml` file with a valid stem (`not-regular-file`,
/// `file-too-large`) come first: they carry a conflict key and hold the
/// server (fail closed), so a keyless placeholder that collides with one on
/// a case-insensitive file system (`BIG.toml` with `big.toml`) must not
/// take its place and let a lower-priority definition win.
fn write_placeholders(mut placeholders: Vec<(PathBuf, Written)>) -> Result<()> {
    placeholders.sort_by_key(|(_, placeholder)| {
        !matches!(placeholder, Written::NotRegular | Written::TooLarge)
    });
    for (path, placeholder) in placeholders {
        write_entry(&path, placeholder)?;
    }
    Ok(())
}

/// Creates a new file with mode `0600`, never following or replacing an
/// existing entry.
fn new_file(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(crate::store::FILE_MODE);
    }
    options.open(path)
}

#[cfg(unix)]
fn not_regular(path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(NOT_REGULAR_TARGET, path)
}

#[cfg(not(unix))]
fn not_regular(_path: &Path) -> std::io::Result<()> {
    let _ = NOT_REGULAR_TARGET;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_names_are_skipped() {
        for name in [&b""[..], b".", b"..", b"a/b", b"a\0b"] {
            assert!(safe_name(name).is_none(), "{name:?}");
        }
        assert_eq!(
            safe_name(b"github.toml"),
            Some(PathBuf::from("github.toml"))
        );
    }

    #[test]
    fn stamps_must_match_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let pin = CommitId::parse(&"a".repeat(40)).unwrap();
        assert!(!stamp_matches(temp.path(), &pin));
        fs::write(temp.path().join(STAMP_FILE), format!("{pin}\n")).unwrap();
        assert!(stamp_matches(temp.path(), &pin));
        fs::write(temp.path().join(STAMP_FILE), format!("{pin}")).unwrap();
        assert!(!stamp_matches(temp.path(), &pin));
        let other = CommitId::parse(&"b".repeat(40)).unwrap();
        fs::write(temp.path().join(STAMP_FILE), format!("{other}\n")).unwrap();
        assert!(!stamp_matches(temp.path(), &pin));
    }

    #[cfg(unix)]
    #[test]
    fn placeholders_classify_like_the_scanner() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        write_entry(&dir.join("a.toml"), Written::Bytes(b"x".to_vec())).unwrap();
        write_entry(&dir.join(".hidden"), Written::Empty).unwrap();
        write_entry(&dir.join("sub"), Written::Directory).unwrap();
        write_entry(&dir.join("link.toml"), Written::NotRegular).unwrap();
        write_entry(&dir.join("big.toml"), Written::TooLarge).unwrap();
        // A second placeholder of the same name is skipped, a definition is not.
        write_entry(&dir.join(".hidden"), Written::Empty).unwrap();
        assert!(write_entry(&dir.join("a.toml"), Written::Bytes(Vec::new())).is_err());
        assert_eq!(fs::read(dir.join("a.toml")).unwrap(), b"x");
        assert!(
            fs::symlink_metadata(dir.join("link.toml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::metadata(dir.join("sub")).unwrap().is_dir());
        assert_eq!(
            fs::metadata(dir.join("big.toml")).unwrap().len(),
            MAX_DEFINITION_BYTES + 1
        );
    }

    /// On a case-insensitive file system `GitHub.toml` (an invalid stem) or
    /// `GITHUB.TOML` (not `.toml`) names the same file as `github.toml`; the
    /// same name stands in for that collision here. The definition is
    /// written first, so every placeholder kind is skipped, never an error.
    #[cfg(unix)]
    #[test]
    fn a_placeholder_that_collides_with_a_definition_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("github.toml");
        write_entry(&path, Written::Bytes(b"name = \"github\"\n".to_vec())).unwrap();
        write_placeholders(vec![
            (path.clone(), Written::Empty),
            (path.clone(), Written::Directory),
            (path.clone(), Written::NotRegular),
            (path.clone(), Written::TooLarge),
        ])
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"name = \"github\"\n");
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
    }

    /// `BIG.toml` (an invalid stem) sorts before `big.toml` in a Git tree;
    /// on a case-insensitive file system both name the same file. The
    /// keyed placeholder of the oversized `big.toml` wins, whatever the
    /// listing order, so the server stays held instead of vanishing.
    #[cfg(unix)]
    #[test]
    fn a_keyed_placeholder_wins_over_a_keyless_one() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("big.toml");
        write_placeholders(vec![
            (path.clone(), Written::Empty),
            (path.clone(), Written::Directory),
            (path.clone(), Written::TooLarge),
        ])
        .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), MAX_DEFINITION_BYTES + 1);
        let link = temp.path().join("link.toml");
        write_placeholders(vec![
            (link.clone(), Written::Directory),
            (link.clone(), Written::NotRegular),
        ])
        .unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
    }
}
