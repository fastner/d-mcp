//! The neutral server definition: model, defaults, validation and hashes.
//!
//! A definition file `servers/<name>.toml` is parsed in two phases: first
//! into the untyped, spanned tree of [`crate::spanned`], then by the field
//! walker in `walk`, which tracks the field path and maps every failure to
//! a [`Rule`]. Errors carry file position, field path and rule code only;
//! they never echo a value. This module does no I/O.

mod hash;
mod walk;

use std::collections::BTreeMap;
use std::fmt;

use crate::rule::Rule;

pub use hash::{CORE_DOMAIN_TAG, DEFINITION_DOMAIN_TAG, canonical_json};

/// Maximum length of a neutral name.
///
/// It follows from a 64-character model tool-name limit minus `mcp__` and
/// `__`, which leaves at least 25 characters for tool names. Claude Code
/// 2.1.310 validates tool names against `^[a-zA-Z0-9_-]{1,64}$`.
pub const MAX_NAME_LEN: usize = 32;

/// A validated server definition, with defaults applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    /// The neutral name; equals the file stem.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// How the harness reaches the server.
    pub transport: Transport,
    /// Opaque per-harness override tables (`claude`, `codex`), as JSON
    /// objects. Empty tables are dropped.
    pub overrides: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
}

/// The TOML kind of a top-level override value, as written in the file. It
/// keeps what the JSON form loses: a datetime is not a string.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverrideKind {
    /// A string.
    String,
    /// An integer.
    Integer(i64),
    /// A finite float.
    Float(f64),
    /// A boolean.
    Boolean,
    /// A datetime.
    Datetime,
    /// An array; `all_strings` is true when every item is a string.
    Array {
        /// Whether every item is a string (also true for an empty array).
        all_strings: bool,
    },
    /// A table.
    Table,
}

/// Where a top-level override key was written.
#[derive(Debug, Clone, PartialEq)]
pub struct OverrideSite {
    /// The harness table: `codex` or `claude`.
    pub harness: String,
    /// The key.
    pub key: String,
    /// The value's TOML kind.
    pub kind: OverrideKind,
    /// 1-based line of the key.
    pub line: usize,
    /// 1-based column of the key.
    pub column: usize,
}

/// The override sites of a definition, in file order.
///
/// Positions are diagnostics, not part of the model, so they are kept apart
/// from [`Definition`]: [`parse_definition_with_sites`] returns them next to
/// it, and two definitions that differ only in formatting stay equal. Their
/// contents never enter a hash.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverrideSites(pub Vec<OverrideSite>);

impl OverrideSites {
    /// The sites of one harness table.
    pub fn of<'a>(&'a self, harness: &'a str) -> impl Iterator<Item = &'a OverrideSite> + 'a {
        self.0.iter().filter(move |site| site.harness == harness)
    }
}

/// The transport of a definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A local command speaking MCP over stdio.
    Stdio {
        /// A bare program name or an absolute path.
        command: String,
        /// Arguments; defaults to `[]`.
        args: Vec<String>,
        /// Environment entries by variable name; defaults to `{}`.
        env: BTreeMap<String, EnvValue>,
    },
    /// A remote server reached over streamable HTTP.
    Http {
        /// The `https` URL (or `http` for loopback hosts), without a query.
        url: String,
        /// The auth method; defaults to OAuth.
        auth: Auth,
    },
}

/// An environment entry of a stdio server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvValue {
    /// `{ forward = true }`: passed through under the same name.
    Forward,
    /// `{ value = "..." }`: a non-secret literal.
    Literal(String),
}

/// The auth method of an HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// OAuth, the default. Tokens never leave the harness.
    OAuth {
        /// Optional non-secret client id.
        client_id: Option<String>,
        /// Requested scopes, sorted and deduplicated; defaults to `[]`.
        scopes: Vec<String>,
        /// Variable holding the client secret; requires `client_id`.
        client_secret_env: Option<String>,
    },
    /// A static bearer token read from an environment variable.
    BearerEnv {
        /// The variable name.
        var: String,
        /// Whether the server also supports OAuth; defaults to `false`.
        oauth_supported: bool,
    },
    /// A static header value read from an environment variable.
    HeaderEnv {
        /// The header name, an RFC 9110 token.
        header: String,
        /// The variable name.
        var: String,
    },
    /// No authentication.
    None,
}

impl Auth {
    /// The stable `type` value of this auth method.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Auth::OAuth { .. } => "oauth",
            Auth::BearerEnv { .. } => "bearer_env",
            Auth::HeaderEnv { .. } => "header_env",
            Auth::None => "none",
        }
    }
}

impl Definition {
    /// The transport kind: `stdio` or `http`.
    #[must_use]
    pub fn transport_kind(&self) -> &'static str {
        match self.transport {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
        }
    }

    /// The auth kind of an HTTP server; `None` for stdio.
    #[must_use]
    pub fn auth_kind(&self) -> Option<&'static str> {
        match &self.transport {
            Transport::Stdio { .. } => None,
            Transport::Http { auth, .. } => Some(auth.kind()),
        }
    }

    /// The names of all environment variables the definition reads from the
    /// user's environment, sorted: stdio forwards and the auth variables.
    /// Literal values are never included.
    #[must_use]
    pub fn forwarded_env(&self) -> Vec<String> {
        let mut names: Vec<String> = match &self.transport {
            Transport::Stdio { env, .. } => env
                .iter()
                .filter(|(_, value)| matches!(value, EnvValue::Forward))
                .map(|(name, _)| name.clone())
                .collect(),
            Transport::Http { auth, .. } => match auth {
                Auth::OAuth {
                    client_secret_env, ..
                } => client_secret_env.iter().cloned().collect(),
                Auth::BearerEnv { var, .. } | Auth::HeaderEnv { var, .. } => vec![var.clone()],
                Auth::None => Vec::new(),
            },
        };
        names.sort();
        names
    }

    /// `definition_hash`: SHA-256 over the domain tag `d-mcp-definition-v1`,
    /// a newline and the canonical JSON of the full model, as lowercase hex.
    #[must_use]
    pub fn definition_hash(&self) -> String {
        hash::definition_hash(self)
    }

    /// `core_hash`: SHA-256 over the domain tag `d-mcp-core-v1`, a newline
    /// and the canonical JSON of the model without `description` and without
    /// the override tables, as lowercase hex.
    #[must_use]
    pub fn core_hash(&self) -> String {
        hash::core_hash(self)
    }
}

/// A definition that failed validation. It names the rule and the position
/// only; the offending value is never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionError {
    /// The broken rule.
    pub rule: Rule,
    /// The field path, such as `transport.args[1]`; empty for the whole file.
    pub field: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, in Unicode scalars.
    pub column: usize,
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}: ", self.line, self.column)?;
        if !self.field.is_empty() {
            write!(f, "field `{}` ", self.field)?;
        }
        write!(f, "breaks rule `{}`", self.rule)
    }
}

impl std::error::Error for DefinitionError {}

/// Parses and validates the text of `servers/<stem>.toml`.
///
/// # Errors
///
/// Returns the first broken rule in walk order.
pub fn parse_definition(text: &str, stem: &str) -> Result<Definition, DefinitionError> {
    walk::walk(text, stem).map(|(definition, _)| definition)
}

/// Like [`parse_definition`], and also returns the TOML kind and position of
/// every top-level override key, for the adapter-specific `validate` step.
///
/// # Errors
///
/// The same as [`parse_definition`].
pub fn parse_definition_with_sites(
    text: &str,
    stem: &str,
) -> Result<(Definition, OverrideSites), DefinitionError> {
    walk::walk(text, stem)
}

/// The neutral name rule, shared by server names and source ids: lowercase
/// letters, digits, `-` and `_`; starts with a letter or digit; does not end
/// with `-` or `_`; no `__`; at most [`MAX_NAME_LEN`] characters.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else {
        return false;
    };
    bytes.len() <= MAX_NAME_LEN
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && !matches!(last, b'-' | b'_')
        && !name.contains("__")
}

/// The variable name rule: `^[A-Za-z_][A-Za-z0-9_]*$`.
#[must_use]
pub fn is_valid_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The header name rule: a non-empty RFC 9110 token.
#[must_use]
pub fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                        | '~'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rule() {
        for name in [
            "github",
            "a",
            "0x",
            "my-server",
            "my_server",
            &"a".repeat(32),
        ] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in [
            "",
            "GitHub",
            "-a",
            "_a",
            "a-",
            "a_",
            "a__b",
            "a.b",
            "a b",
            &"a".repeat(33),
        ] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    #[test]
    fn variable_and_header_names() {
        assert!(is_valid_variable_name("GITHUB_TOKEN"));
        assert!(is_valid_variable_name("_x1"));
        assert!(!is_valid_variable_name("1X"));
        assert!(!is_valid_variable_name("A-B"));
        assert!(!is_valid_variable_name(""));
        assert!(is_valid_header_name("X-Api-Key"));
        assert!(is_valid_header_name("!#$%&'*+-.^_`|~"));
        assert!(!is_valid_header_name("X Api"));
        assert!(!is_valid_header_name("X:Y"));
        assert!(!is_valid_header_name(""));
    }

    #[test]
    fn error_display_never_contains_values() {
        let error = DefinitionError {
            rule: Rule::SecretPrefix,
            field: "transport.args[0]".to_owned(),
            line: 3,
            column: 9,
        };
        assert_eq!(
            error.to_string(),
            "line 3, column 9: field `transport.args[0]` breaks rule `secret-prefix`"
        );
        let error = DefinitionError {
            field: String::new(),
            ..error
        };
        assert_eq!(
            error.to_string(),
            "line 3, column 9: breaks rule `secret-prefix`"
        );
    }

    #[test]
    fn override_sites_keep_kind_and_position() {
        let (definition, sites) = parse_definition_with_sites(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
             [harness.codex]\ncwd = 1979-05-27T07:32:00Z\nn = 1\nf = 1.5\nb = true\n\
             s = \"v\"\nt = [\"x\"]\nm = [1]\nsub = { k = 1 }\n[harness.claude]\nk = 1\n",
            "a",
        )
        .unwrap();
        let codex: Vec<_> = sites.of("codex").collect();
        assert_eq!(codex.len(), 8);
        assert_eq!(codex[0].kind, OverrideKind::Datetime);
        assert_eq!((codex[0].line, codex[0].column), (6, 1));
        assert_eq!(codex[1].kind, OverrideKind::Integer(1));
        assert_eq!(codex[2].kind, OverrideKind::Float(1.5));
        assert_eq!(codex[3].kind, OverrideKind::Boolean);
        assert_eq!(codex[4].kind, OverrideKind::String);
        assert_eq!(codex[5].kind, OverrideKind::Array { all_strings: true });
        assert_eq!(codex[6].kind, OverrideKind::Array { all_strings: false });
        assert_eq!(codex[7].kind, OverrideKind::Table);
        assert_eq!(sites.of("claude").count(), 1);
        // Positions are not part of the definition: the same model written
        // on other lines is equal, while its sites differ.
        let (moved, moved_sites) = parse_definition_with_sites(
            "name = \"a\"\n\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
             [harness.codex]\n\ncwd = 1979-05-27T07:32:00Z\nn = 1\nf = 1.5\nb = true\n\
             s = \"v\"\nt = [\"x\"]\nm = [1]\nsub = { k = 1 }\n[harness.claude]\nk = 1\n",
            "a",
        )
        .unwrap();
        assert_eq!(moved, definition);
        assert_ne!(moved_sites, sites);
    }

    #[test]
    fn accessors() {
        let stdio = parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
             [env]\nB = { forward = true }\nA = { forward = true }\nC = { value = \"1\" }\n",
            "a",
        )
        .unwrap();
        assert_eq!(stdio.transport_kind(), "stdio");
        assert_eq!(stdio.auth_kind(), None);
        assert_eq!(stdio.forwarded_env(), ["A", "B"]);
        let cases = [
            (
                "type = \"oauth\"\nclient_id = \"c\"\nclient_secret_env = \"S\"",
                "oauth",
                vec!["S"],
            ),
            (
                "type = \"bearer_env\"\nvar = \"T\"",
                "bearer_env",
                vec!["T"],
            ),
            (
                "type = \"header_env\"\nheader = \"X-Key\"\nvar = \"K\"",
                "header_env",
                vec!["K"],
            ),
            ("type = \"none\"", "none", vec![]),
        ];
        for (auth, kind, vars) in cases {
            let text = format!(
                "name = \"h\"\n[transport]\ntype = \"http\"\nurl = \"https://x.test/mcp\"\n[auth]\n{auth}\n"
            );
            let http = parse_definition(&text, "h").unwrap();
            assert_eq!(http.transport_kind(), "http");
            assert_eq!(http.auth_kind(), Some(kind));
            assert_eq!(http.forwarded_env(), vars);
        }
    }
}
