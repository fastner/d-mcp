//! Importing a Claude Code entry as a neutral candidate definition: the
//! inverse of the rendering in `docs/reference.md`, "Claude Code mapping".
//!
//! - `command` and `args` (`type` absent or `stdio`) make a stdio server. An
//!   `env` value `${NAME}` or `${NAME:-}` under the key `NAME` is a forward
//!   (`${NAME}` is declared as rendered `${NAME:-}`); any other env value
//!   without variable syntax is a literal and becomes a forward of the same
//!   key name (a [`Replacement`]), when that key is a valid variable name
//!   shown as it is (an `invalid-name` gap otherwise).
//! - Every name the definition would carry (an env key, forwarded or
//!   replaced, a header name, and the variable a header reads) passes the
//!   one shared name check, [`carried_name`]: a name that is not shown as it
//!   is is an `invalid-name` gap.
//! - `type = "http"` and `url` make an http server. One header
//!   `Authorization: Bearer ${VAR}` is `bearer_env`, one other header
//!   `${VAR}` is `header_env`; a literal header becomes either with a
//!   proposed variable (a [`Replacement`]). Otherwise `oauth.clientId`,
//!   `oauth.scopes` (space-separated) and `oauth.callbackPort` make OAuth.
//! - `timeout` and `oauth.callbackPort` go into `harness.claude`.
//!
//! A renamed forward, a non-empty default, variable syntax inside a longer
//! value (also in `command`, `args` and `url`), `type = "sse"`, more than one
//! header, a header next to `oauth`, `headersHelper` and any other key are
//! [`ImportGap`]s.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::adapter::discovery::{NameKind, carried_name, literal_header};
use crate::adapter::{
    ImportGap, ImportGapReason, Imported, Literal, Normalization, NormalizationKind, Replacement,
    ReplacementKind, ScopeRead,
};
use crate::config::Harness;
use crate::credential::contains_variable_syntax;
use crate::definition::{Auth, Definition, EnvValue, Transport, is_valid_variable_name};
use crate::redact::safe_key;

/// What a Claude Code string value is, by its variable syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reference<'a> {
    /// No variable syntax.
    Literal,
    /// Exactly `${NAME}` (`empty_default = false`) or `${NAME:-}`.
    Var { name: &'a str, empty_default: bool },
    /// Exactly `${NAME:-default}` with a non-empty default.
    Default,
    /// Variable syntax inside a longer value, or an invalid name.
    Embedded,
}

fn reference(value: &str) -> Reference<'_> {
    if let Some(inner) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
        && !inner.contains(['{', '}', '$'])
    {
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        if !is_valid_variable_name(name) {
            return Reference::Embedded;
        }
        return match default {
            None => Reference::Var {
                name,
                empty_default: false,
            },
            Some("") => Reference::Var {
                name,
                empty_default: true,
            },
            Some(_) => Reference::Default,
        };
    }
    if contains_variable_syntax(value) {
        Reference::Embedded
    } else {
        Reference::Literal
    }
}

/// The keys of a Claude Code entry the import maps.
const MAPPED_KEYS: &[&str] = &[
    "type", "command", "args", "env", "url", "headers", "oauth", "timeout",
];

struct Import<'a> {
    name: &'a str,
    object: &'a Map<String, Value>,
    gaps: Vec<ImportGap>,
    replacements: Vec<Replacement>,
    normalizations: Vec<Normalization>,
    overrides: Map<String, Value>,
}

impl Import<'_> {
    fn gap(&mut self, field: impl Into<String>, reason: ImportGapReason) {
        self.gaps.push(ImportGap::new(field, reason));
    }

    fn normalize(&mut self, field: impl Into<String>, kind: NormalizationKind) {
        self.normalizations.push(Normalization {
            field: field.into(),
            kind,
        });
    }

    /// A string that must not hold variable syntax.
    fn plain(&mut self, field: String, value: Option<&Value>) -> Option<String> {
        match value {
            Some(Value::String(text)) if !contains_variable_syntax(text) => Some(text.clone()),
            Some(Value::String(_)) => {
                self.gap(field, ImportGapReason::EmbeddedVariable);
                None
            }
            _ => {
                self.gap(field, ImportGapReason::InvalidValue);
                None
            }
        }
    }

    fn check_keys(&mut self, stdio: bool) {
        for key in self.object.keys() {
            let key = key.as_str();
            let other_transport = if stdio {
                matches!(key, "url" | "headers" | "oauth")
            } else {
                matches!(key, "command" | "args" | "env")
            };
            if key == "headersHelper" || key == "headers_helper" {
                self.gap(key, ImportGapReason::ExcludedKey);
            } else if !MAPPED_KEYS.contains(&key) {
                self.gap(safe_key(key), ImportGapReason::UnknownKey);
            } else if other_transport {
                self.gap(key, ImportGapReason::InvalidValue);
            }
        }
        if let Some(timeout) = self.object.get("timeout") {
            self.overrides.insert("timeout".to_owned(), timeout.clone());
        }
    }

    fn stdio(&mut self) -> Option<Transport> {
        let command = self.plain("command".to_owned(), self.object.get("command"));
        let mut args = Vec::new();
        match self.object.get("args") {
            None => {}
            Some(Value::Array(items)) => {
                for (index, item) in items.iter().enumerate() {
                    args.extend(self.plain(format!("args[{index}]"), Some(item)));
                }
            }
            Some(_) => self.gap("args", ImportGapReason::InvalidValue),
        }
        let mut env = BTreeMap::new();
        match self.object.get("env") {
            None => {}
            Some(Value::Object(items)) => {
                for (key, value) in items {
                    let field = format!("env.{}", safe_key(key));
                    let Some(text) = value.as_str() else {
                        self.gap(field, ImportGapReason::InvalidValue);
                        continue;
                    };
                    match reference(text) {
                        Reference::Var {
                            name,
                            empty_default,
                        } if name == key => {
                            if let Err(gap) = carried_name(&field, key, NameKind::Variable) {
                                self.gaps.push(gap);
                                continue;
                            }
                            if !empty_default {
                                self.normalize(field, NormalizationKind::EmptyDefault);
                            }
                            env.insert(key.clone(), EnvValue::Forward);
                        }
                        Reference::Var { .. } => self.gap(field, ImportGapReason::RenamedForward),
                        Reference::Default => self.gap(field, ImportGapReason::ForwardDefault),
                        Reference::Embedded => {
                            self.gap(field, ImportGapReason::EmbeddedVariable);
                        }
                        Reference::Literal => {
                            if let Err(gap) = carried_name(&field, key, NameKind::Variable) {
                                self.gaps.push(gap);
                                continue;
                            }
                            self.replacements.push(Replacement {
                                field,
                                kind: ReplacementKind::Env,
                                var: key.clone(),
                                literal: Literal::new(text),
                            });
                            env.insert(key.clone(), EnvValue::Forward);
                        }
                    }
                }
            }
            Some(_) => self.gap("env", ImportGapReason::InvalidValue),
        }
        Some(Transport::Stdio {
            command: command?,
            args,
            env,
        })
    }

    /// The auth of the single header.
    fn header(&mut self, header: &str, value: &Value) -> Option<Auth> {
        let field = format!("headers.{}", safe_key(header));
        let Some(text) = value.as_str() else {
            self.gap(field, ImportGapReason::InvalidValue);
            return None;
        };
        let bearer = header
            .eq_ignore_ascii_case("authorization")
            .then(|| text.strip_prefix("Bearer "))
            .flatten();
        let referenced = reference(bearer.unwrap_or(text));
        match referenced {
            Reference::Var {
                name,
                empty_default,
            } => {
                let checked = carried_name(&field, header, NameKind::Header)
                    .and_then(|()| carried_name(&field, name, NameKind::Variable));
                if let Err(gap) = checked {
                    self.gaps.push(gap);
                    return None;
                }
                if empty_default {
                    self.normalize(field, NormalizationKind::DefaultDropped);
                }
                Some(match bearer {
                    Some(_) => Auth::BearerEnv {
                        var: name.to_owned(),
                        oauth_supported: false,
                    },
                    None => Auth::HeaderEnv {
                        header: header.to_owned(),
                        var: name.to_owned(),
                    },
                })
            }
            Reference::Literal => match literal_header(self.name, field, header, text) {
                Ok((auth, replacement)) => {
                    self.replacements.push(replacement);
                    Some(auth)
                }
                Err(gap) => {
                    self.gaps.push(gap);
                    None
                }
            },
            Reference::Default => {
                self.gap(field, ImportGapReason::ForwardDefault);
                None
            }
            Reference::Embedded => {
                self.gap(field, ImportGapReason::EmbeddedVariable);
                None
            }
        }
    }

    fn oauth(&mut self, oauth: &Map<String, Value>) -> Auth {
        let mut client_id = None;
        let mut scopes = Vec::new();
        for (key, value) in oauth {
            match (key.as_str(), value) {
                ("clientId", _) => client_id = self.plain("oauth.clientId".to_owned(), Some(value)),
                ("scopes", Value::String(text)) => {
                    scopes = text.split_whitespace().map(str::to_owned).collect();
                    let mut sorted = scopes.clone();
                    sorted.sort();
                    sorted.dedup();
                    if sorted.join(" ") != *text {
                        self.normalize("oauth.scopes", NormalizationKind::ScopesSorted);
                    }
                    scopes = sorted;
                }
                ("callbackPort", _) => {
                    self.overrides
                        .insert("oauth_callback_port".to_owned(), value.clone());
                }
                ("scopes", _) => self.gap("oauth.scopes", ImportGapReason::InvalidValue),
                _ => self.gap(
                    format!("oauth.{}", safe_key(key)),
                    ImportGapReason::UnknownKey,
                ),
            }
        }
        Auth::OAuth {
            client_id,
            scopes,
            client_secret_env: None,
        }
    }

    fn http(&mut self) -> Option<Transport> {
        let url = self.plain("url".to_owned(), self.object.get("url"));
        let oauth = match self.object.get("oauth") {
            None => None,
            Some(Value::Object(oauth)) => Some(oauth),
            Some(_) => {
                self.gap("oauth", ImportGapReason::InvalidValue);
                None
            }
        };
        let headers: Vec<(String, Value)> = match self.object.get("headers") {
            None => Vec::new(),
            Some(Value::Object(headers)) => headers
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            Some(_) => {
                self.gap("headers", ImportGapReason::InvalidValue);
                Vec::new()
            }
        };
        let auth = match headers.split_first() {
            Some(((header, value), rest)) => {
                for (extra, _) in rest {
                    self.gap(
                        format!("headers.{}", safe_key(extra)),
                        ImportGapReason::MultipleHeaders,
                    );
                }
                if oauth.is_some() {
                    self.gap("oauth", ImportGapReason::HeaderWithOauth);
                }
                self.header(header, value)
            }
            None => Some(match oauth {
                Some(oauth) => self.oauth(oauth),
                None => Auth::OAuth {
                    client_id: None,
                    scopes: Vec::new(),
                    client_secret_env: None,
                },
            }),
        };
        Some(Transport::Http {
            url: url?,
            auth: auth?,
        })
    }
}

/// The entry as the import's candidate claims it: every replaced literal as
/// its reference (`${KEY:-}` for an env literal, `Bearer ${VAR}` or `${VAR}`
/// for the header) and every declared normalization applied. Pure.
pub(super) fn normalized(raw: &Value, imported: &Imported) -> Value {
    let mut value = raw.clone();
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    for replacement in &imported.replacements {
        let var = &replacement.var;
        match replacement.kind {
            ReplacementKind::Env => {
                if let Some(Value::Object(env)) = object.get_mut("env") {
                    env.insert(var.clone(), Value::String(format!("${{{var}:-}}")));
                }
            }
            ReplacementKind::BearerHeader | ReplacementKind::Header => {
                let reference = if replacement.kind == ReplacementKind::BearerHeader {
                    format!("Bearer ${{{var}}}")
                } else {
                    format!("${{{var}}}")
                };
                // The import refuses more than one header.
                if let Some(header) = object
                    .get_mut("headers")
                    .and_then(Value::as_object_mut)
                    .and_then(|headers| headers.values_mut().next())
                {
                    *header = Value::String(reference);
                }
            }
        }
    }
    for normalization in &imported.normalizations {
        match normalization.kind {
            NormalizationKind::EmptyDefault => {
                if let Some(Value::Object(env)) = object.get_mut("env") {
                    for (key, item) in env.iter_mut() {
                        if item.as_str() == Some(format!("${{{key}}}").as_str()) {
                            *item = Value::String(format!("${{{key}:-}}"));
                        }
                    }
                }
            }
            NormalizationKind::DefaultDropped => {
                if let Some(Value::Object(headers)) = object.get_mut("headers") {
                    for item in headers.values_mut() {
                        if let Some(text) = item.as_str() {
                            *item = Value::String(text.replace(":-}", "}"));
                        }
                    }
                }
            }
            NormalizationKind::ScopesSorted => {
                if let Some(scopes) = object
                    .get_mut("oauth")
                    .and_then(|oauth| oauth.get_mut("scopes"))
                    && let Some(text) = scopes.as_str()
                {
                    let mut sorted: Vec<&str> = text.split_whitespace().collect();
                    sorted.sort_unstable();
                    sorted.dedup();
                    *scopes = Value::String(sorted.join(" "));
                }
            }
            NormalizationKind::EnvVarsSorted => {}
        }
    }
    value
}

/// Imports one Claude Code entry.
pub(super) fn import(read: &ScopeRead, name: &str) -> Result<Imported, Vec<ImportGap>> {
    if let Some(gaps) = ImportGap::for_scope(read, name) {
        return Err(gaps);
    }
    let Some(object) = read.entries[name]
        .raw
        .as_ref()
        .and_then(|raw| raw.value().as_object())
    else {
        return Err(vec![ImportGap::new("", ImportGapReason::NotAnObject)]);
    };
    let mut import = Import {
        name,
        object,
        gaps: Vec::new(),
        replacements: Vec::new(),
        normalizations: Vec::new(),
        overrides: Map::new(),
    };
    let transport = match object.get("type") {
        None if object.contains_key("command") => {
            import.check_keys(true);
            import.stdio()
        }
        Some(Value::String(kind)) if kind == "stdio" => {
            import.check_keys(true);
            import.stdio()
        }
        Some(Value::String(kind)) if kind == "http" => {
            import.check_keys(false);
            import.http()
        }
        Some(Value::String(kind)) if kind == "sse" => {
            import.gap("type", ImportGapReason::SseTransport);
            None
        }
        None => {
            import.gap("", ImportGapReason::MissingTransport);
            None
        }
        Some(_) => {
            import.gap("type", ImportGapReason::InvalidValue);
            None
        }
    };
    match transport {
        Some(transport) if import.gaps.is_empty() => {
            let mut overrides = BTreeMap::new();
            if !import.overrides.is_empty() {
                overrides.insert(Harness::Claude.as_str().to_owned(), import.overrides);
            }
            Ok(Imported {
                definition: Definition {
                    name: name.to_owned(),
                    description: None,
                    transport,
                    overrides,
                },
                replacements: import.replacements,
                normalizations: import.normalizations,
            })
        }
        _ => Err(import.gaps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::claude::{ClaudeAdapter, entry_hash};
    use crate::adapter::{Adapter, DiscoveredEntry, DiscoveredScope, RawEntry};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn read_of(entries: Value) -> ScopeRead {
        let mut read = ScopeRead::new(Harness::Claude, DiscoveredScope::User, PathBuf::from("/c"));
        read.entries = entries
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    DiscoveredEntry {
                        hash: Some(entry_hash(value)),
                        supported_style: value.is_object(),
                        raw: Some(RawEntry::new(value.clone())),
                        preserved: BTreeSet::new(),
                        approval: None,
                    },
                )
            })
            .collect();
        read
    }

    fn reasons(read: &ScopeRead, name: &str) -> Vec<(String, ImportGapReason)> {
        ClaudeAdapter
            .import(read, name)
            .unwrap_err()
            .into_iter()
            .map(|gap| (gap.field, gap.reason))
            .collect()
    }

    #[test]
    fn references_are_classified_by_their_whole_value() {
        assert_eq!(reference("plain"), Reference::Literal);
        assert_eq!(
            reference("${A}"),
            Reference::Var {
                name: "A",
                empty_default: false
            }
        );
        assert_eq!(
            reference("${A:-}"),
            Reference::Var {
                name: "A",
                empty_default: true
            }
        );
        assert_eq!(reference("${A:-x}"), Reference::Default);
        for embedded in ["x${A}", "${A}${B}", "${1A}", "$A", "${A:-${B}}"] {
            assert_eq!(reference(embedded), Reference::Embedded, "{embedded}");
        }
    }

    #[test]
    fn rendered_entries_import_back_to_the_same_hash() {
        let read = read_of(json!({
            "stdio": {"command": "npx", "args": ["-y", "x"], "env": {"T": "${T:-}"}, "timeout": 30000},
            "bearer": {"type": "http", "url": "https://x.test/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"}},
            "header": {"type": "http", "url": "https://x.test/mcp", "headers": {"X-Key": "${KEY}"}},
            "oauth": {"type": "http", "url": "https://x.test/mcp",
                      "oauth": {"clientId": "cid", "scopes": "a b", "callbackPort": 3118}},
            "discovery": {"type": "http", "url": "https://x.test/mcp"},
        }));
        for name in ["stdio", "bearer", "header", "oauth", "discovery"] {
            let imported = ClaudeAdapter.import(&read, name).unwrap();
            assert!(imported.replacements.is_empty(), "{name}");
            assert!(imported.normalizations.is_empty(), "{name}");
            let rendered = ClaudeAdapter.render(&imported.definition);
            assert_eq!(Some(rendered.sent_hash), read.entries[name].hash, "{name}");
        }
    }

    #[test]
    fn literals_become_references_and_forms_are_declared() {
        let read = read_of(json!({
            "env": {"command": "x", "env": {"TOKEN": "CANARY_ENV", "FWD": "${FWD}"}},
            "bearer": {"type": "http", "url": "https://x.test", "headers": {"Authorization": "Bearer CANARY"}},
            "header": {"type": "http", "url": "https://x.test", "headers": {"X-Key": "${KEY:-}"}},
            "scopes": {"type": "http", "url": "https://x.test", "oauth": {"scopes": "b a a"}},
        }));
        let env = ClaudeAdapter.import(&read, "env").unwrap();
        assert_eq!(env.replacements.len(), 1);
        assert_eq!(env.replacements[0].var, "TOKEN");
        assert_eq!(env.replacements[0].literal.expose(), "CANARY_ENV");
        assert_eq!(
            env.normalizations,
            [Normalization {
                field: "env.FWD".to_owned(),
                kind: NormalizationKind::EmptyDefault
            }]
        );
        let bearer = ClaudeAdapter.import(&read, "bearer").unwrap();
        assert_eq!(bearer.definition.auth_kind(), Some("bearer_env"));
        assert_eq!(bearer.replacements[0].var, "BEARER_TOKEN");
        let header = ClaudeAdapter.import(&read, "header").unwrap();
        assert_eq!(
            header.normalizations[0].kind,
            NormalizationKind::DefaultDropped
        );
        let scopes = ClaudeAdapter.import(&read, "scopes").unwrap();
        assert_eq!(
            scopes.normalizations[0].kind,
            NormalizationKind::ScopesSorted
        );
        let Transport::Http {
            auth: Auth::OAuth { scopes, .. },
            ..
        } = &scopes.definition.transport
        else {
            panic!("oauth");
        };
        assert_eq!(scopes, &["a", "b"]);
    }

    #[test]
    fn normalized_entries_hash_like_their_rendering() {
        let read = read_of(json!({
            "env": {"command": "x", "env": {"T": "lit", "F": "${F}"}},
            "bearer": {"type": "http", "url": "https://x.test", "headers": {"Authorization": "Bearer lit"}},
            "header": {"type": "http", "url": "https://x.test", "headers": {"X-Key": "lit"}},
            "default": {"type": "http", "url": "https://x.test", "headers": {"Authorization": "Bearer ${T:-}"}},
            "scopes": {"type": "http", "url": "https://x.test", "oauth": {"scopes": "b a a"}},
            "lower": {"type": "http", "url": "https://x.test", "headers": {"authorization": "Bearer lit"}},
        }));
        let round_trips = |name: &str| {
            let imported = ClaudeAdapter.import(&read, name).unwrap();
            let raw = read.entries[name].raw.as_ref().unwrap();
            let normalized = ClaudeAdapter.normalized_entry(raw, &imported);
            ClaudeAdapter.entry_hash(&normalized)
                == ClaudeAdapter.render(&imported.definition).sent_hash
        };
        for name in ["env", "bearer", "header", "default", "scopes"] {
            assert!(round_trips(name), "{name}");
        }
        // The rendering spells the header `Authorization`: an undeclared
        // change, so the import is not lossless.
        assert!(!round_trips("lower"));
        let env = ClaudeAdapter.import(&read, "env").unwrap();
        assert_eq!(normalized(&Value::Null, &env), Value::Null);
    }

    #[test]
    fn inexpressible_entries_are_gaps() {
        let read = read_of(json!({
            "sse": {"type": "sse", "url": "https://x.test"},
            "renamed": {"command": "x", "env": {"A": "${B}", "C": "${C:-d}", "E": "x${E}", "F": 1}},
            "multi": {"type": "http", "url": "https://x.test", "headers": {"A": "${A}", "B": "${B}"}},
            "mixed": {"type": "http", "url": "https://x.test", "headers": {"A": "${A}"}, "oauth": {"clientId": "c"}},
            "helper": {"type": "http", "url": "https://x.test", "headersHelper": "/bin/x"},
            "unknown": {"command": "x", "ghp_abcdefghijklmnopqrstuvwxyz0123456789": 1, "url": "u"},
            "vars": {"command": "${HOME}/x", "args": ["${A}", 1]},
            "url": {"type": "http", "url": "https://${HOST}/mcp", "oauth": {"other": 1, "scopes": 1}},
            "none": {"timeout": 1},
            "weird": {"type": 1},
            "text": "CANARY",
        }));
        assert_eq!(
            reasons(&read, "sse"),
            [("type".to_owned(), ImportGapReason::SseTransport)]
        );
        assert_eq!(
            reasons(&read, "renamed"),
            [
                ("env.A".to_owned(), ImportGapReason::RenamedForward),
                ("env.C".to_owned(), ImportGapReason::ForwardDefault),
                ("env.E".to_owned(), ImportGapReason::EmbeddedVariable),
                ("env.F".to_owned(), ImportGapReason::InvalidValue),
            ]
        );
        assert_eq!(
            reasons(&read, "multi"),
            [("headers.B".to_owned(), ImportGapReason::MultipleHeaders)]
        );
        assert_eq!(
            reasons(&read, "mixed"),
            [("oauth".to_owned(), ImportGapReason::HeaderWithOauth)]
        );
        assert_eq!(
            reasons(&read, "helper"),
            [("headersHelper".to_owned(), ImportGapReason::ExcludedKey)]
        );
        assert_eq!(
            reasons(&read, "unknown"),
            [
                ("<redacted-key>".to_owned(), ImportGapReason::UnknownKey),
                ("url".to_owned(), ImportGapReason::InvalidValue),
            ]
        );
        assert_eq!(
            reasons(&read, "vars"),
            [
                ("command".to_owned(), ImportGapReason::EmbeddedVariable),
                ("args[0]".to_owned(), ImportGapReason::EmbeddedVariable),
                ("args[1]".to_owned(), ImportGapReason::InvalidValue),
            ]
        );
        assert_eq!(
            reasons(&read, "url"),
            [
                ("url".to_owned(), ImportGapReason::EmbeddedVariable),
                ("oauth.other".to_owned(), ImportGapReason::UnknownKey),
                ("oauth.scopes".to_owned(), ImportGapReason::InvalidValue),
            ]
        );
        assert_eq!(
            reasons(&read, "none"),
            [(String::new(), ImportGapReason::MissingTransport)]
        );
        assert_eq!(
            reasons(&read, "weird"),
            [("type".to_owned(), ImportGapReason::InvalidValue)]
        );
        assert_eq!(
            reasons(&read, "text"),
            [(String::new(), ImportGapReason::NotAnObject)]
        );
    }
}
