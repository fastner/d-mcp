//! The store in `~/.d-mcp`: path resolution, layout, permissions, the lock,
//! atomic writes and schema checks.
//!
//! This module owns every write d-mcp makes and the advisory lock. Mutating
//! commands take the lock first and read `config.toml` only afterwards, which
//! avoids TOCTOU races. Read-only commands and dry runs take no lock and write
//! nothing.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tempfile::NamedTempFile;

use crate::config::{Config, ConfigError};
use crate::error::{Error, Result};

/// Directory mode of every store directory.
pub const DIR_MODE: u32 = 0o700;
/// File mode of every store file.
pub const FILE_MODE: u32 = 0o600;

/// Retry delays of [`StoreLock::acquire`]: about 1.6 seconds in total.
pub const LOCK_RETRY_DELAYS: [Duration; 7] = [
    Duration::ZERO,
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

/// Store entries that may exist in a (possibly partial) store directory.
/// `state.toml`, `lock.toml` and `approvals.toml` are written by later work
/// packages.
const KNOWN_ENTRIES: [&str; 6] = [
    "config.toml",
    "local",
    ".lock",
    "state.toml",
    "lock.toml",
    "approvals.toml",
];

/// A non-fatal store condition, reported as a `warning:` line on stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreWarning {
    /// The rename of an atomic write succeeded, but the fsync of the parent
    /// directory failed. The new content is in place; only its durability
    /// across a crash is not guaranteed.
    ParentSyncFailed {
        /// The written file.
        path: PathBuf,
        /// The I/O error kind (the OS message is not kept).
        kind: io::ErrorKind,
    },
    /// An existing store root is accessible by group or others. d-mcp
    /// creates it with mode `0700` but never changes an existing one.
    LoosePermissions {
        /// The store root.
        path: PathBuf,
        /// The permission bits.
        mode: u32,
    },
}

impl std::fmt::Display for StoreWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreWarning::ParentSyncFailed { path, kind } => write!(
                f,
                "wrote {} but could not sync its directory: {}; the change may not survive a crash",
                path.display(),
                io::Error::from(*kind)
            ),
            StoreWarning::LoosePermissions { path, mode } => write!(
                f,
                "the store directory {} has mode {mode:04o}, which lets group or others in; \
                 d-mcp expects 0700 and leaves it unchanged",
                path.display()
            ),
        }
    }
}

/// The process environment d-mcp reads, injectable for tests.
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// `HOME`.
    pub home: Option<OsString>,
    /// `D_MCP_STORE`.
    pub store: Option<OsString>,
    /// The current directory.
    pub cwd: Option<PathBuf>,
}

impl Environment {
    /// Reads the environment of the current process. `HOME` is read
    /// directly: `std::env::home_dir` would fall back to `getpwuid`.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME"),
            store: std::env::var_os("D_MCP_STORE"),
            cwd: std::env::current_dir().ok(),
        }
    }

    fn home(&self) -> Result<PathBuf> {
        match &self.home {
            Some(home) if !home.is_empty() => Ok(PathBuf::from(home)),
            _ => Err(Error::HomeUnset),
        }
    }

    /// Expands a leading `~` and resolves a relative path against the current
    /// directory. The result is absolute and lexically normalized, but not
    /// canonicalized: the path may not exist yet.
    ///
    /// # Errors
    ///
    /// [`Error::HomeUnset`] when `~` needs an unset `HOME`, and
    /// [`Error::CurrentDirUnavailable`] for a relative path without a current
    /// directory.
    pub fn absolute(&self, path: &Path) -> Result<PathBuf> {
        let expanded = match path.strip_prefix("~") {
            Ok(rest) => self.home()?.join(rest),
            Err(_) => path.to_path_buf(),
        };
        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            self.cwd
                .as_ref()
                .ok_or(Error::CurrentDirUnavailable)?
                .join(expanded)
        };
        Ok(normalize(&absolute))
    }

    /// Resolves the store root: `--store`, then `D_MCP_STORE` (an empty
    /// value counts as unset), then `$HOME/.d-mcp`.
    ///
    /// # Errors
    ///
    /// See [`Environment::absolute`]; [`Error::HomeUnset`] when the default
    /// is needed and `HOME` is unset or empty.
    pub fn store_root(&self, flag: Option<&Path>) -> Result<PathBuf> {
        if let Some(flag) = flag {
            return self.absolute(flag);
        }
        if let Some(store) = self.store.as_ref().filter(|store| !store.is_empty()) {
            return self.absolute(Path::new(store));
        }
        Ok(self.home()?.join(".d-mcp"))
    }
}

/// Removes `.` and resolves `..` lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// The paths of a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    /// The store root.
    pub root: PathBuf,
    /// `config.toml`.
    pub config: PathBuf,
    /// `local/`, the root of the built-in local source.
    pub local_root: PathBuf,
    /// `local/servers/`.
    pub local_servers: PathBuf,
    /// `.lock`, the advisory lock file.
    pub lock: PathBuf,
}

impl StorePaths {
    /// Derives the store paths from its root.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            config: root.join("config.toml"),
            local_root: root.join("local"),
            local_servers: root.join("local").join("servers"),
            lock: root.join(".lock"),
            root,
        }
    }

    /// Whether `config.toml` exists (as any file-system entry).
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        fs::symlink_metadata(&self.config).is_ok()
    }

    /// Fails with [`Error::StoreNotInitialized`] unless `config.toml` exists.
    ///
    /// # Errors
    ///
    /// See above.
    pub fn require_initialized(&self) -> Result<()> {
        if self.is_initialized() {
            Ok(())
        } else {
            Err(Error::StoreNotInitialized {
                path: self.root.clone(),
            })
        }
    }

    /// Reads and validates `config.toml`.
    ///
    /// # Errors
    ///
    /// [`Error::StoreNotInitialized`] when it is missing,
    /// [`Error::ConfigCorrupt`] or [`Error::SchemaUnsupported`] when it is
    /// invalid, and [`Error::Io`] when it cannot be read.
    pub fn load_config(&self) -> Result<Config> {
        let bytes = match fs::read(&self.config) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(Error::StoreNotInitialized {
                    path: self.root.clone(),
                });
            }
            Err(error) => return Err(Error::io("read", &self.config, &error)),
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Err(Error::ConfigCorrupt {
                path: self.config.clone(),
                line: 1,
                column: 1,
                field: String::new(),
                rule: crate::rule::Rule::NotUtf8,
            });
        };
        Config::parse(&text).map_err(|error| match error {
            ConfigError::Corrupt {
                rule,
                field,
                line,
                column,
            } => Error::ConfigCorrupt {
                path: self.config.clone(),
                line,
                column,
                field,
                rule,
            },
            ConfigError::UnsupportedSchema { found } => Error::SchemaUnsupported {
                path: self.config.clone(),
                found,
            },
        })
    }

    /// Writes `config.toml` atomically. Call only while holding the lock.
    /// Returns a warning when only the final directory fsync failed.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the write fails; the old file then stays intact.
    pub fn save_config(&self, _lock: &StoreLock, config: &Config) -> Result<Option<StoreWarning>> {
        write_atomic(&self.config, config.render().as_bytes())
    }
}

/// The held advisory store lock. Dropping it releases the lock.
#[derive(Debug)]
pub struct StoreLock {
    _file: File,
}

impl StoreLock {
    /// Takes the lock with the default retry delays.
    ///
    /// # Errors
    ///
    /// [`Error::StoreLocked`] when another process holds it after all
    /// retries, and [`Error::Io`] when the lock file cannot be opened.
    pub fn acquire(paths: &StorePaths) -> Result<Self> {
        Self::acquire_with_delays(paths, &LOCK_RETRY_DELAYS)
    }

    /// Takes the lock, sleeping for each delay before the next attempt.
    ///
    /// # Errors
    ///
    /// See [`StoreLock::acquire`].
    pub fn acquire_with_delays(paths: &StorePaths, delays: &[Duration]) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        set_file_mode(&mut options);
        // A `.lock` symlink is refused rather than followed out of the store.
        no_follow(&mut options, false);
        let file = options
            .open(&paths.lock)
            .map_err(|error| Error::io("open", &paths.lock, &error))?;
        for delay in delays {
            if !delay.is_zero() {
                thread::sleep(*delay);
            }
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) => {}
                Err(fs::TryLockError::Error(error)) => {
                    return Err(Error::io("lock", &paths.lock, &error));
                }
            }
        }
        Err(Error::StoreLocked {
            path: paths.lock.clone(),
        })
    }
}

#[cfg(unix)]
fn set_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(FILE_MODE);
}

#[cfg(not(unix))]
fn set_file_mode(_options: &mut OpenOptions) {}

/// `O_NOFOLLOW` and `O_NONBLOCK` for the targets d-mcp knows the values of.
///
/// The values are written out because `libc` is not a dependency. On other
/// targets both are 0, and callers rely on their handle re-checks alone.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "loongarch64",
        target_arch = "s390x"
    )
))]
const OPEN_FLAGS: (i32, i32) = (0o400_000, 0o4_000);
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    )
))]
const OPEN_FLAGS: (i32, i32) = (0o100_000, 0o4_000);
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
const OPEN_FLAGS: (i32, i32) = (0x0100, 0x0004);
#[cfg(all(
    unix,
    not(any(
        all(
            target_os = "linux",
            any(
                target_arch = "x86_64",
                target_arch = "x86",
                target_arch = "riscv64",
                target_arch = "loongarch64",
                target_arch = "s390x",
                target_arch = "aarch64",
                target_arch = "arm",
                target_arch = "powerpc",
                target_arch = "powerpc64"
            )
        ),
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))
))]
const OPEN_FLAGS: (i32, i32) = (0, 0);

/// Makes an open fail on a symlink in the last path component
/// (`O_NOFOLLOW`) and, with `nonblocking`, return at once on a FIFO instead
/// of waiting for a writer (`O_NONBLOCK`). Regular files ignore
/// `O_NONBLOCK`. A no-op where the flag values are unknown.
#[cfg(unix)]
pub(crate) fn no_follow(options: &mut OpenOptions, nonblocking: bool) {
    use std::os::unix::fs::OpenOptionsExt;
    let (nofollow, nonblock) = OPEN_FLAGS;
    options.custom_flags(if nonblocking {
        nofollow | nonblock
    } else {
        nofollow
    });
}

/// See the Unix variant; there is nothing to set elsewhere.
#[cfg(not(unix))]
pub(crate) fn no_follow(_options: &mut OpenOptions, _nonblocking: bool) {}

/// Creates one directory with mode `0700`.
fn create_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(DIR_MODE);
    }
    builder
        .create(path)
        .map_err(|error| Error::io("create directory", path, &error))
}

/// Writes a file atomically: temp file in the same directory (mode `0600`),
/// fsync, `persist` over the target, then fsync of the parent directory.
///
/// Once the rename has succeeded the new content is in place, so a failed
/// parent-directory fsync is returned as a [`StoreWarning::ParentSyncFailed`]
/// instead of an error.
///
/// # Errors
///
/// [`Error::Io`] when any step up to and including the rename fails; the old
/// file then stays intact.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<Option<StoreWarning>> {
    write_atomic_with(path, bytes, |_| Ok(()))
}

/// [`write_atomic`] with a test seam that runs between the temp write and the
/// persist. A failing seam aborts the write and removes the temp file.
///
/// # Errors
///
/// See [`write_atomic`].
pub fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    before_persist: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<Option<StoreWarning>> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let io_error = |error: io::Error| Error::io("write", path, &error);
    let mut temp = NamedTempFile::new_in(parent).map_err(io_error)?;
    temp.write_all(bytes).map_err(io_error)?;
    temp.flush().map_err(io_error)?;
    temp.as_file().sync_all().map_err(io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(FILE_MODE))
            .map_err(io_error)?;
    }
    before_persist(temp.path()).map_err(io_error)?;
    temp.persist(path).map_err(|error| io_error(error.error))?;
    Ok(File::open(parent)
        .and_then(|directory| directory.sync_all())
        .err()
        .map(|error| StoreWarning::ParentSyncFailed {
            path: path.to_path_buf(),
            kind: error.kind(),
        }))
}

/// What `init` did, or would do in a dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InitReport {
    /// The store root.
    pub store: PathBuf,
    /// Absolute paths created (or, in a dry run, that would be created), in
    /// creation order.
    pub created: Vec<PathBuf>,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Non-fatal conditions to report as warnings.
    #[serde(skip)]
    pub warnings: Vec<StoreWarning>,
}

/// Initializes or completes a store. Idempotent.
///
/// A store root that is a symlink to a directory is followed, as by every
/// other command, so a dotfile-managed `~/.d-mcp` works. A dangling symlink
/// is a [`Error::StorePathInvalid`]. An existing root that is accessible by
/// group or others yields a [`StoreWarning::LoosePermissions`]; its mode is
/// never changed.
///
/// # Errors
///
/// [`Error::StorePathInvalid`] when the root is not a directory,
/// [`Error::ForeignDirectory`] for a non-empty directory that is not a store,
/// the [`StorePaths::load_config`] errors for an existing invalid
/// `config.toml` (which is never rewritten), [`Error::StoreLocked`], and
/// [`Error::Io`].
pub fn init(paths: &StorePaths, dry_run: bool) -> Result<InitReport> {
    let mut created = Vec::new();
    let mut warnings = Vec::new();
    // `fs::metadata` follows a symlinked root, like every other command.
    let root_exists = match fs::metadata(&paths.root) {
        Ok(metadata) if metadata.is_dir() => {
            if let Some(warning) = loose_permissions(&paths.root, &metadata) {
                warnings.push(warning);
            }
            true
        }
        Ok(_) => {
            return Err(Error::StorePathInvalid {
                path: paths.root.clone(),
            });
        }
        // A dangling symlink: the root exists as an entry but leads nowhere.
        Err(_) if fs::symlink_metadata(&paths.root).is_ok() => {
            return Err(Error::StorePathInvalid {
                path: paths.root.clone(),
            });
        }
        Err(_) => false,
    };
    if root_exists {
        check_store_directory(paths)?;
    }

    if dry_run {
        // The same order as a real run: root, `.lock`, `config.toml`,
        // `local`, `local/servers`.
        if !root_exists {
            created.push(paths.root.clone());
        }
        for path in [
            &paths.lock,
            &paths.config,
            &paths.local_root,
            &paths.local_servers,
        ] {
            if fs::symlink_metadata(path).is_err() {
                created.push(path.clone());
            }
        }
        return Ok(InitReport {
            store: paths.root.clone(),
            created,
            dry_run,
            warnings,
        });
    }

    if !root_exists {
        create_dir_all(&paths.root, &mut created)?;
    }
    let lock_existed = fs::symlink_metadata(&paths.lock).is_ok();
    let lock = StoreLock::acquire(paths)?;
    if !lock_existed {
        created.push(paths.lock.clone());
    }
    // Re-check under the lock: another process may have initialized the store.
    if paths.is_initialized() {
        paths.load_config()?;
    } else {
        warnings.extend(paths.save_config(&lock, &Config::default())?);
        created.push(paths.config.clone());
    }
    for directory in [&paths.local_root, &paths.local_servers] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(Error::StorePathInvalid {
                    path: directory.clone(),
                });
            }
            Err(_) => {
                create_dir(directory)?;
                created.push(directory.clone());
            }
        }
    }
    drop(lock);
    Ok(InitReport {
        store: paths.root.clone(),
        created,
        dry_run,
        warnings,
    })
}

/// A warning when a directory is accessible by group or others.
#[cfg(unix)]
fn loose_permissions(path: &Path, metadata: &fs::Metadata) -> Option<StoreWarning> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o7777;
    (mode & 0o077 != 0).then(|| StoreWarning::LoosePermissions {
        path: path.to_path_buf(),
        mode,
    })
}

#[cfg(not(unix))]
fn loose_permissions(_path: &Path, _metadata: &fs::Metadata) -> Option<StoreWarning> {
    None
}

/// Creates a directory and its missing parents, each with mode `0700`.
fn create_dir_all(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(candidate) = current {
        if fs::symlink_metadata(candidate).is_ok() {
            break;
        }
        missing.push(candidate.to_path_buf());
        current = candidate.parent();
    }
    for directory in missing.into_iter().rev() {
        create_dir(&directory)?;
        if directory == path {
            created.push(directory);
        }
    }
    Ok(())
}

/// Accepts an existing directory as a (possibly partial) store: it has a
/// `config.toml` that loads, or it holds only known store entries.
fn check_store_directory(paths: &StorePaths) -> Result<()> {
    if paths.is_initialized() {
        paths.load_config()?;
        return Ok(());
    }
    let entries =
        fs::read_dir(&paths.root).map_err(|error| Error::io("read", &paths.root, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| Error::io("read", &paths.root, &error))?;
        let name = entry.file_name();
        if !KNOWN_ENTRIES.iter().any(|known| name == *known) {
            return Err(Error::ForeignDirectory {
                path: paths.root.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: Option<&str>, store: Option<&str>) -> Environment {
        Environment {
            home: home.map(OsString::from),
            store: store.map(OsString::from),
            cwd: Some(PathBuf::from("/work")),
        }
    }

    #[test]
    fn store_root_precedence() {
        let all = env(Some("/home/u"), Some("/env/store"));
        assert_eq!(
            all.store_root(Some(Path::new("/flag"))).unwrap(),
            PathBuf::from("/flag")
        );
        assert_eq!(all.store_root(None).unwrap(), PathBuf::from("/env/store"));
        let empty_store = env(Some("/home/u"), Some(""));
        assert_eq!(
            empty_store.store_root(None).unwrap(),
            PathBuf::from("/home/u/.d-mcp")
        );
        assert!(matches!(
            env(None, None).store_root(None),
            Err(Error::HomeUnset)
        ));
        assert!(matches!(
            env(Some(""), Some("")).store_root(None),
            Err(Error::HomeUnset)
        ));
        // An explicit store does not need HOME.
        assert_eq!(
            env(None, Some("/env/store")).store_root(None).unwrap(),
            PathBuf::from("/env/store")
        );
    }

    #[test]
    fn tilde_and_relative_paths() {
        let e = env(Some("/home/u"), Some("~/stores/d"));
        assert_eq!(
            e.store_root(None).unwrap(),
            PathBuf::from("/home/u/stores/d")
        );
        assert_eq!(
            e.store_root(Some(Path::new("~"))).unwrap(),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            e.store_root(Some(Path::new("./a/../b"))).unwrap(),
            PathBuf::from("/work/b")
        );
        assert!(matches!(
            env(None, None).absolute(Path::new("~/x")),
            Err(Error::HomeUnset)
        ));
        let no_cwd = Environment {
            cwd: None,
            ..env(None, None)
        };
        assert!(matches!(
            no_cwd.absolute(Path::new("rel")),
            Err(Error::CurrentDirUnavailable)
        ));
        // `~user` is not expanded.
        assert_eq!(
            e.absolute(Path::new("~other/x")).unwrap(),
            PathBuf::from("/work/~other/x")
        );
    }

    #[test]
    fn init_creates_layout_with_modes_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().join("a").join("store"));
        let report = init(&paths, false).unwrap();
        assert_eq!(
            report.created,
            [
                paths.root.clone(),
                paths.lock.clone(),
                paths.config.clone(),
                paths.local_root.clone(),
                paths.local_servers.clone()
            ]
        );
        assert_eq!(
            fs::read_to_string(&paths.config).unwrap(),
            "schema_version = 1\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&paths.root), DIR_MODE);
            assert_eq!(mode(paths.root.parent().unwrap()), DIR_MODE);
            assert_eq!(mode(&paths.local_root), DIR_MODE);
            assert_eq!(mode(&paths.local_servers), DIR_MODE);
            assert_eq!(mode(&paths.config), FILE_MODE);
            assert_eq!(mode(&paths.lock), FILE_MODE);
        }
        let again = init(&paths, false).unwrap();
        assert!(again.created.is_empty());
        assert!(init(&paths, true).unwrap().created.is_empty());
    }

    #[test]
    fn init_completes_a_partial_store() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        fs::create_dir(&paths.local_root).unwrap();
        let dry = init(&paths, true).unwrap();
        assert_eq!(
            dry.created,
            [
                paths.lock.clone(),
                paths.config.clone(),
                paths.local_servers.clone()
            ]
        );
        assert!(!paths.config.exists());
        let report = init(&paths, false).unwrap();
        // A dry run lists the paths in the order a real run creates them.
        assert_eq!(report.created, dry.created);
    }

    #[test]
    fn init_refuses_foreign_and_invalid_paths() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("notes.txt"), "x").unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        assert!(matches!(
            init(&paths, false),
            Err(Error::ForeignDirectory { .. })
        ));
        assert!(matches!(
            init(&paths, true),
            Err(Error::ForeignDirectory { .. })
        ));
        let file = StorePaths::new(temp.path().join("notes.txt"));
        assert!(matches!(
            init(&file, false),
            Err(Error::StorePathInvalid { .. })
        ));
        let blocked = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(blocked.path().to_path_buf());
        fs::write(&paths.local_root, "not a directory").unwrap();
        fs::write(&paths.config, "schema_version = 1\n").unwrap();
        assert!(matches!(
            init(&paths, false),
            Err(Error::StorePathInvalid { .. })
        ));
    }

    #[test]
    fn init_never_rewrites_a_corrupt_config() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        fs::write(&paths.config, "schema_version = 7\n").unwrap();
        assert!(matches!(
            init(&paths, false),
            Err(Error::SchemaUnsupported { found: 7, .. })
        ));
        fs::write(&paths.config, b"\xff\xfe").unwrap();
        assert!(matches!(
            init(&paths, false),
            Err(Error::ConfigCorrupt { .. })
        ));
        assert_eq!(fs::read(&paths.config).unwrap(), b"\xff\xfe");
    }

    #[test]
    fn load_config_reports_missing_and_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        assert!(matches!(
            paths.load_config(),
            Err(Error::StoreNotInitialized { .. })
        ));
        assert!(paths.require_initialized().is_err());
        fs::create_dir(&paths.config).unwrap();
        assert!(paths.require_initialized().is_ok());
        assert!(matches!(paths.load_config(), Err(Error::Io { .. })));
    }

    #[test]
    fn lock_with_zero_delays_reports_store_locked() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        let held = StoreLock::acquire_with_delays(&paths, &[Duration::ZERO]).unwrap();
        let second = StoreLock::acquire_with_delays(&paths, &[Duration::ZERO, Duration::ZERO]);
        let error = second.unwrap_err();
        assert!(matches!(error, Error::StoreLocked { .. }));
        assert_eq!(error.exit_code(), 3);
        drop(held);
        assert!(StoreLock::acquire_with_delays(&paths, &[Duration::ZERO]).is_ok());
        let missing = StorePaths::new(temp.path().join("missing"));
        assert!(matches!(
            StoreLock::acquire_with_delays(&missing, &[Duration::ZERO]),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn lock_retries_with_a_delay() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        let _held = StoreLock::acquire_with_delays(&paths, &[Duration::ZERO]).unwrap();
        let result =
            StoreLock::acquire_with_delays(&paths, &[Duration::ZERO, Duration::from_millis(1)]);
        assert!(matches!(result, Err(Error::StoreLocked { .. })));
    }

    #[test]
    fn interrupted_atomic_write_keeps_the_old_file() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("config.toml");
        fs::write(&target, "old").unwrap();
        let result = write_atomic_with(&target, b"new", |temp_path| {
            assert!(temp_path.exists());
            Err(io::Error::other("injected failure"))
        });
        assert!(matches!(result, Err(Error::Io { .. })));
        assert_eq!(fs::read_to_string(&target).unwrap(), "old");
        let leftovers: Vec<_> = fs::read_dir(temp.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "the temp file is removed");
        assert_eq!(write_atomic(&target, b"new").unwrap(), None);
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        assert!(matches!(
            write_atomic(&temp.path().join("missing").join("f"), b"x"),
            Err(Error::Io { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn failed_parent_sync_after_rename_is_a_warning() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("store");
        fs::create_dir(&dir).unwrap();
        let target = dir.join("config.toml");
        fs::write(&target, "old").unwrap();
        // Write and search permission still allow the rename, but without
        // read permission the directory cannot be opened for its fsync.
        let result = write_atomic_with(&target, b"new", |_| {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o300))
        });
        let opened_anyway = File::open(&dir).is_ok();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let warning = result.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
        if opened_anyway {
            // Running with privileges that ignore permission bits.
            assert_eq!(warning, None);
            return;
        }
        let warning = warning.expect("a warning for the failed fsync");
        assert_eq!(
            warning,
            StoreWarning::ParentSyncFailed {
                path: target.clone(),
                kind: io::ErrorKind::PermissionDenied
            }
        );
        let text = warning.to_string();
        assert!(text.contains("could not sync its directory"), "{text}");
        assert!(text.contains(&target.display().to_string()), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn init_follows_a_symlinked_root_and_warns_about_loose_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("dotfiles-store");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = temp.path().join(".d-mcp");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let paths = StorePaths::new(link.clone());
        let dry = init(&paths, true).unwrap();
        assert_eq!(dry.created.len(), 4);
        assert_eq!(
            dry.warnings,
            [StoreWarning::LoosePermissions {
                path: link,
                mode: 0o755
            }]
        );
        let report = init(&paths, false).unwrap();
        assert_eq!(report.created, dry.created);
        assert_eq!(report.warnings, dry.warnings);
        assert!(report.warnings[0].to_string().contains("mode 0755"));
        assert!(real.join("config.toml").is_file());
        assert!(real.join("local/servers").is_dir());
        // The mode is reported, never changed.
        let mode = fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let again = init(&paths, false).unwrap();
        assert!(again.created.is_empty());
        assert!(again.warnings.is_empty());
        // A dangling symlink is not a store.
        let dangling = temp.path().join("dangling");
        std::os::unix::fs::symlink(temp.path().join("nowhere"), &dangling).unwrap();
        assert!(matches!(
            init(&StorePaths::new(dangling), false),
            Err(Error::StorePathInvalid { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_lock_file_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().join("store"));
        fs::create_dir(&paths.root).unwrap();
        let outside = temp.path().join("outside");
        std::os::unix::fs::symlink(&outside, &paths.lock).unwrap();
        assert!(matches!(
            StoreLock::acquire_with_delays(&paths, &[Duration::ZERO]),
            Err(Error::Io { .. })
        ));
        assert!(
            fs::symlink_metadata(&outside).is_err(),
            "the symlink target is not created"
        );
    }
}
