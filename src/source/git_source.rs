//! Git sources in the store: `source add-git` and the file side of
//! `source remove`.
//!
//! A Git source lives in `<store>/sources/<id>/`: `repo/` is a full clone
//! without a checkout, with `refs/d-mcp/pin` keeping the pinned commit
//! reachable, and `snapshot/` holds the materialized definitions with a
//! commit stamp (see [`super::snapshot`]).
//!
//! `add-git` clones into a temporary directory `sources/.tmp-<random>`
//! outside the store lock (the network can be slow), verifies the commit,
//! records the followed ref fully qualified (`refs/heads/<ref>` or
//! `refs/tags/<ref>`, whichever `git clone --branch` checked out), sets the
//! pin ref and materializes the snapshot. Then, under the lock, it re-checks
//! the id and the priority, sweeps stale `sources/.tmp-*` left by a killed
//! run, replaces an unregistered `sources/<id>`, renames the temporary
//! directory into place, writes `lock.toml` and finally `config.toml`. A
//! registered source therefore always has a pin and a snapshot; a failure
//! or a crash leaves at most an unregistered directory or a stale pin, which
//! the next `add-git` replaces. A dry run clones into a fresh directory
//! under `TMPDIR`, deletes it afterwards, and leaves the store
//! byte-identical.
//!
//! A temporary directory in `sources/` holds an exclusive lock on its
//! `.owner` file while its run is alive ([`Staging`]); the sweep skips such
//! a directory, so a run that is still cloning never loses its clone to a
//! concurrent `add-git` or `refresh`. The directory gets its `.tmp-` name
//! only once the lock is held. The lock is released when the process ends,
//! so a killed run's directory is swept by the next run.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::{Config, GitOrigin, LOCAL_SOURCE_ID, SourceEntry, SourceKind, SourceOrigin};
use crate::definition::is_valid_name;
use crate::error::{Error, Result};
use crate::git::{CommitId, Git, GitUrl, validate_ref_syntax, validate_url};
use crate::hashkey;
use crate::lockfile::{self, LockFile};
use crate::resolver::SourceSnapshot;
use crate::store::{Environment, StoreLock, StorePaths, StoreWarning, create_dir};

use super::{GitSpec, SourceSpec, scan, snapshot};

/// The snapshot directory of a Git source.
pub const SNAPSHOT_DIR: &str = "snapshot";
/// The clone directory of a Git source.
pub const REPO_DIR: &str = "repo";
/// The prefix of the temporary directories in `sources/`.
pub const TEMP_PREFIX: &str = ".tmp-";
/// The lock file a live run holds in its temporary directory.
const OWNER_FILE: &str = ".owner";
/// The prefix a temporary directory is created under before its `.owner`
/// lock is held; the sweep leaves it alone until it is
/// [`STALE_STAGING_AFTER`] old.
const STAGING_PREFIX: &str = ".staging-";
/// How old a `sources/.staging-*` directory must be before the sweep takes
/// it for the leftover of a run killed between creating and renaming it.
const STALE_STAGING_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// The arguments of `source add-git`.
#[derive(Debug, Clone, Default)]
pub struct AddGitOptions<'a> {
    /// The remote URL.
    pub url: &'a str,
    /// The source id; derived from the URL when `None`.
    pub id: Option<&'a str>,
    /// The branch or tag; the remote's default branch when `None`.
    pub reference: Option<&'a str>,
    /// The commit to pin; the head of the ref when `None`.
    pub commit: Option<&'a str>,
    /// The priority; one above the highest user source when `None`.
    pub priority: Option<u32>,
    /// Whether the source is trusted.
    pub trusted: bool,
    /// Whether this is a dry run.
    pub dry_run: bool,
}

/// What `add-git` did, or would do in a dry run.
#[derive(Debug, Clone)]
pub struct AddGitOutcome {
    /// The registered (or, in a dry run, the would-be) source.
    pub entry: SourceEntry,
    /// Its spec at the final location.
    pub spec: SourceSpec,
    /// The pinned commit.
    pub commit: CommitId,
    /// The scan of the new snapshot.
    pub snapshot: SourceSnapshot,
    /// Non-fatal store conditions.
    pub warnings: Vec<StoreWarning>,
}

/// The ref and the commit a clone was pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The followed ref, as `config.toml` stores it (`main`, `v1`).
    pub reference: String,
    /// The followed ref, fully qualified (`refs/heads/main`,
    /// `refs/tags/v1`), as `lock.toml` stores it.
    pub followed: String,
    /// The pinned commit.
    pub commit: CommitId,
}

/// The temporary directory a clone is made in: `sources/.tmp-<random>`
/// whose `.owner` lock is held while this value lives, or, for a dry run, a
/// fresh directory under `TMPDIR`. Dropped, it is deleted.
///
/// The directory is created as `sources/.staging-<random>`, which the sweep
/// leaves alone, and renamed to `.tmp-<random>` only once its `.owner` lock
/// is held, so a concurrent sweep never sees a `.tmp-*` directory of a live
/// run without its lock.
#[derive(Debug)]
pub struct Staging {
    path: PathBuf,
    owner: Option<fs::File>,
    /// Whether dropping the value deletes the directory; cleared by
    /// [`Staging::keep`].
    delete: bool,
}

impl Staging {
    /// Creates the directory; in `sources/` (made when missing) unless
    /// `dry_run`, in which case `dry_prefix` names it under `TMPDIR`.
    ///
    /// # Errors
    ///
    /// [`Error::StorePathInvalid`] and [`Error::Io`].
    pub fn create(paths: &StorePaths, dry_run: bool, dry_prefix: &str) -> Result<Self> {
        if dry_run {
            let dir = tempfile::Builder::new()
                .prefix(dry_prefix)
                .tempdir()
                .map_err(|error| Error::io("create directory", std::env::temp_dir(), &error))?;
            return Ok(Self {
                path: dir.keep(),
                owner: None,
                delete: true,
            });
        }
        let dir = Self::create_unlocked(paths)?;
        Self::lock_and_publish(paths, dir)
    }

    /// The first step of [`Staging::create`]: the directory, not yet
    /// locked, under a name the sweep leaves alone.
    fn create_unlocked(paths: &StorePaths) -> Result<tempfile::TempDir> {
        ensure_sources_dir(paths)?;
        tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(&paths.sources)
            .map_err(|error| Error::io("create directory", &paths.sources, &error))
    }

    /// The second step of [`Staging::create`]: takes the `.owner` lock,
    /// then renames the directory to its `.tmp-` name.
    fn lock_and_publish(paths: &StorePaths, dir: tempfile::TempDir) -> Result<Self> {
        let owner_path = dir.path().join(OWNER_FILE);
        let owner = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&owner_path)
            .map_err(|error| Error::io("create", &owner_path, &error))?;
        owner
            .try_lock()
            .map_err(|_| Error::io("lock", &owner_path, &io::Error::other("already locked")))?;
        // The lock follows the open file across the rename.
        let suffix = dir
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(STAGING_PREFIX))
            .unwrap_or_default()
            .to_owned();
        let path = paths.sources.join(format!("{TEMP_PREFIX}{suffix}"));
        if fs::symlink_metadata(&path).is_ok() {
            // Never rename over an existing entry; `dir` is deleted.
            return Err(Error::io(
                "create directory",
                &path,
                &io::Error::from(io::ErrorKind::AlreadyExists),
            ));
        }
        fs::rename(dir.path(), &path).map_err(|error| Error::io("rename", dir.path(), &error))?;
        // Renamed away: the guard has nothing left to delete.
        let _ = dir.keep();
        Ok(Self {
            path,
            owner: Some(owner),
            delete: true,
        })
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hands the directory over to be renamed into place: the `.owner`
    /// file is removed and its lock released. Call under the store lock,
    /// which keeps every sweep away from now on.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory is gone (a sweep removed it before
    /// its lock was taken); nothing has been replaced then.
    pub fn keep(mut self) -> Result<PathBuf> {
        if self.owner.is_some() {
            remove_entry(&self.path.join(OWNER_FILE))?;
        }
        if !fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(Error::io(
                "read",
                &self.path,
                &io::Error::from(io::ErrorKind::NotFound),
            ));
        }
        drop(self.owner.take());
        self.delete = false;
        Ok(self.path.clone())
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if self.delete {
            // Best effort, like `tempfile::TempDir`; a symlink inside is
            // unlinked, never followed.
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Whether a temporary directory belongs to a live run: its `.owner` lock
/// is held. The file is opened without following a symlink.
fn in_use(dir: &Path) -> bool {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    crate::store::no_follow(&mut options, true);
    match options.open(dir.join(OWNER_FILE)) {
        Ok(file) => matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)),
        Err(_) => false,
    }
}

/// Moves `staged` to `target`. An existing `target` is first renamed to
/// `<park_dir>/old` and restored when the second rename fails, so a failure
/// never loses it; the caller deletes `park_dir` (and the old copy with it)
/// afterwards. Call under the store lock.
///
/// # Errors
///
/// [`Error::Io`]; `target` is then unchanged.
pub fn swap_in(target: &Path, staged: &Path, park_dir: &Path) -> Result<()> {
    if !fs::symlink_metadata(staged).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(Error::io(
            "rename",
            staged,
            &io::Error::from(io::ErrorKind::NotFound),
        ));
    }
    let parked = park_dir.join("old");
    let had_old = fs::symlink_metadata(target).is_ok();
    if had_old {
        fs::rename(target, &parked).map_err(|error| Error::io("rename", target, &error))?;
    }
    if let Err(error) = fs::rename(staged, target) {
        if had_old {
            let _ = fs::rename(&parked, target);
        }
        return Err(Error::io("rename", target, &error));
    }
    Ok(())
}

/// Runs `source add-git`.
///
/// # Errors
///
/// The validation errors (`source-url-invalid`, `invalid-name`,
/// `source-id-reserved`, `source-ref-invalid`, `priority-invalid`), the
/// store errors (`store-not-initialized`, `source-id-taken`,
/// `priority-taken`, `store-locked`, `config-corrupt`, `lock-corrupt`), the
/// git errors and `source-commit-unreachable`.
pub fn add_git(
    paths: &StorePaths,
    env: &Environment,
    options: &AddGitOptions<'_>,
) -> Result<AddGitOutcome> {
    paths.require_initialized()?;
    let url = validate_url(options.url)?;
    let id = match options.id {
        Some(id) => id.to_owned(),
        None => url
            .default_id()
            .ok_or(Error::InvalidName { what: "source id" })?,
    };
    if id == LOCAL_SOURCE_ID {
        return Err(Error::SourceIdReserved);
    }
    if !is_valid_name(&id) {
        return Err(Error::InvalidName { what: "source id" });
    }
    if let Some(reference) = options.reference {
        validate_ref_syntax(reference)?;
    }
    let requested = options.commit.map(CommitId::from_input).transpose()?;
    if options.priority == Some(0) {
        return Err(Error::PriorityInvalid);
    }
    // Fail before any network access; everything is re-checked under the
    // lock.
    let config = paths.load_config()?;
    check_free(&config, &id, options.priority)?;
    let git = Git::locate(env)?;
    let staging = Staging::create(paths, options.dry_run, "d-mcp-add-git-")?;
    let fetched = fetch_into(
        &git,
        staging.path(),
        &url,
        options.reference,
        requested.as_ref(),
        &id,
    )?;
    let origin = GitOrigin {
        url: url.as_str().to_owned(),
        reference: fetched.reference.clone(),
        trusted: options.trusted,
    };
    let staged = scan(&git_spec(
        &id,
        0,
        &origin,
        staging.path().join(SNAPSHOT_DIR),
        Some(fetched.commit.clone()),
    ));
    let mut warnings = Vec::new();
    let priority = if options.dry_run {
        options
            .priority
            .unwrap_or_else(|| default_priority(&config))
    } else {
        let lock = StoreLock::acquire(paths)?;
        let mut config = paths.load_config()?;
        warnings.extend(hashkey::ensure_under_lock(paths, &lock)?);
        check_free(&config, &id, options.priority)?;
        let priority = options
            .priority
            .unwrap_or_else(|| default_priority(&config));
        let mut pins = lockfile::load(paths)?;
        sweep(paths, staging.path());
        let target = paths.git_source(&id);
        let staged_dir = staging.keep()?;
        let replaced = remove_entry(&target).and_then(|()| {
            fs::rename(&staged_dir, &target).map_err(|error| Error::io("rename", &target, &error))
        });
        if let Err(error) = replaced {
            let _ = fs::remove_dir_all(&staged_dir);
            return Err(error);
        }
        pins.set_pin(&id, fetched.commit.clone(), fetched.followed.clone());
        let entry = SourceEntry {
            id: id.clone(),
            priority,
            origin: SourceOrigin::Git(origin.clone()),
        };
        config.sources.push(entry);
        let written = lockfile::save(paths, &lock, &pins)
            .and_then(|warning| Ok((warning, paths.save_config(&lock, &config)?)));
        match written {
            Ok((first, second)) => warnings.extend(first.into_iter().chain(second)),
            Err(error) => {
                // Not registered: the directory is an unregistered leftover
                // (and the pin, if written, a stale one); remove it now.
                let _ = fs::remove_dir_all(&target);
                return Err(error);
            }
        }
        priority
    };
    let entry = SourceEntry {
        id: id.clone(),
        priority,
        origin: SourceOrigin::Git(origin.clone()),
    };
    let spec = git_spec(
        &id,
        priority,
        &origin,
        paths.git_source(&id).join(SNAPSHOT_DIR),
        Some(fetched.commit.clone()),
    );
    Ok(AddGitOutcome {
        entry,
        spec,
        commit: fetched.commit,
        snapshot: staged,
        warnings,
    })
}

/// The spec of a Git source scanned at `root`.
pub(super) fn git_spec(
    id: &str,
    priority: u32,
    origin: &GitOrigin,
    root: std::path::PathBuf,
    pin: Option<CommitId>,
) -> SourceSpec {
    SourceSpec {
        id: id.to_owned(),
        kind: SourceKind::Git,
        root,
        priority,
        builtin: false,
        git: Some(GitSpec {
            url: origin.url.clone(),
            reference: origin.reference.clone(),
            trusted: origin.trusted,
            pin,
        }),
    }
}

/// Fails when `id` is registered or `priority` is used.
fn check_free(config: &Config, id: &str, priority: Option<u32>) -> Result<()> {
    if config.source(id).is_some() {
        return Err(Error::SourceIdTaken { id: id.to_owned() });
    }
    if let Some(priority) = priority
        && let Some(other) = config.sources.iter().find(|s| s.priority == priority)
    {
        return Err(Error::PriorityTaken {
            priority,
            id: other.id.clone(),
        });
    }
    Ok(())
}

/// One above the highest user-source priority, at least 1.
#[must_use]
pub fn default_priority(config: &Config) -> u32 {
    config
        .sources
        .iter()
        .map(|source| source.priority)
        .max()
        .map_or(1, |highest| highest.saturating_add(1))
}

/// Clones `url` into `<staging>/repo`, resolves the ref and the commit,
/// sets the pin ref and materializes `<staging>/snapshot`.
///
/// # Errors
///
/// `source-ref-invalid`, `source-commit-unreachable`, the git errors and
/// [`Error::Io`].
pub fn fetch_into(
    git: &Git,
    staging: &Path,
    url: &GitUrl,
    reference: Option<&str>,
    requested: Option<&CommitId>,
    id: &str,
) -> Result<Fetched> {
    if let Some(reference) = reference {
        git.check_ref_format(staging, reference)?;
    }
    let repo = staging.join(REPO_DIR);
    git.clone_repo(staging, url, reference, &repo)?;
    let git_dir = repo.join(".git");
    // `clone --branch` prefers a branch over a tag of the same name; the
    // followed ref is recorded as what it checked out.
    let branch = git.head_branch(&git_dir)?;
    let (reference, followed) = match reference {
        Some(reference) if branch.as_deref() == Some(reference) => {
            (reference.to_owned(), format!("refs/heads/{reference}"))
        }
        Some(reference) => (reference.to_owned(), format!("refs/tags/{reference}")),
        None => {
            let branch = branch.ok_or(Error::GitRefNotFound { operation: "clone" })?;
            let followed = format!("refs/heads/{branch}");
            (branch, followed)
        }
    };
    let head = git
        .rev_parse_commit(&git_dir, "HEAD")?
        .ok_or(Error::GitRefNotFound { operation: "clone" })?;
    let commit = match requested {
        None => head,
        Some(requested) => {
            let found = git.rev_parse_commit(&git_dir, requested.as_str())?;
            let reachable =
                found.as_ref() == Some(requested) && git.is_ancestor(&git_dir, requested, &head)?;
            if !reachable {
                return Err(Error::SourceCommitUnreachable {
                    id: id.to_owned(),
                    reference,
                    commit: requested.to_string(),
                });
            }
            requested.clone()
        }
    };
    git.update_pin(&git_dir, &commit)?;
    snapshot::materialize(git, &git_dir, &commit, &staging.join(SNAPSHOT_DIR))?;
    Ok(Fetched {
        reference,
        followed,
        commit,
    })
}

/// Creates `sources/` (mode `0700`) when it is missing. Refuses a
/// `sources` entry that is not a directory, a symlink included, so nothing
/// is ever written outside the store.
///
/// # Errors
///
/// [`Error::StorePathInvalid`] and [`Error::Io`].
pub fn ensure_sources_dir(paths: &StorePaths) -> Result<()> {
    match fs::symlink_metadata(&paths.sources) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Error::StorePathInvalid {
            path: paths.sources.clone(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_dir(&paths.sources).or_else(|error| {
                // Another process may have created it meanwhile.
                if fs::symlink_metadata(&paths.sources).is_ok_and(|m| m.is_dir()) {
                    Ok(())
                } else {
                    Err(error)
                }
            })
        }
        Err(error) => Err(Error::io("read", &paths.sources, &error)),
    }
}

/// Removes the stale temporary directories `sources/.tmp-*` of killed runs,
/// except `keep` and every directory whose `.owner` lock a live run holds,
/// and the `sources/.staging-*` directories older than an hour (a live run
/// renames its own at once). Call under
/// the store lock. Best effort.
pub fn sweep(paths: &StorePaths, keep: &Path) {
    let Ok(entries) = fs::read_dir(&paths.sources) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_str().unwrap_or_default();
        let directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let stale = if name.starts_with(TEMP_PREFIX) {
            path != keep && !(directory && in_use(&path))
        } else if name.starts_with(STAGING_PREFIX) {
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= STALE_STAGING_AFTER)
        } else {
            false
        };
        if stale {
            let _ = remove_entry(&path);
        }
    }
}

/// Removes a file-system entry: a directory recursively, anything else
/// (a symlink included, which is never followed) by unlinking it. A missing
/// entry is fine.
///
/// # Errors
///
/// [`Error::Io`].
pub fn remove_entry(path: &Path) -> Result<()> {
    let result = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    };
    result.map_err(|error| Error::io("remove", path, &error))
}

/// Deletes the pin and the directory of a Git source that has just left
/// `config.toml`. Call under the store lock with the `lock.toml` loaded
/// before `config.toml` was written, so a corrupt `lock.toml` fails the
/// command before anything changes.
///
/// # Errors
///
/// [`Error::Io`]; the source is already unregistered, and a leftover is
/// replaced by the next `add-git` of the id.
pub fn forget(
    paths: &StorePaths,
    lock: &StoreLock,
    id: &str,
    mut pins: LockFile,
) -> Result<Vec<StoreWarning>> {
    let mut warnings = Vec::new();
    if pins.remove_pin(id).is_some() {
        warnings.extend(lockfile::save(paths, lock, &pins)?);
    }
    remove_entry(&paths.git_source(id))?;
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(id: &str, priority: u32) -> SourceEntry {
        SourceEntry {
            id: id.to_owned(),
            priority,
            origin: SourceOrigin::Directory {
                path: format!("/srv/{id}").into(),
            },
        }
    }

    #[test]
    fn default_priority_is_above_every_user_source() {
        let mut config = Config::default();
        assert_eq!(default_priority(&config), 1);
        config.sources = vec![directory("a", 3), directory("b", 10)];
        assert_eq!(default_priority(&config), 11);
        assert!(check_free(&config, "c", Some(4)).is_ok());
        assert!(matches!(
            check_free(&config, "a", None),
            Err(Error::SourceIdTaken { .. })
        ));
        assert!(matches!(
            check_free(&config, "c", Some(10)),
            Err(Error::PriorityTaken { priority: 10, .. })
        ));
    }

    /// A run that is still cloning holds its `.owner` lock: a concurrent
    /// sweep leaves its directory alone, and sweeps it once the run is
    /// gone.
    #[test]
    fn the_sweep_skips_the_directory_of_a_live_run() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        let live = Staging::create(&paths, false, "unused-").unwrap();
        fs::create_dir(live.path().join(REPO_DIR)).unwrap();
        let mine = paths.sources.join(".tmp-mine");
        sweep(&paths, &mine);
        assert!(live.path().join(REPO_DIR).is_dir());
        // A killed run's directory: its `.owner` exists, the lock is gone.
        let killed = paths.sources.join(".tmp-killed");
        fs::create_dir(&killed).unwrap();
        fs::write(killed.join(OWNER_FILE), "").unwrap();
        // A `.owner` symlink is never followed (and never counts as held).
        let linked = paths.sources.join(".tmp-linked");
        fs::create_dir(&linked).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(live.path().join(OWNER_FILE), linked.join(OWNER_FILE)).unwrap();
        sweep(&paths, &mine);
        assert!(!killed.exists());
        assert!(!linked.exists());
        assert!(live.path().is_dir());
        let path = live.path().to_path_buf();
        drop(live);
        assert!(!path.exists(), "a dropped staging directory is deleted");
        let released = Staging::create(&paths, false, "unused-").unwrap();
        let kept = released.keep().unwrap();
        assert!(!kept.join(OWNER_FILE).exists());
        sweep(&paths, &mine);
        assert!(!kept.exists(), "a kept directory left in sources/ is swept");
    }

    /// A sweep between creating a staging directory and taking its lock
    /// leaves it alone: it is not a `.tmp-*` directory yet. A leftover of a
    /// run killed in between is swept once it is old.
    #[test]
    fn a_sweep_before_the_lock_is_taken_keeps_the_directory() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        let unlocked = Staging::create_unlocked(&paths).unwrap();
        sweep(&paths, &paths.sources.join(".tmp-mine"));
        assert!(unlocked.path().is_dir());
        let staging = Staging::lock_and_publish(&paths, unlocked).unwrap();
        let name = staging.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(TEMP_PREFIX), "{name}");
        assert!(in_use(staging.path()));
        let left: Vec<_> = fs::read_dir(&paths.sources)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
        // A killed run's leftover: kept while young, swept once stale.
        let leftover = paths.sources.join(format!("{STAGING_PREFIX}killed"));
        fs::create_dir(&leftover).unwrap();
        sweep(&paths, staging.path());
        assert!(leftover.is_dir());
        let old = std::time::SystemTime::now() - STALE_STAGING_AFTER;
        fs::File::open(&leftover)
            .unwrap()
            .set_modified(old)
            .unwrap();
        sweep(&paths, staging.path());
        assert!(!leftover.exists());
        assert!(staging.path().is_dir());
    }

    /// A staging directory that a sweep removed before its lock was taken
    /// is refused, and a swap with a missing staged directory leaves the
    /// target as it was.
    #[test]
    fn a_vanished_staging_directory_never_replaces_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        let staging = Staging::create(&paths, false, "unused-").unwrap();
        fs::remove_dir_all(staging.path()).unwrap();
        assert!(matches!(staging.keep(), Err(Error::Io { .. })));

        let target = paths.sources.join("personal");
        fs::create_dir_all(target.join("snapshot")).unwrap();
        fs::write(target.join("snapshot/commit"), "old\n").unwrap();
        let park = tempfile::tempdir_in(&paths.sources).unwrap();
        let missing = paths.sources.join(".tmp-gone");
        assert!(matches!(
            swap_in(&target, &missing, park.path()),
            Err(Error::Io { .. })
        ));
        assert_eq!(
            fs::read_to_string(target.join("snapshot/commit")).unwrap(),
            "old\n"
        );
        // The target cannot be parked: nothing moves.
        let staged = paths.sources.join(".tmp-new");
        fs::create_dir_all(staged.join("snapshot")).unwrap();
        let bad_park = paths.sources.join("no-such-park");
        assert!(swap_in(&target, &staged, &bad_park).is_err());
        assert_eq!(
            fs::read_to_string(target.join("snapshot/commit")).unwrap(),
            "old\n"
        );
        // The second rename fails (the staged directory lay inside the
        // parked target): the target is restored.
        let inner = target.join("inner");
        fs::create_dir(&inner).unwrap();
        assert!(swap_in(&target, &inner, park.path()).is_err());
        assert_eq!(
            fs::read_to_string(target.join("snapshot/commit")).unwrap(),
            "old\n"
        );
        assert!(!park.path().join("old").exists());
        fs::remove_dir(&inner).unwrap();
        swap_in(&target, &staged, park.path()).unwrap();
        assert!(target.join("snapshot").is_dir());
        assert!(!target.join("snapshot/commit").exists());
        assert!(park.path().join("old/snapshot/commit").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sources_dir_sweep_and_removal() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        ensure_sources_dir(&paths).unwrap();
        ensure_sources_dir(&paths).unwrap();
        let keep = paths.sources.join(".tmp-mine");
        fs::create_dir_all(keep.join("repo")).unwrap();
        fs::create_dir_all(paths.sources.join(".tmp-stale/repo")).unwrap();
        fs::write(paths.sources.join(".tmp-file"), "x").unwrap();
        fs::create_dir(paths.sources.join("personal")).unwrap();
        sweep(&paths, &keep);
        let mut left: Vec<_> = fs::read_dir(&paths.sources)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, [".tmp-mine", "personal"]);
        // A symlink is unlinked, never followed.
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "x").unwrap();
        let link = paths.sources.join("linked");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        remove_entry(&link).unwrap();
        assert!(outside.join("keep.txt").exists());
        remove_entry(&link).unwrap();
        remove_entry(&paths.sources.join("personal")).unwrap();
        // A `sources` symlink is refused.
        fs::remove_dir_all(&paths.sources).unwrap();
        std::os::unix::fs::symlink(&outside, &paths.sources).unwrap();
        assert!(matches!(
            ensure_sources_dir(&paths),
            Err(Error::StorePathInvalid { .. })
        ));
    }
}
