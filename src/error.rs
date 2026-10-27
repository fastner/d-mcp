//! The error enum, its exit codes and its stable error codes.
//!
//! Every variant maps to exactly one documented exit code and one stable
//! error code (see `docs/reference.md`). Messages name paths, ids, rule codes
//! and positions only; they never contain a definition or config value.
//!
//! | Exit | Meaning |
//! |---:|---|
//! | 0 | Success, including resolution problems, holds and warnings |
//! | 1 | Expected failure |
//! | 2 | Usage error (clap) |
//! | 3 | Unsafe state |
//! | 4 | Environment problem |

use std::io;
use std::path::PathBuf;

use crate::rule::Rule;

/// Exit code for an expected failure.
pub const EXIT_FAILURE: u8 = 1;
/// Exit code for a clap usage error.
pub const EXIT_USAGE: u8 = 2;
/// Exit code for an unsafe store state.
pub const EXIT_UNSAFE: u8 = 3;
/// Exit code for an environment problem.
pub const EXIT_ENVIRONMENT: u8 = 4;

/// Result alias for d-mcp operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A d-mcp error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The store directory has no `config.toml`.
    #[error("the store at {} is not initialized; run `d-mcp init`", path.display())]
    StoreNotInitialized {
        /// The store root.
        path: PathBuf,
    },
    /// The store path exists but is not a directory.
    #[error("the store path {} exists but is not a directory", path.display())]
    StorePathInvalid {
        /// The store root.
        path: PathBuf,
    },
    /// `init` was pointed at a non-empty directory that is not a store.
    #[error("{} is not empty and is not a d-mcp store; refusing to initialize it", path.display())]
    ForeignDirectory {
        /// The directory.
        path: PathBuf,
    },
    /// A name argument breaks the neutral name rule.
    #[error(
        "the {what} is not a valid name: use lowercase letters, digits, `-` and `_`, start with a \
         letter or digit, do not end with `-` or `_`, avoid `__`, at most 32 characters"
    )]
    InvalidName {
        /// What was named: `source id` or `server name`.
        what: &'static str,
    },
    /// The id `local` belongs to the built-in source.
    #[error(
        "the source id `local` is reserved for the built-in local source, which cannot be added, removed or re-prioritized"
    )]
    SourceIdReserved,
    /// The source id is already registered.
    #[error("a source with id `{id}` already exists")]
    SourceIdTaken {
        /// The id.
        id: String,
    },
    /// The priority is 0.
    #[error("the priority must be at least 1 (0 belongs to the built-in local source)")]
    PriorityInvalid,
    /// The priority is used by another source.
    #[error("priority {priority} is already used by source `{id}`")]
    PriorityTaken {
        /// The priority.
        priority: u32,
        /// The source using it.
        id: String,
    },
    /// The source path does not exist or cannot be resolved.
    #[error("the source path {} does not exist", path.display())]
    SourcePathNotFound {
        /// The path as given, after `~` expansion.
        path: PathBuf,
    },
    /// The source path is not a directory.
    #[error("the source path {} is not a directory", path.display())]
    SourcePathNotDirectory {
        /// The canonical path.
        path: PathBuf,
    },
    /// The source path is not valid UTF-8.
    #[error("the source path is not valid UTF-8")]
    SourcePathNotUtf8,
    /// The path is already registered under another id.
    #[error("the source path {} is already registered as `{id}`", path.display())]
    SourcePathRegistered {
        /// The canonical path.
        path: PathBuf,
        /// The registered id.
        id: String,
    },
    /// The path lies inside the store.
    #[error("the source path {} lies inside the store; use the built-in `local` source instead", path.display())]
    SourcePathInStore {
        /// The canonical path.
        path: PathBuf,
    },
    /// No source with this id exists.
    #[error("no source with id `{id}` exists")]
    SourceNotFound {
        /// The id.
        id: String,
    },
    /// `unselect` of a server that is not selected.
    #[error("`{server}` is not selected")]
    NotSelected {
        /// The server name.
        server: String,
    },
    /// `config.toml` breaks a rule. It is never rewritten.
    #[error(
        "{} is corrupt at line {line}, column {column}{} (rule `{rule}`); fix it by hand, d-mcp will not rewrite it",
        path.display(),
        if field.is_empty() { String::new() } else { format!(", field `{field}`") }
    )]
    ConfigCorrupt {
        /// The file.
        path: PathBuf,
        /// 1-based line.
        line: usize,
        /// 1-based column.
        column: usize,
        /// The field path.
        field: String,
        /// The broken rule.
        rule: Rule,
    },
    /// A persisted file declares an unsupported schema version.
    #[error("{} has schema_version {found}, but this d-mcp supports only version 1", path.display())]
    SchemaUnsupported {
        /// The file.
        path: PathBuf,
        /// The declared version.
        found: i64,
    },
    /// Another process holds the store lock.
    #[error("the store is locked by another d-mcp process ({}); try again later", path.display())]
    StoreLocked {
        /// The lock file.
        path: PathBuf,
    },
    /// `HOME` is unset or empty, but it is needed.
    #[error("HOME is unset or empty; set HOME, D_MCP_STORE or pass --store")]
    HomeUnset,
    /// The current directory cannot be determined.
    #[error("the current directory cannot be determined")]
    CurrentDirUnavailable,
    /// A file-system operation failed.
    #[error("cannot {operation} {}: {}", path.display(), io::Error::from(*kind))]
    Io {
        /// What d-mcp was doing.
        operation: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The I/O error kind (the OS message is not kept).
        kind: io::ErrorKind,
    },
}

impl Error {
    /// Builds an [`Error::Io`] from an I/O error.
    #[must_use]
    pub fn io(operation: &'static str, path: impl Into<PathBuf>, error: &io::Error) -> Self {
        Error::Io {
            operation,
            path: path.into(),
            kind: error.kind(),
        }
    }

    /// The stable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::StoreNotInitialized { .. } => "store-not-initialized",
            Error::StorePathInvalid { .. } => "store-path-invalid",
            Error::ForeignDirectory { .. } => "foreign-directory",
            Error::InvalidName { .. } => "invalid-name",
            Error::SourceIdReserved => "source-id-reserved",
            Error::SourceIdTaken { .. } => "source-id-taken",
            Error::PriorityInvalid => "priority-invalid",
            Error::PriorityTaken { .. } => "priority-taken",
            Error::SourcePathNotFound { .. } => "source-path-not-found",
            Error::SourcePathNotDirectory { .. } => "source-path-not-directory",
            Error::SourcePathNotUtf8 => "source-path-not-utf8",
            Error::SourcePathRegistered { .. } => "source-path-registered",
            Error::SourcePathInStore { .. } => "source-path-in-store",
            Error::SourceNotFound { .. } => "source-not-found",
            Error::NotSelected { .. } => "not-selected",
            Error::ConfigCorrupt { .. } => "config-corrupt",
            Error::SchemaUnsupported { .. } => "schema-unsupported",
            Error::StoreLocked { .. } => "store-locked",
            Error::HomeUnset => "home-unset",
            Error::CurrentDirUnavailable => "cwd-unavailable",
            Error::Io { .. } => "io-error",
        }
    }

    /// The documented exit code.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::StoreNotInitialized { .. }
            | Error::StorePathInvalid { .. }
            | Error::ForeignDirectory { .. }
            | Error::InvalidName { .. }
            | Error::SourceIdReserved
            | Error::SourceIdTaken { .. }
            | Error::PriorityInvalid
            | Error::PriorityTaken { .. }
            | Error::SourcePathNotFound { .. }
            | Error::SourcePathNotDirectory { .. }
            | Error::SourcePathNotUtf8
            | Error::SourcePathRegistered { .. }
            | Error::SourcePathInStore { .. }
            | Error::SourceNotFound { .. }
            | Error::NotSelected { .. } => EXIT_FAILURE,
            Error::ConfigCorrupt { .. }
            | Error::SchemaUnsupported { .. }
            | Error::StoreLocked { .. } => EXIT_UNSAFE,
            Error::HomeUnset | Error::CurrentDirUnavailable | Error::Io { .. } => EXIT_ENVIRONMENT,
        }
    }

    /// The human-readable hint; the same text as [`std::fmt::Display`].
    #[must_use]
    pub fn hint(&self) -> String {
        self.to_string()
    }

    /// One sample of every variant, for contract tests. The exhaustive match
    /// makes adding a variant without a sample a compile error.
    #[must_use]
    pub fn samples() -> Vec<Error> {
        let path = PathBuf::from("/store");
        let samples = vec![
            Error::StoreNotInitialized { path: path.clone() },
            Error::StorePathInvalid { path: path.clone() },
            Error::ForeignDirectory { path: path.clone() },
            Error::InvalidName { what: "source id" },
            Error::SourceIdReserved,
            Error::SourceIdTaken { id: "team".into() },
            Error::PriorityInvalid,
            Error::PriorityTaken {
                priority: 1,
                id: "team".into(),
            },
            Error::SourcePathNotFound { path: path.clone() },
            Error::SourcePathNotDirectory { path: path.clone() },
            Error::SourcePathNotUtf8,
            Error::SourcePathRegistered {
                path: path.clone(),
                id: "team".into(),
            },
            Error::SourcePathInStore { path: path.clone() },
            Error::SourceNotFound { id: "team".into() },
            Error::NotSelected {
                server: "github".into(),
            },
            Error::ConfigCorrupt {
                path: path.clone(),
                line: 1,
                column: 1,
                field: "sources[0].id".into(),
                rule: Rule::NameRule,
            },
            Error::SchemaUnsupported {
                path: path.clone(),
                found: 2,
            },
            Error::StoreLocked { path: path.clone() },
            Error::HomeUnset,
            Error::CurrentDirUnavailable,
            Error::Io {
                operation: "read",
                path,
                kind: io::ErrorKind::PermissionDenied,
            },
        ];
        for sample in &samples {
            // Exhaustiveness guard: a new variant must be listed here and
            // above.
            match sample {
                Error::StoreNotInitialized { .. }
                | Error::StorePathInvalid { .. }
                | Error::ForeignDirectory { .. }
                | Error::InvalidName { .. }
                | Error::SourceIdReserved
                | Error::SourceIdTaken { .. }
                | Error::PriorityInvalid
                | Error::PriorityTaken { .. }
                | Error::SourcePathNotFound { .. }
                | Error::SourcePathNotDirectory { .. }
                | Error::SourcePathNotUtf8
                | Error::SourcePathRegistered { .. }
                | Error::SourcePathInStore { .. }
                | Error::SourceNotFound { .. }
                | Error::NotSelected { .. }
                | Error::ConfigCorrupt { .. }
                | Error::SchemaUnsupported { .. }
                | Error::StoreLocked { .. }
                | Error::HomeUnset
                | Error::CurrentDirUnavailable
                | Error::Io { .. } => {}
            }
        }
        samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFERENCE: &str = include_str!("../docs/reference.md");

    /// Parses the error-code table of `docs/reference.md`: rows of the form
    /// `` | `code` | exit | ... | ``.
    fn documented_codes() -> Vec<(String, u8)> {
        let section = REFERENCE
            .split("<!-- error-codes:start -->")
            .nth(1)
            .and_then(|rest| rest.split("<!-- error-codes:end -->").next())
            .expect("docs/reference.md has an error-code table");
        section
            .lines()
            .filter_map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                let code = cells.get(1)?.strip_prefix('`')?.strip_suffix('`')?;
                let exit = cells.get(2)?.parse().ok()?;
                Some((code.to_owned(), exit))
            })
            .collect()
    }

    #[test]
    fn every_variant_matches_the_documented_table() {
        let documented = documented_codes();
        let samples = Error::samples();
        assert_eq!(
            documented.len(),
            samples.len(),
            "the documented table and the enum differ in size"
        );
        for sample in &samples {
            assert!(
                documented.contains(&(sample.code().to_owned(), sample.exit_code())),
                "`{}` with exit {} is not documented",
                sample.code(),
                sample.exit_code()
            );
            assert!(!sample.hint().is_empty());
        }
    }

    #[test]
    fn exit_codes_are_in_the_documented_range() {
        for sample in Error::samples() {
            assert!(matches!(
                sample.exit_code(),
                EXIT_FAILURE | EXIT_UNSAFE | EXIT_ENVIRONMENT
            ));
        }
        assert_eq!(EXIT_USAGE, 2);
    }

    #[test]
    fn io_errors_keep_only_the_kind() {
        let error = Error::io(
            "read",
            "/store/config.toml",
            &io::Error::new(io::ErrorKind::NotFound, "ghp_secret in an OS message"),
        );
        assert!(!error.to_string().contains("ghp_secret"));
        assert_eq!(error.code(), "io-error");
        assert_eq!(error.exit_code(), EXIT_ENVIRONMENT);
    }

    #[test]
    fn corrupt_config_hint_names_position_and_rule() {
        let error = Error::ConfigCorrupt {
            path: PathBuf::from("/s/config.toml"),
            line: 3,
            column: 7,
            field: String::new(),
            rule: Rule::TomlSyntax,
        };
        assert_eq!(
            error.hint(),
            "/s/config.toml is corrupt at line 3, column 7 (rule `toml-syntax`); fix it by hand, d-mcp will not rewrite it"
        );
    }
}
