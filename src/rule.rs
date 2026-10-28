//! Stable rule codes for definition, source-file and `config.toml` problems.
//!
//! A rule code names *which* rule a file broke. Problems report the code
//! together with file, line, column and field path, never the offending
//! value. The codes are part of the documented JSON contract in
//! `docs/reference.md`.

use serde::Serialize;

macro_rules! rules {
    ($($variant:ident => $code:literal,)*) => {
        /// A stable rule code.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Rule {
            $(
                #[doc = concat!("`", $code, "`")]
                $variant,
            )*
        }

        impl Rule {
            /// Every rule, in declaration order.
            pub const ALL: &'static [Rule] = &[$(Rule::$variant,)*];

            /// The stable code of this rule.
            #[must_use]
            pub fn code(self) -> &'static str {
                match self {
                    $(Rule::$variant => $code,)*
                }
            }

            /// Looks a rule up by its stable code.
            #[must_use]
            pub fn from_code(code: &str) -> Option<Rule> {
                match code {
                    $($code => Some(Rule::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

rules! {
    // TOML and file level.
    TomlSyntax => "toml-syntax",
    NotUtf8 => "not-utf8",
    FileTooLarge => "file-too-large",
    NotRegularFile => "not-regular-file",
    Unreadable => "unreadable",
    InvalidStem => "invalid-stem",
    // Structure.
    UnknownKey => "unknown-key",
    MissingField => "missing-field",
    WrongType => "wrong-type",
    UnsupportedType => "unsupported-type",
    InvalidEnvEntry => "invalid-env-entry",
    TransportMismatch => "transport-mismatch",
    NonFiniteFloat => "non-finite-float",
    // Field rules.
    NameRule => "name-rule",
    NameMismatch => "name-mismatch",
    VariableName => "variable-name",
    HeaderName => "header-name",
    ClientSecretWithoutClientId => "client-secret-without-client-id",
    CommandRule => "command-rule",
    // URL rules.
    UrlInvalid => "url-invalid",
    UrlScheme => "url-scheme",
    UrlQuery => "url-query",
    UrlUserinfo => "url-userinfo",
    // Secret rules.
    VariableSyntax => "variable-syntax",
    SecretName => "secret-name",
    SecretPrefix => "secret-prefix",
    SecretJwt => "secret-jwt",
    SecretHex => "secret-hex",
    SecretEntropy => "secret-entropy",
    // Harness override tables (`validate`).
    OverrideKey => "override-key",
    OverrideType => "override-type",
    OverrideTransport => "override-transport",
    // `config.toml` only.
    InvalidValue => "invalid-value",
    DuplicateEntry => "duplicate-entry",
    // Notices about ignored entries in `servers/`.
    HiddenFile => "hidden-file",
    NonTomlFile => "non-toml-file",
    Subdirectory => "subdirectory",
}

impl Serialize for Rule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for rule in Rule::ALL {
            assert_eq!(Rule::from_code(rule.code()), Some(*rule));
            assert!(seen.insert(rule.code()), "duplicate {}", rule.code());
            assert_eq!(rule.to_string(), rule.code());
        }
        assert_eq!(Rule::from_code("nope"), None);
        assert_eq!(
            serde_json::to_string(&Rule::SecretHex).unwrap(),
            "\"secret-hex\""
        );
    }
}
