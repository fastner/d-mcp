//! Importing a Codex entry as a neutral candidate definition: the inverse of
//! the rendering in `docs/reference.md`, "Codex mapping".
//!
//! - `command`, `args` and `cwd` (into `harness.codex`) make a stdio server;
//!   `env_vars` are forwards, and every `env` literal becomes a forward of
//!   the same key name (a [`Replacement`]), when that key is a valid
//!   variable name shown as it is (an `invalid-name` gap otherwise).
//! - `url` makes an http server: `bearer_token_env_var` is `bearer_env`, one
//!   `env_http_headers` entry is `header_env`, one literal `http_headers`
//!   entry becomes `bearer_env` or `header_env` with a proposed variable (a
//!   [`Replacement`]), and otherwise `scopes` and `oauth.client_id` make
//!   OAuth.
//! - Every name the definition would carry (an `env_vars` name, an `env`
//!   key, a header name, and the variable a header or bearer token reads)
//!   passes the one shared name check, [`carried_name`]: a name that is not
//!   shown as it is is an `invalid-name` gap.
//! - The allowed `harness.codex` keys are copied into the override table;
//!   `startup_timeout_ms` becomes `startup_timeout_sec`.
//! - The preserved keys (`oauth.client_secret`, `oauth.callback_url`,
//!   `oauth.callback_port`) never reach the entry value; they are not
//!   literals to replace and stay harness-local (ADR 0005).
//!
//! Unsorted or duplicate `env_vars` are declared as the `env-vars-sorted`
//! normalization, because the rendering lists forwards sorted by name.
//!
//! Any other key, an excluded key, a value of the wrong shape, more than one
//! header, a header next to OAuth settings, and an entry in an inline or
//! dotted style are [`ImportGap`]s.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::{EXCLUDED_OVERRIDE_KEYS, OVERRIDE_KEYS};
use crate::adapter::discovery::{NameKind, carried_name, literal_header};
use crate::adapter::{
    ImportGap, ImportGapReason, Imported, Literal, Normalization, NormalizationKind, Replacement,
    ReplacementKind, ScopeRead,
};
use crate::config::Harness;
use crate::definition::{Auth, Definition, EnvValue, Transport};
use crate::redact::safe_key;

/// The keys the import maps, besides the override keys.
const MAPPED_KEYS: &[&str] = &[
    "command",
    "args",
    "env_vars",
    "env",
    "url",
    "bearer_token_env_var",
    "env_http_headers",
    "http_headers",
    "scopes",
    "oauth",
    "startup_timeout_ms",
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

    /// Whether `name` passes [`carried_name`]; records its gap otherwise.
    fn carried(&mut self, field: &str, name: &str, kind: NameKind) -> bool {
        match carried_name(field, name, kind) {
            Ok(()) => true,
            Err(gap) => {
                self.gaps.push(gap);
                false
            }
        }
    }

    fn string(&mut self, key: &str) -> Option<String> {
        match self.object.get(key)? {
            Value::String(text) => Some(text.clone()),
            _ => {
                self.gap(key, ImportGapReason::InvalidValue);
                None
            }
        }
    }

    fn strings(&mut self, key: &str) -> Vec<String> {
        let Some(value) = self.object.get(key) else {
            return Vec::new();
        };
        match value.as_array().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        }) {
            Some(items) => items,
            None => {
                self.gap(key, ImportGapReason::InvalidValue);
                Vec::new()
            }
        }
    }

    fn table(&mut self, key: &str) -> Option<&Map<String, Value>> {
        let value = self.object.get(key)?;
        if value.as_object().is_none() {
            self.gaps
                .push(ImportGap::new(key, ImportGapReason::InvalidValue));
        }
        value.as_object()
    }

    /// Unknown and excluded keys, and the keys of the other transport.
    fn check_keys(&mut self, stdio: bool) {
        let stdio_only = ["command", "args", "env_vars", "env", "cwd"];
        let http_only = [
            "url",
            "bearer_token_env_var",
            "env_http_headers",
            "http_headers",
            "scopes",
            "oauth",
        ];
        for key in self.object.keys() {
            let key = key.as_str();
            if EXCLUDED_OVERRIDE_KEYS.contains(&key) {
                self.gap(key, ImportGapReason::ExcludedKey);
            } else if !MAPPED_KEYS.contains(&key)
                && !OVERRIDE_KEYS.iter().any(|known| known.key == key)
            {
                self.gap(safe_key(key), ImportGapReason::UnknownKey);
            } else if (stdio && http_only.contains(&key)) || (!stdio && stdio_only.contains(&key)) {
                self.gap(key, ImportGapReason::InvalidValue);
            }
        }
    }

    fn overrides(&mut self) {
        for key in OVERRIDE_KEYS {
            if let Some(value) = self.object.get(key.key) {
                if key.key == "cwd" && !value.is_string() {
                    self.gap("cwd", ImportGapReason::InvalidValue);
                    continue;
                }
                self.overrides.insert(key.key.to_owned(), value.clone());
            }
        }
        if let Some(milliseconds) = self.object.get("startup_timeout_ms") {
            let seconds = milliseconds
                .as_f64()
                .and_then(|ms| serde_json::Number::from_f64(ms / 1000.0));
            match seconds {
                Some(seconds) if !self.object.contains_key("startup_timeout_sec") => {
                    self.overrides
                        .insert("startup_timeout_sec".to_owned(), Value::Number(seconds));
                }
                _ => self.gap("startup_timeout_ms", ImportGapReason::InvalidValue),
            }
        }
    }

    fn stdio(&mut self) -> Option<Transport> {
        let command = self.string("command")?;
        let args = self.strings("args");
        let mut env = BTreeMap::new();
        if let Some(vars) = self.object.get("env_vars") {
            match vars.as_array() {
                Some(vars) => {
                    let names: Vec<Option<&str>> = vars
                        .iter()
                        .map(|var| {
                            var.as_str()
                                .or_else(|| var.get("name").and_then(Value::as_str))
                        })
                        .collect();
                    if !names.is_sorted_by(|a, b| a < b) {
                        self.normalizations.push(Normalization {
                            field: "env_vars".to_owned(),
                            kind: NormalizationKind::EnvVarsSorted,
                        });
                    }
                    for (index, var) in vars.iter().enumerate() {
                        let name = match var {
                            Value::String(name) => Some(name.as_str()),
                            Value::Object(table) => {
                                let local = table
                                    .get("source")
                                    .is_none_or(|source| source.as_str() == Some("local"));
                                if !local {
                                    self.gap(
                                        format!("env_vars[{index}]"),
                                        ImportGapReason::EnvSource,
                                    );
                                }
                                table.get("name").and_then(Value::as_str)
                            }
                            _ => None,
                        };
                        match name {
                            Some(name) => {
                                let field = format!("env_vars[{index}]");
                                if self.carried(&field, name, NameKind::Variable) {
                                    env.insert(name.to_owned(), EnvValue::Forward);
                                }
                            }
                            None => self
                                .gap(format!("env_vars[{index}]"), ImportGapReason::InvalidValue),
                        }
                    }
                }
                None => self.gap("env_vars", ImportGapReason::InvalidValue),
            }
        }
        let literals: Vec<(String, Value)> = self
            .table("env")
            .map(|table| table.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        for (key, value) in literals {
            let field = format!("env.{}", safe_key(&key));
            match value {
                Value::String(literal) if !env.contains_key(&key) => {
                    if let Err(gap) = carried_name(&field, &key, NameKind::Variable) {
                        self.gaps.push(gap);
                        continue;
                    }
                    self.replacements.push(Replacement {
                        field,
                        kind: ReplacementKind::Env,
                        var: key.clone(),
                        literal: Literal::new(literal),
                    });
                    env.insert(key, EnvValue::Forward);
                }
                _ => self.gap(field, ImportGapReason::InvalidValue),
            }
        }
        Some(Transport::Stdio { command, args, env })
    }

    fn http(&mut self) -> Option<Transport> {
        let url = self.string("url")?;
        let mut auths: Vec<(String, Auth)> = Vec::new();
        if let Some(var) = self.string("bearer_token_env_var")
            && self.carried("bearer_token_env_var", &var, NameKind::Variable)
        {
            auths.push((
                "bearer_token_env_var".to_owned(),
                Auth::BearerEnv {
                    var,
                    oauth_supported: false,
                },
            ));
        }
        let env_headers: Vec<(String, Value)> = self
            .table("env_http_headers")
            .map(|table| table.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        for (header, var) in env_headers {
            let field = format!("env_http_headers.{}", safe_key(&header));
            match var {
                Value::String(var) => {
                    if self.carried(&field, &header, NameKind::Header)
                        && self.carried(&field, &var, NameKind::Variable)
                    {
                        auths.push((field, Auth::HeaderEnv { header, var }));
                    }
                }
                _ => self.gap(field, ImportGapReason::InvalidValue),
            }
        }
        let static_headers: Vec<(String, Value)> = self
            .table("http_headers")
            .map(|table| table.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        for (header, value) in static_headers {
            let field = format!("http_headers.{}", safe_key(&header));
            match value {
                Value::String(value) => {
                    match literal_header(self.name, field.clone(), &header, &value) {
                        Ok((auth, replacement)) => {
                            self.replacements.push(replacement);
                            auths.push((field, auth));
                        }
                        Err(gap) => self.gaps.push(gap),
                    }
                }
                _ => self.gap(field, ImportGapReason::InvalidValue),
            }
        }
        let scopes = self.strings("scopes");
        let client_id = match self.table("oauth").cloned() {
            Some(oauth) => {
                for key in oauth.keys().filter(|key| *key != "client_id") {
                    self.gap(
                        format!("oauth.{}", safe_key(key)),
                        ImportGapReason::UnknownKey,
                    );
                }
                match oauth.get("client_id") {
                    None => None,
                    Some(Value::String(id)) => Some(id.clone()),
                    Some(_) => {
                        self.gap("oauth.client_id", ImportGapReason::InvalidValue);
                        None
                    }
                }
            }
            None => None,
        };
        let mut auths = auths.into_iter();
        let auth = match auths.next() {
            Some((field, auth)) => {
                for (extra, _) in auths {
                    self.gap(extra, ImportGapReason::MultipleHeaders);
                }
                if client_id.is_some() || !scopes.is_empty() {
                    self.gap(field, ImportGapReason::HeaderWithOauth);
                }
                auth
            }
            None => {
                let mut sorted = scopes.clone();
                sorted.sort();
                sorted.dedup();
                if sorted != scopes {
                    self.normalizations.push(Normalization {
                        field: "scopes".to_owned(),
                        kind: NormalizationKind::ScopesSorted,
                    });
                }
                Auth::OAuth {
                    client_id,
                    scopes: sorted,
                    client_secret_env: None,
                }
            }
        };
        Some(Transport::Http { url, auth })
    }
}

/// The names of an `env_vars` array, string or table form.
fn env_var_names(vars: Option<&Value>) -> Vec<String> {
    vars.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|var| {
            var.as_str()
                .or_else(|| var.get("name").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .collect()
}

/// The entry as the import's candidate claims it: every replaced env literal
/// moved into `env_vars`, a replaced header as `bearer_token_env_var` or
/// `env_http_headers`, and every declared normalization applied. Pure.
pub(super) fn normalized(raw: &Value, imported: &Imported) -> Value {
    let mut value = raw.clone();
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    let mut forwards = env_var_names(object.get("env_vars"));
    let mut rebuild_forwards = false;
    for replacement in &imported.replacements {
        let var = &replacement.var;
        match replacement.kind {
            ReplacementKind::Env => {
                if let Some(Value::Object(env)) = object.get_mut("env") {
                    env.remove(var);
                }
                forwards.push(var.clone());
                rebuild_forwards = true;
            }
            ReplacementKind::BearerHeader => {
                object.remove("http_headers");
                object.insert(
                    "bearer_token_env_var".to_owned(),
                    Value::String(var.clone()),
                );
            }
            ReplacementKind::Header => {
                // The import refuses more than one header.
                let header = object
                    .remove("http_headers")
                    .and_then(|headers| headers.as_object()?.keys().next().cloned())
                    .unwrap_or_default();
                let mut headers = Map::new();
                headers.insert(header, Value::String(var.clone()));
                object.insert("env_http_headers".to_owned(), Value::Object(headers));
            }
        }
    }
    if object
        .get("env")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty)
    {
        object.remove("env");
    }
    for normalization in &imported.normalizations {
        match normalization.kind {
            NormalizationKind::EnvVarsSorted => rebuild_forwards = true,
            NormalizationKind::ScopesSorted => {
                if let Some(Value::Array(scopes)) = object.get_mut("scopes") {
                    let mut sorted: Vec<String> = scopes
                        .iter()
                        .filter_map(|scope| scope.as_str().map(str::to_owned))
                        .collect();
                    sorted.sort();
                    sorted.dedup();
                    *scopes = sorted.into_iter().map(Value::String).collect();
                }
            }
            NormalizationKind::EmptyDefault | NormalizationKind::DefaultDropped => {}
        }
    }
    if rebuild_forwards {
        forwards.sort();
        forwards.dedup();
        object.insert(
            "env_vars".to_owned(),
            Value::Array(forwards.into_iter().map(Value::String).collect()),
        );
    }
    value
}

/// Imports one Codex entry.
pub(super) fn import(read: &ScopeRead, name: &str) -> Result<Imported, Vec<ImportGap>> {
    if let Some(gaps) = ImportGap::for_scope(read, name) {
        return Err(gaps);
    }
    let entry = &read.entries[name];
    let Some(object) = entry.raw.as_ref().and_then(|raw| raw.value().as_object()) else {
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
    if !entry.supported_style {
        import.gap("", ImportGapReason::UnsupportedStyle);
    }
    let transport = match (object.contains_key("command"), object.contains_key("url")) {
        (true, false) => {
            import.check_keys(true);
            import.stdio()
        }
        (false, true) => {
            import.check_keys(false);
            import.http()
        }
        (true, true) => {
            import.gap("url", ImportGapReason::InvalidValue);
            None
        }
        (false, false) => {
            import.gap("", ImportGapReason::MissingTransport);
            None
        }
    };
    import.overrides();
    match transport {
        Some(transport) if import.gaps.is_empty() => {
            let mut overrides = BTreeMap::new();
            if !import.overrides.is_empty() {
                overrides.insert(Harness::Codex.as_str().to_owned(), import.overrides);
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
    use crate::adapter::{Adapter, DiscoveredScope, Rendered, codex::CodexAdapter};
    use std::path::PathBuf;

    /// A scope read of one parsed Codex document.
    fn read_of(text: &str) -> ScopeRead {
        let parsed = super::super::document::parse(text).unwrap();
        let mut read = ScopeRead::new(Harness::Codex, DiscoveredScope::User, PathBuf::from("/c"));
        read.entries = parsed
            .entries
            .into_iter()
            .map(|(name, entry)| {
                (
                    name,
                    crate::adapter::DiscoveredEntry {
                        supported_style: entry.region.is_some(),
                        hash: Some(entry.hash),
                        raw: Some(entry.value),
                        preserved: entry.preserved,
                        approval: None,
                    },
                )
            })
            .collect();
        read
    }

    fn reasons(result: Result<Imported, Vec<ImportGap>>) -> Vec<(String, ImportGapReason)> {
        result
            .unwrap_err()
            .into_iter()
            .map(|gap| (gap.field, gap.reason))
            .collect()
    }

    fn rendered_hash(imported: &Imported) -> String {
        let Rendered { sent_hash, .. } = CodexAdapter.render(&imported.definition);
        sent_hash
    }

    #[test]
    fn stdio_entries_import_with_literals_as_forwards() {
        let read = read_of(
            "[mcp_servers.gh]\ncommand = \"npx\"\nargs = [\"-y\", \"gh\"]\n\
             env_vars = [\"FWD\", { name = \"TBL\", source = \"local\" }]\n\
             cwd = \"/srv\"\nenabled = false\nstartup_timeout_ms = 5000\n\
             [mcp_servers.gh.env]\nTOKEN = \"CANARY_ENV\"\n",
        );
        let imported = CodexAdapter.import(&read, "gh").unwrap();
        let Transport::Stdio { command, args, env } = &imported.definition.transport else {
            panic!("stdio");
        };
        assert_eq!(command, "npx");
        assert_eq!(args, &["-y", "gh"]);
        assert_eq!(env.len(), 3);
        assert!(env.values().all(|value| *value == EnvValue::Forward));
        assert_eq!(imported.replacements.len(), 1);
        assert_eq!(imported.replacements[0].field, "env.TOKEN");
        assert_eq!(imported.replacements[0].literal.expose(), "CANARY_ENV");
        let codex = &imported.definition.overrides["codex"];
        assert_eq!(codex["cwd"], "/srv");
        assert_eq!(codex["enabled"], false);
        assert_eq!(codex["startup_timeout_sec"], 5.0);
        assert!(!format!("{imported:?}").contains("CANARY"));
    }

    #[test]
    fn a_literal_free_entry_round_trips_through_render() {
        let text = "[mcp_servers.a]\ncommand = \"x\"\nargs = [\"y\"]\nenv_vars = [\"T\"]\n\
                    tool_timeout_sec = 30\n\
                    [mcp_servers.b]\nurl = \"https://x.test/mcp\"\nscopes = [\"r\"]\n\
                    [mcp_servers.b.oauth]\nclient_id = \"cid\"\nclient_secret = \"s\"\n\
                    [mcp_servers.c]\nurl = \"https://x.test/mcp\"\nbearer_token_env_var = \"TOKEN\"\n\
                    [mcp_servers.d]\nurl = \"https://x.test/mcp\"\n\
                    [mcp_servers.d.env_http_headers]\nX-Key = \"KEY\"\n";
        let read = read_of(text);
        for name in ["a", "b", "c", "d"] {
            let imported = CodexAdapter.import(&read, name).unwrap();
            assert!(imported.replacements.is_empty(), "{name}");
            assert_eq!(
                Some(rendered_hash(&imported)),
                read.entries[name].hash,
                "{name}"
            );
        }
    }

    #[test]
    fn literal_headers_become_references() {
        let read = read_of(
            "[mcp_servers.api]\nurl = \"https://x.test\"\n\
             [mcp_servers.api.http_headers]\nAuthorization = \"Bearer CANARY_HEADER\"\n\
             [mcp_servers.key]\nurl = \"https://x.test\"\n\
             [mcp_servers.key.http_headers]\nX-Api-Key = \"CANARY_HEADER\"\n",
        );
        let api = CodexAdapter.import(&read, "api").unwrap();
        assert_eq!(api.definition.auth_kind(), Some("bearer_env"));
        assert_eq!(api.replacements[0].var, "API_TOKEN");
        assert_eq!(api.replacements[0].kind, ReplacementKind::BearerHeader);
        assert_eq!(api.replacements[0].literal.expose(), "CANARY_HEADER");
        let key = CodexAdapter.import(&read, "key").unwrap();
        assert_eq!(key.definition.auth_kind(), Some("header_env"));
        assert_eq!(key.replacements[0].var, "KEY_X_API_KEY");
    }

    #[test]
    fn inexpressible_entries_are_gaps() {
        let read = read_of(
            "[mcp_servers.multi]\nurl = \"https://x.test\"\nbearer_token_env_var = \"T\"\n\
             [mcp_servers.multi.env_http_headers]\nX-A = \"A\"\n\
             [mcp_servers.mixed]\nurl = \"https://x.test\"\nscopes = [\"r\"]\nbearer_token_env_var = \"T\"\n\
             [mcp_servers.helper]\nurl = \"https://x.test\"\nhttp_headers_helper = \"/bin/x\"\n\
             [mcp_servers.odd]\ncommand = \"x\"\nghp_abcdefghijklmnopqrstuvwxyz0123456789 = 1\n\
             [mcp_servers.both]\ncommand = \"x\"\nurl = \"https://x.test\"\n\
             [mcp_servers.none]\nenabled = true\n\
             [mcp_servers.wrong]\ncommand = 1\nargs = \"x\"\nscopes = [\"r\"]\n\
             [mcp_servers.remote]\ncommand = \"x\"\nenv_vars = [{ name = \"R\", source = \"remote\" }, 3]\n\
             [mcp_servers.inline]\ncommand = \"x\"\nenv = { A = 1 }\n\
             [mcp_servers.sub]\ncommand = \"x\"\n[mcp_servers.sub.oauth]\nclient_id = \"c\"\n\
             [mcp_servers]\ndotted.command = \"x\"\n",
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "multi")),
            [(
                "env_http_headers.X-A".to_owned(),
                ImportGapReason::MultipleHeaders
            )]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "mixed")),
            [(
                "bearer_token_env_var".to_owned(),
                ImportGapReason::HeaderWithOauth
            )]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "helper")),
            [(
                "http_headers_helper".to_owned(),
                ImportGapReason::ExcludedKey
            )]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "odd")),
            [("<redacted-key>".to_owned(), ImportGapReason::UnknownKey)]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "both")),
            [("url".to_owned(), ImportGapReason::InvalidValue)]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "none")),
            [(String::new(), ImportGapReason::MissingTransport)]
        );
        let wrong = reasons(CodexAdapter.import(&read, "wrong"));
        assert!(wrong.contains(&("command".to_owned(), ImportGapReason::InvalidValue)));
        assert!(wrong.contains(&("scopes".to_owned(), ImportGapReason::InvalidValue)));
        let remote = reasons(CodexAdapter.import(&read, "remote"));
        assert!(remote.contains(&("env_vars[0]".to_owned(), ImportGapReason::EnvSource)));
        assert!(remote.contains(&("env_vars[1]".to_owned(), ImportGapReason::InvalidValue)));
        // An inline value inside a standard table is a supported style.
        assert_eq!(
            reasons(CodexAdapter.import(&read, "inline")),
            [("env.A".to_owned(), ImportGapReason::InvalidValue)]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "sub")),
            [("oauth".to_owned(), ImportGapReason::InvalidValue)]
        );
        assert_eq!(
            reasons(CodexAdapter.import(&read, "dotted")),
            [(String::new(), ImportGapReason::UnsupportedStyle)]
        );
    }

    #[test]
    fn normalized_entries_hash_like_their_rendering() {
        let read = read_of(
            "[mcp_servers.env]\ncommand = \"x\"\nenv_vars = [\"B\", \"A\", \"A\"]\n\
             [mcp_servers.env.env]\nT = \"lit\"\n\
             [mcp_servers.bearer]\nurl = \"https://x.test/mcp\"\n\
             [mcp_servers.bearer.http_headers]\nAuthorization = \"Bearer lit\"\n\
             [mcp_servers.header]\nurl = \"https://x.test/mcp\"\n\
             [mcp_servers.header.http_headers]\nX-Key = \"lit\"\n\
             [mcp_servers.scopes]\nurl = \"https://x.test/mcp\"\nscopes = [\"b\", \"a\", \"a\"]\n",
        );
        for name in ["env", "bearer", "header", "scopes"] {
            let imported = CodexAdapter.import(&read, name).unwrap();
            let raw = read.entries[name].raw.as_ref().unwrap();
            let normalized = CodexAdapter.normalized_entry(raw, &imported);
            assert_eq!(
                CodexAdapter.entry_hash(&normalized),
                rendered_hash(&imported),
                "{name}"
            );
            // Each needs a rewrite: the entry itself differs.
            assert_ne!(
                read.entries[name].hash,
                Some(rendered_hash(&imported)),
                "{name}"
            );
        }
        let env = CodexAdapter.import(&read, "env").unwrap();
        assert_eq!(env.normalizations[0].kind, NormalizationKind::EnvVarsSorted);
        // A non-object value is left as it is.
        assert_eq!(normalized(&Value::Null, &env), Value::Null);
    }

    #[test]
    fn unsorted_scopes_are_a_declared_normalization() {
        let read = read_of("[mcp_servers.s]\nurl = \"https://x.test\"\nscopes = [\"b\", \"a\"]\n");
        let imported = CodexAdapter.import(&read, "s").unwrap();
        assert_eq!(
            imported.normalizations,
            [Normalization {
                field: "scopes".to_owned(),
                kind: NormalizationKind::ScopesSorted
            }]
        );
    }
}
