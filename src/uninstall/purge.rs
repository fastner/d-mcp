//! `uninstall --purge`: deletes the known store entries inside the resolved
//! store root, `.lock` last, and then the root when it is empty.
//!
//! Only names in [`KNOWN_ENTRIES`] are touched. An entry that is a symlink
//! is unlinked, never followed, and [`fs::remove_dir_all`] does not follow
//! symlinks inside `local/` either, so nothing outside the store (such as
//! a registered source) is deleted. A symlinked store root is purged only
//! with `--follow-store-link`: then the known entries inside its target are
//! deleted, and the link and the target directory are left.

use std::fs;
use std::path::{Path, PathBuf};

use super::BlockReason;
use crate::config::Harness;
use crate::error::{Error, Result};
use crate::store::{KNOWN_ENTRIES, StorePaths};

/// The purge of a store, planned or done.
#[derive(Debug, Clone)]
pub struct PurgePlan {
    /// Whether `--purge` was given.
    pub requested: bool,
    /// Whether `--follow-store-link` was given.
    pub follow_store_link: bool,
    /// Whether the store root is a symlink.
    pub root_is_link: bool,
    /// The directory the entries are deleted from: the store root, or the
    /// target of a symlinked root.
    pub target: PathBuf,
    /// The existing known entries, in deletion order.
    pub entries: Vec<PathBuf>,
    /// What blocks the purge, with the harness it is about.
    pub blocked: Vec<(BlockReason, Option<Harness>)>,
    /// Whether the entries were deleted.
    pub deleted: bool,
}

impl PurgePlan {
    /// Plans the purge of the store at `paths`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when a symlinked store root cannot be resolved.
    pub fn new(paths: &StorePaths, requested: bool, follow_store_link: bool) -> Result<Self> {
        let root_is_link = fs::symlink_metadata(&paths.root)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        let target = if root_is_link {
            fs::canonicalize(&paths.root)
                .map_err(|error| Error::io("resolve", &paths.root, &error))?
        } else {
            paths.root.clone()
        };
        // `.lock` goes last, while the lock is still held.
        let entries = KNOWN_ENTRIES
            .iter()
            .copied()
            .filter(|name| *name != ".lock")
            .chain([".lock"])
            .map(|name| target.join(name))
            .filter(|path| fs::symlink_metadata(path).is_ok())
            .collect();
        let mut plan = Self {
            requested,
            follow_store_link,
            root_is_link,
            target,
            entries,
            blocked: Vec::new(),
            deleted: false,
        };
        if requested && root_is_link && !follow_store_link {
            plan.block(BlockReason::StoreRootSymlink, None);
        }
        Ok(plan)
    }

    /// Adds a blocking condition, once.
    pub fn block(&mut self, reason: BlockReason, harness: Option<Harness>) {
        if !self.blocked.contains(&(reason, harness)) {
            self.blocked.push((reason, harness));
        }
    }

    /// The explicit warning for a symlinked store root, when `--purge` was
    /// given.
    #[must_use]
    pub fn symlink_warning(&self, preview: bool) -> Option<String> {
        if !(self.requested && self.root_is_link) {
            return None;
        }
        let target = self.target.display();
        Some(if !self.follow_store_link {
            format!(
                "the store root is a symlink to {target}; --purge deletes the store files inside it only with --follow-store-link"
            )
        } else if preview {
            format!(
                "the store root is a symlink: --purge would delete the store files inside its target {target}"
            )
        } else {
            format!(
                "the store root is a symlink: --purge deletes the store files inside its target {target}"
            )
        })
    }

    /// Deletes the entries, then the store root when it is empty and not a
    /// symlink.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for the first entry that cannot be deleted; the
    /// entries before it are gone.
    pub fn run(&mut self) -> Result<()> {
        for entry in &self.entries {
            remove(entry)?;
        }
        self.deleted = true;
        if !self.root_is_link {
            // A store with foreign files (or a temporary file of an
            // interrupted write) keeps its directory.
            let _ = fs::remove_dir(&self.target);
        }
        Ok(())
    }
}

fn remove(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| Error::io("remove", path, &error))?;
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io("remove", path, &error)),
    }
}
