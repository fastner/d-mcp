//! The per-store key of the display hashes, in `<store>/.hash-key`.
//!
//! d-mcp shows a hash of an entry it does not own (an unmanaged entry, a
//! taken-over entry, a project-scope entry) only as a *display hash*: an
//! HMAC-SHA-256 under a random 32-byte key over the adapter's semantic entry
//! hash. A plain entry hash of a foreign entry could be used to guess a
//! low-entropy secret in it by hashing candidates; without the key, a
//! display hash cannot. Owned entries that d-mcp wrote itself keep their
//! plain hash.
//!
//! The key file holds the key as 64 lowercase hex characters and a newline,
//! with mode `0600`. `init` creates it, and every mutating command creates it
//! under the store lock when it is missing. Read-only commands never create
//! it: without a key they show no display hashes and hint `d-mcp init`. The
//! key is never printed; its [`fmt::Debug`] form is redacted.

use std::fmt;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::canonical::hex;
use crate::error::{Error, Result};
use crate::store::{self, CappedReadError, StoreLock, StorePaths, StoreWarning};

/// The key length in bytes.
pub const KEY_LEN: usize = 32;

/// The domain tag of display hashes, put in front of the entry hash.
pub const DISPLAY_HASH_TAG: &str = "d-mcp-display-v1";

/// The largest `.hash-key` file d-mcp reads: the hex key and a newline.
const MAX_KEY_FILE_BYTES: u64 = 2 * KEY_LEN as u64 + 1;

/// The display-hash key of a store.
#[derive(Clone, PartialEq, Eq)]
pub struct HashKey([u8; KEY_LEN]);

impl fmt::Debug for HashKey {
    /// The key is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HashKey(<redacted>)")
    }
}

impl HashKey {
    /// A key from raw bytes (tests and fixtures).
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// A fresh random key from the operating system.
    ///
    /// # Errors
    ///
    /// [`Error::HashKeyUnavailable`] when the system has no random source.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes).map_err(|_| Error::HashKeyUnavailable)?;
        Ok(Self(bytes))
    }

    /// The display hash of an adapter entry hash: HMAC-SHA-256 under this
    /// key over [`DISPLAY_HASH_TAG`], a newline and the entry hash, as 64
    /// lowercase hex characters.
    #[must_use]
    pub fn display_hash(&self, entry_hash: &str) -> String {
        // HMAC accepts keys of any length, so this cannot fail.
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0)
            .unwrap_or_else(|_| unreachable!("HMAC takes keys of any length"));
        mac.update(DISPLAY_HASH_TAG.as_bytes());
        mac.update(b"\n");
        mac.update(entry_hash.as_bytes());
        hex(&mac.finalize().into_bytes())
    }

    /// The file form: the key in hex and a newline.
    fn render(&self) -> String {
        format!("{}\n", hex(&self.0))
    }

    /// Parses the file form.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let text = text.strip_suffix('\n').unwrap_or(text);
        if text.len() != 2 * KEY_LEN
            || !text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let mut key = [0u8; KEY_LEN];
        for (index, slot) in key.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).ok()?;
        }
        Some(Self(key))
    }
}

/// Reads the key of a store without creating it. `Ok(None)` when the file
/// does not exist.
///
/// # Errors
///
/// [`Error::HashKeyCorrupt`] for a file that is not a regular file, is too
/// large, or does not hold a key, and [`Error::Io`] when it cannot be read.
pub fn load(paths: &StorePaths) -> Result<Option<HashKey>> {
    let path = &paths.hash_key;
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    let corrupt = || Error::HashKeyCorrupt { path: path.clone() };
    let bytes =
        store::read_regular_capped(path, MAX_KEY_FILE_BYTES).map_err(|error| match error {
            CappedReadError::NotRegular | CappedReadError::TooLarge => corrupt(),
            CappedReadError::Io(error) => Error::io("read", path, &error),
        })?;
    HashKey::parse(&bytes).map(Some).ok_or_else(corrupt)
}

/// Returns the key of a store, creating it when it is missing. Call only
/// while holding the lock. Returns whether the key was created, and a
/// warning when only the final directory fsync of the write failed.
///
/// # Errors
///
/// The [`load`] errors (an existing corrupt key is never replaced),
/// [`Error::HashKeyUnavailable`], and [`Error::Io`] when the write fails.
pub fn ensure(
    paths: &StorePaths,
    _lock: &StoreLock,
) -> Result<(HashKey, bool, Option<StoreWarning>)> {
    if let Some(key) = load(paths)? {
        return Ok((key, false, None));
    }
    let key = HashKey::generate()?;
    let warning = store::write_atomic(&paths.hash_key, key.render().as_bytes())?;
    Ok((key, true, warning))
}

/// [`ensure`] for a mutating command that does not need the key itself:
/// creates it under the held lock when it is missing and returns only the
/// write warning, if any.
///
/// # Errors
///
/// See [`ensure`].
pub fn ensure_under_lock(paths: &StorePaths, lock: &StoreLock) -> Result<Option<StoreWarning>> {
    ensure(paths, lock).map(|(_, _, warning)| warning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn paths() -> (tempfile::TempDir, StorePaths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        (temp, paths)
    }

    #[test]
    fn display_hashes_are_keyed_and_stable() {
        let a = HashKey::from_bytes([1; KEY_LEN]);
        let b = HashKey::from_bytes([2; KEY_LEN]);
        let entry = "0".repeat(64);
        assert_eq!(a.display_hash(&entry), a.display_hash(&entry));
        assert_ne!(a.display_hash(&entry), b.display_hash(&entry));
        assert_ne!(a.display_hash(&entry), a.display_hash(&"1".repeat(64)));
        assert_eq!(a.display_hash(&entry).len(), 64);
        assert_ne!(a.display_hash(&entry), entry);
        assert_eq!(format!("{a:?}"), "HashKey(<redacted>)");
    }

    #[test]
    fn the_file_form_round_trips_and_rejects_anything_else() {
        let key = HashKey::from_bytes([0xab; KEY_LEN]);
        assert_eq!(HashKey::parse(key.render().as_bytes()), Some(key.clone()));
        assert_eq!(
            HashKey::parse(key.render().trim_end().as_bytes()),
            Some(key)
        );
        for bad in [
            &b""[..],
            b"ab",
            &[b'A'; 64],
            &[b'g'; 64],
            &[b'a'; 66],
            b"\xff\xfe",
        ] {
            assert_eq!(HashKey::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn ensure_creates_once_and_load_never_creates() {
        let (_temp, paths) = paths();
        assert_eq!(load(&paths).unwrap(), None);
        assert!(!paths.hash_key.exists());
        let lock = StoreLock::acquire(&paths).unwrap();
        let (key, created, _) = ensure(&paths, &lock).unwrap();
        assert!(created);
        let (again, created, _) = ensure(&paths, &lock).unwrap();
        assert!(!created);
        assert_eq!(key, again);
        assert_eq!(load(&paths).unwrap(), Some(key));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.hash_key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_corrupt_key_is_reported_and_never_replaced() {
        let (_temp, paths) = paths();
        fs::write(&paths.hash_key, "not a key").unwrap();
        assert_eq!(load(&paths).unwrap_err().code(), "hash-key-corrupt");
        let lock = StoreLock::acquire(&paths).unwrap();
        assert_eq!(
            ensure(&paths, &lock).unwrap_err().code(),
            "hash-key-corrupt"
        );
        assert_eq!(fs::read(&paths.hash_key).unwrap(), b"not a key");
        fs::remove_file(&paths.hash_key).unwrap();
        fs::write(&paths.hash_key, vec![b'a'; 200]).unwrap();
        assert_eq!(load(&paths).unwrap_err().code(), "hash-key-corrupt");
        fs::remove_file(&paths.hash_key).unwrap();
        fs::create_dir(&paths.hash_key).unwrap();
        assert_eq!(load(&paths).unwrap_err().code(), "hash-key-corrupt");
        #[cfg(unix)]
        {
            fs::remove_dir(&paths.hash_key).unwrap();
            std::os::unix::fs::symlink(paths.root.join("elsewhere"), &paths.hash_key).unwrap();
            assert_eq!(load(&paths).unwrap_err().code(), "hash-key-corrupt");
        }
    }
}
