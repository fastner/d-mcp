//! Phase two of definition parsing: d-mcp's own walker over the spanned tree.
//!
//! The walker visits fields in a fixed order and stops at the first broken
//! rule, so every file maps to exactly one rule code. Every error carries the
//! field path and the source position, never the value.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};

use super::{
    Auth, Definition, DefinitionError, EnvValue, OverrideKind, OverrideSite, OverrideSites,
    Transport, is_valid_header_name, is_valid_name, is_valid_variable_name,
};
use crate::credential::{
    SecretFinding, contains_userinfo, contains_variable_syntax, parse_url_shape,
    secret_name_heuristic, secret_value_heuristic,
};
use crate::rule::Rule;
use crate::spanned::{self, Entry, Node, NodeKind};

type Result<T> = std::result::Result<T, DefinitionError>;

const ROOT_KEYS: &[&str] = &["name", "description", "transport", "env", "auth", "harness"];
const STDIO_KEYS: &[&str] = &["type", "command", "args"];
const HTTP_KEYS: &[&str] = &["type", "url"];
const ENV_ENTRY_KEYS: &[&str] = &["forward", "value"];
const OAUTH_KEYS: &[&str] = &["type", "client_id", "scopes", "client_secret_env"];
const BEARER_KEYS: &[&str] = &["type", "var", "oauth_supported"];
const HEADER_KEYS: &[&str] = &["type", "header", "var"];
const NONE_KEYS: &[&str] = &["type"];
const HARNESS_KEYS: &[&str] = &["codex", "claude"];
/// Override tables whose string values name environment variables rather
/// than hold literals, so their keys skip the name heuristic when the value
/// is a variable name.
const ENV_REFERENCE_TABLES: &[&str] = &["env_http_headers"];
/// Override tables whose string values are literals; keys inside them, at any
/// depth and through arrays, are never exempt from the name heuristic.
const LITERAL_ENV_TABLE: &str = "env";

pub(super) fn walk(text: &str, stem: &str) -> Result<(Definition, OverrideSites)> {
    Walker { text }.definition(stem)
}

struct Walker<'t> {
    text: &'t str,
}

/// Joins a field path and a key. Keys that look like secrets or contain
/// unusual characters are redacted, so the path itself cannot leak a value.
fn join(path: &str, key: &str) -> String {
    let safe = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        && secret_value_heuristic(key).is_none();
    let key = if safe { key } else { "<redacted>" };
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn index(path: &str, position: usize) -> String {
    format!("{path}[{position}]")
}

fn find<'e>(entries: &'e [Entry], key: &str) -> Option<&'e Entry> {
    entries.iter().find(|entry| entry.key == key)
}

fn secret_rule(finding: SecretFinding) -> Rule {
    match finding {
        SecretFinding::Prefix => Rule::SecretPrefix,
        SecretFinding::Jwt => Rule::SecretJwt,
        SecretFinding::Hex => Rule::SecretHex,
        SecretFinding::Entropy => Rule::SecretEntropy,
        SecretFinding::Name => Rule::SecretName,
        SecretFinding::Userinfo => Rule::UrlUserinfo,
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|ip| ip == Ipv6Addr::LOCALHOST)
}

impl Walker<'_> {
    fn error(&self, rule: Rule, field: &str, offset: Option<usize>) -> DefinitionError {
        let (line, column) = spanned::line_column(self.text, offset.unwrap_or(0));
        DefinitionError {
            rule,
            field: field.to_owned(),
            line,
            column,
        }
    }

    fn check_keys(&self, entries: &[Entry], path: &str, allowed: &[&str]) -> Result<()> {
        match entries
            .iter()
            .find(|entry| !allowed.contains(&entry.key.as_str()))
        {
            Some(entry) => Err(self.error(
                Rule::UnknownKey,
                &join(path, &entry.key),
                entry.key_offset(),
            )),
            None => Ok(()),
        }
    }

    fn required<'e>(
        &self,
        entries: &'e [Entry],
        path: &str,
        key: &str,
        parent: Option<usize>,
    ) -> Result<&'e Entry> {
        find(entries, key).ok_or_else(|| self.error(Rule::MissingField, &join(path, key), parent))
    }

    fn string<'n>(&self, node: &'n Node, field: &str) -> Result<&'n str> {
        match &node.kind {
            NodeKind::String(value) => Ok(value),
            _ => Err(self.error(Rule::WrongType, field, node.offset())),
        }
    }

    fn table<'n>(&self, node: &'n Node, field: &str) -> Result<&'n [Entry]> {
        match &node.kind {
            NodeKind::Table(entries) => Ok(entries),
            _ => Err(self.error(Rule::WrongType, field, node.offset())),
        }
    }

    fn array<'n>(&self, node: &'n Node, field: &str) -> Result<&'n [Node]> {
        match &node.kind {
            NodeKind::Array(items) => Ok(items),
            _ => Err(self.error(Rule::WrongType, field, node.offset())),
        }
    }

    fn boolean(&self, node: &Node, field: &str) -> Result<bool> {
        match node.kind {
            NodeKind::Boolean(value) => Ok(value),
            _ => Err(self.error(Rule::WrongType, field, node.offset())),
        }
    }

    fn string_array(&self, node: &Node, field: &str) -> Result<Vec<(String, Option<usize>)>> {
        self.array(node, field)?
            .iter()
            .enumerate()
            .map(|(position, item)| {
                let value = self.string(item, &index(field, position))?;
                Ok((value.to_owned(), item.offset()))
            })
            .collect()
    }

    fn no_variables(&self, value: &str, field: &str, offset: Option<usize>) -> Result<()> {
        if contains_variable_syntax(value) {
            Err(self.error(Rule::VariableSyntax, field, offset))
        } else {
            Ok(())
        }
    }

    fn no_secret(&self, value: &str, field: &str, offset: Option<usize>) -> Result<()> {
        match secret_value_heuristic(value) {
            Some(finding) => Err(self.error(secret_rule(finding), field, offset)),
            None => Ok(()),
        }
    }

    fn variable_name(&self, value: &str, field: &str, offset: Option<usize>) -> Result<()> {
        if is_valid_variable_name(value) {
            Ok(())
        } else {
            Err(self.error(Rule::VariableName, field, offset))
        }
    }

    fn definition(&self, stem: &str) -> Result<(Definition, OverrideSites)> {
        let root = spanned::parse_document(self.text)
            .map_err(|offset| self.error(Rule::TomlSyntax, "", offset))?;
        self.check_keys(&root, "", ROOT_KEYS)?;

        let name_entry = self.required(&root, "", "name", None)?;
        let name = self.string(&name_entry.value, "name")?;
        if !is_valid_name(name) {
            return Err(self.error(Rule::NameRule, "name", name_entry.value_offset()));
        }
        if name != stem {
            return Err(self.error(Rule::NameMismatch, "name", name_entry.value_offset()));
        }

        let description = match find(&root, "description") {
            Some(entry) => {
                let value = self.string(&entry.value, "description")?;
                self.no_secret(value, "description", entry.value_offset())?;
                Some(value.to_owned())
            }
            None => None,
        };

        let transport_entry = self.required(&root, "", "transport", None)?;
        let transport = self.transport(transport_entry)?;
        let transport = match transport {
            Transport::Stdio { command, args, .. } => Transport::Stdio {
                command,
                args,
                env: self.env(&root)?,
            },
            Transport::Http { url, .. } => {
                if let Some(env) = find(&root, "env") {
                    return Err(self.error(Rule::TransportMismatch, "env", env.key_offset()));
                }
                Transport::Http {
                    url,
                    auth: self.auth(&root)?,
                }
            }
        };
        if matches!(transport, Transport::Stdio { .. })
            && let Some(auth) = find(&root, "auth")
        {
            return Err(self.error(Rule::TransportMismatch, "auth", auth.key_offset()));
        }

        let overrides = self.harness(&root)?;
        Ok((
            Definition {
                name: name.to_owned(),
                description,
                transport,
                overrides,
            },
            self.override_sites(&root),
        ))
    }

    /// Parses `transport`. A stdio transport is returned with an empty `env`
    /// and an HTTP transport with the default auth; the caller fills both in.
    fn transport(&self, entry: &Entry) -> Result<Transport> {
        let table = self.table(&entry.value, "transport")?;
        let offset = entry.value_offset();
        let kind_entry = self.required(table, "transport", "type", offset)?;
        let kind = self.string(&kind_entry.value, "transport.type")?;
        match kind {
            "stdio" => {
                self.check_keys(table, "transport", STDIO_KEYS)?;
                let command_entry = self.required(table, "transport", "command", offset)?;
                let command = self.string(&command_entry.value, "transport.command")?;
                let command_offset = command_entry.value_offset();
                self.no_variables(command, "transport.command", command_offset)?;
                let relative = command.contains('/') && !command.starts_with('/');
                if command.trim().is_empty() || command.starts_with('~') || relative {
                    return Err(self.error(Rule::CommandRule, "transport.command", command_offset));
                }
                let args = match find(table, "args") {
                    Some(args_entry) => {
                        let args = self.string_array(&args_entry.value, "transport.args")?;
                        for (position, (arg, arg_offset)) in args.iter().enumerate() {
                            self.arg(arg, &index("transport.args", position), *arg_offset)?;
                        }
                        args.into_iter().map(|(arg, _)| arg).collect()
                    }
                    None => Vec::new(),
                };
                Ok(Transport::Stdio {
                    command: command.to_owned(),
                    args,
                    env: BTreeMap::new(),
                })
            }
            "http" => {
                self.check_keys(table, "transport", HTTP_KEYS)?;
                let url_entry = self.required(table, "transport", "url", offset)?;
                let url = self.string(&url_entry.value, "transport.url")?;
                self.url(url, "transport.url", url_entry.value_offset())?;
                Ok(Transport::Http {
                    url: url.to_owned(),
                    auth: Auth::None,
                })
            }
            _ => Err(self.error(
                Rule::UnsupportedType,
                "transport.type",
                kind_entry.value_offset(),
            )),
        }
    }

    fn arg(&self, arg: &str, field: &str, offset: Option<usize>) -> Result<()> {
        self.no_variables(arg, field, offset)?;
        if let Some(flag) = arg.strip_prefix("--") {
            let (flag_name, value) = match flag.split_once('=') {
                Some((flag_name, value)) => (flag_name, Some(value)),
                None => (flag, None),
            };
            if secret_name_heuristic(flag_name) {
                return Err(self.error(Rule::SecretName, field, offset));
            }
            if let Some(value) = value {
                self.no_secret(value, field, offset)?;
            }
        }
        self.no_secret(arg, field, offset)
    }

    fn url(&self, url: &str, field: &str, offset: Option<usize>) -> Result<()> {
        self.no_variables(url, field, offset)?;
        // User info is checked before the shape, so a password that breaks
        // the shape (`user:p/ss@host`) is still reported as user info.
        if contains_userinfo(url) {
            return Err(self.error(Rule::UrlUserinfo, field, offset));
        }
        let Some(shape) = parse_url_shape(url) else {
            return Err(self.error(Rule::UrlInvalid, field, offset));
        };
        if shape.host.is_empty() {
            return Err(self.error(Rule::UrlInvalid, field, offset));
        }
        if shape.userinfo.is_some() {
            return Err(self.error(Rule::UrlUserinfo, field, offset));
        }
        let scheme_ok = shape.scheme.eq_ignore_ascii_case("https")
            || (shape.scheme.eq_ignore_ascii_case("http") && is_loopback(shape.host));
        if !scheme_ok {
            return Err(self.error(Rule::UrlScheme, field, offset));
        }
        if shape.query.is_some() {
            return Err(self.error(Rule::UrlQuery, field, offset));
        }
        self.no_secret(url, field, offset)
    }

    fn env(&self, root: &[Entry]) -> Result<BTreeMap<String, EnvValue>> {
        let Some(entry) = find(root, "env") else {
            return Ok(BTreeMap::new());
        };
        let table = self.table(&entry.value, "env")?;
        let mut env = BTreeMap::new();
        for item in table {
            let field = join("env", &item.key);
            self.variable_name(&item.key, &field, item.key_offset())?;
            let inner = self.table(&item.value, &field)?;
            self.check_keys(inner, &field, ENV_ENTRY_KEYS)?;
            let value = match (find(inner, "forward"), find(inner, "value")) {
                (Some(forward), None) => {
                    let forward_field = join(&field, "forward");
                    if !self.boolean(&forward.value, &forward_field)? {
                        return Err(self.error(
                            Rule::InvalidEnvEntry,
                            &forward_field,
                            forward.value_offset(),
                        ));
                    }
                    EnvValue::Forward
                }
                (None, Some(literal)) => {
                    let value_field = join(&field, "value");
                    let value = self.string(&literal.value, &value_field)?;
                    if secret_name_heuristic(&item.key) {
                        return Err(self.error(Rule::SecretName, &field, item.key_offset()));
                    }
                    self.no_variables(value, &value_field, literal.value_offset())?;
                    self.no_secret(value, &value_field, literal.value_offset())?;
                    EnvValue::Literal(value.to_owned())
                }
                _ => {
                    return Err(self.error(Rule::InvalidEnvEntry, &field, item.value_offset()));
                }
            };
            env.insert(item.key.clone(), value);
        }
        Ok(env)
    }

    fn auth(&self, root: &[Entry]) -> Result<Auth> {
        let Some(entry) = find(root, "auth") else {
            return Ok(default_auth());
        };
        let table = self.table(&entry.value, "auth")?;
        let offset = entry.value_offset();
        let kind_entry = self.required(table, "auth", "type", offset)?;
        let kind = self.string(&kind_entry.value, "auth.type")?;
        match kind {
            "oauth" => {
                self.check_keys(table, "auth", OAUTH_KEYS)?;
                let client_id = match find(table, "client_id") {
                    Some(item) => {
                        let value = self.string(&item.value, "auth.client_id")?;
                        self.no_secret(value, "auth.client_id", item.value_offset())?;
                        Some(value.to_owned())
                    }
                    None => None,
                };
                let mut scopes = BTreeSet::new();
                if let Some(item) = find(table, "scopes") {
                    let values = self.string_array(&item.value, "auth.scopes")?;
                    for (position, (scope, scope_offset)) in values.into_iter().enumerate() {
                        self.no_secret(&scope, &index("auth.scopes", position), scope_offset)?;
                        scopes.insert(scope);
                    }
                }
                let client_secret_env = match find(table, "client_secret_env") {
                    Some(item) => {
                        let value = self.string(&item.value, "auth.client_secret_env")?;
                        self.variable_name(value, "auth.client_secret_env", item.value_offset())?;
                        if client_id.is_none() {
                            return Err(self.error(
                                Rule::ClientSecretWithoutClientId,
                                "auth.client_secret_env",
                                item.key_offset(),
                            ));
                        }
                        Some(value.to_owned())
                    }
                    None => None,
                };
                Ok(Auth::OAuth {
                    client_id,
                    scopes: scopes.into_iter().collect(),
                    client_secret_env,
                })
            }
            "bearer_env" => {
                self.check_keys(table, "auth", BEARER_KEYS)?;
                let var = self.var(table, offset)?;
                let oauth_supported = match find(table, "oauth_supported") {
                    Some(item) => self.boolean(&item.value, "auth.oauth_supported")?,
                    None => false,
                };
                Ok(Auth::BearerEnv {
                    var,
                    oauth_supported,
                })
            }
            "header_env" => {
                self.check_keys(table, "auth", HEADER_KEYS)?;
                let header_entry = self.required(table, "auth", "header", offset)?;
                let header = self.string(&header_entry.value, "auth.header")?;
                if !is_valid_header_name(header) {
                    return Err(self.error(
                        Rule::HeaderName,
                        "auth.header",
                        header_entry.value_offset(),
                    ));
                }
                let var = self.var(table, offset)?;
                Ok(Auth::HeaderEnv {
                    header: header.to_owned(),
                    var,
                })
            }
            "none" => {
                self.check_keys(table, "auth", NONE_KEYS)?;
                Ok(Auth::None)
            }
            _ => Err(self.error(
                Rule::UnsupportedType,
                "auth.type",
                kind_entry.value_offset(),
            )),
        }
    }

    fn var(&self, table: &[Entry], parent: Option<usize>) -> Result<String> {
        let entry = self.required(table, "auth", "var", parent)?;
        let var = self.string(&entry.value, "auth.var")?;
        self.variable_name(var, "auth.var", entry.value_offset())?;
        Ok(var.to_owned())
    }

    fn harness(
        &self,
        root: &[Entry],
    ) -> Result<BTreeMap<String, serde_json::Map<String, serde_json::Value>>> {
        let mut overrides = BTreeMap::new();
        let Some(entry) = find(root, "harness") else {
            return Ok(overrides);
        };
        let table = self.table(&entry.value, "harness")?;
        self.check_keys(table, "harness", HARNESS_KEYS)?;
        for item in table {
            let field = join("harness", &item.key);
            let inner = self.table(&item.value, &field)?;
            let object = self.override_table(inner, &field, None, false)?;
            if !object.is_empty() {
                overrides.insert(item.key.clone(), object);
            }
        }
        Ok(overrides)
    }

    /// Records the kind and position of every top-level key of every harness
    /// table. The walk has already validated the `harness` structure.
    fn override_sites(&self, root: &[Entry]) -> OverrideSites {
        let mut sites = Vec::new();
        let Some(NodeKind::Table(harnesses)) = find(root, "harness").map(|entry| &entry.value.kind)
        else {
            return OverrideSites::default();
        };
        for harness in harnesses {
            let NodeKind::Table(entries) = &harness.value.kind else {
                continue;
            };
            for entry in entries {
                let (line, column) =
                    spanned::line_column(self.text, entry.key_offset().unwrap_or(0));
                sites.push(OverrideSite {
                    harness: harness.key.clone(),
                    key: entry.key.clone(),
                    kind: override_kind(&entry.value),
                    line,
                    column,
                });
            }
        }
        OverrideSites(sites)
    }

    /// Converts an override table. `table_key` is the key of this table in
    /// its parent override table, `None` for a harness table itself and for
    /// tables inside arrays. `in_env` is set when this table is an `env`
    /// table or lies inside one at any depth, including through arrays.
    fn override_table(
        &self,
        entries: &[Entry],
        path: &str,
        table_key: Option<&str>,
        in_env: bool,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut object = serde_json::Map::new();
        for entry in entries {
            let field = join(path, &entry.key);
            // A string under a secret-looking key is a literal secret, as in
            // the core `env` table, unless the key is an env reference.
            if let NodeKind::String(value) = &entry.value.kind
                && secret_name_heuristic(&entry.key)
                && !is_env_reference(in_env, table_key, &entry.key, value)
            {
                return Err(self.error(Rule::SecretName, &field, entry.key_offset()));
            }
            let child_in_env = in_env || entry.key.eq_ignore_ascii_case(LITERAL_ENV_TABLE);
            object.insert(
                entry.key.clone(),
                self.override_value(&entry.value, &field, Some(&entry.key), child_in_env)?,
            );
        }
        Ok(object)
    }

    /// Converts an override value. `key` is the key the value is stored
    /// under, `None` for array items. `in_env` is set when the value lies
    /// inside an `env` table at any depth; arrays pass it on to their items.
    fn override_value(
        &self,
        node: &Node,
        field: &str,
        key: Option<&str>,
        in_env: bool,
    ) -> Result<serde_json::Value> {
        Ok(match &node.kind {
            NodeKind::String(value) => {
                self.no_variables(value, field, node.offset())?;
                self.no_secret(value, field, node.offset())?;
                serde_json::Value::String(value.clone())
            }
            NodeKind::Integer(value) => serde_json::Value::from(*value),
            NodeKind::Float(value) => serde_json::Number::from_f64(*value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| self.error(Rule::NonFiniteFloat, field, node.offset()))?,
            NodeKind::Boolean(value) => serde_json::Value::Bool(*value),
            NodeKind::Datetime(value) => serde_json::Value::String(value.clone()),
            NodeKind::Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(position, item)| {
                        self.override_value(item, &index(field, position), None, in_env)
                    })
                    .collect::<Result<_>>()?,
            ),
            NodeKind::Table(entries) => {
                serde_json::Value::Object(self.override_table(entries, field, key, in_env)?)
            }
        })
    }
}

/// Whether an override string names an environment variable, so its key is
/// exempt from the name heuristic. The value must be a variable name, and
/// either the key's last `_`/`-` segment is `env` or `var`
/// (`bearer_token_env_var`, `client_secret_env`), or the table is listed in
/// [`ENV_REFERENCE_TABLES`]. Keys inside an `env` table (`in_env`), at any
/// depth and through arrays, are never exempt. The value heuristic still runs
/// on the value.
fn is_env_reference(in_env: bool, table_key: Option<&str>, key: &str, value: &str) -> bool {
    if in_env {
        return false;
    }
    if !is_valid_variable_name(value) {
        return false;
    }
    let last_segment = key.rsplit(['_', '-']).next().unwrap_or(key);
    last_segment.eq_ignore_ascii_case("env")
        || last_segment.eq_ignore_ascii_case("var")
        || table_key.is_some_and(|table| ENV_REFERENCE_TABLES.contains(&table))
}

/// The [`OverrideKind`] of an override value.
fn override_kind(node: &Node) -> OverrideKind {
    match &node.kind {
        NodeKind::String(_) => OverrideKind::String,
        NodeKind::Integer(value) => OverrideKind::Integer(*value),
        NodeKind::Float(value) => OverrideKind::Float(*value),
        NodeKind::Boolean(_) => OverrideKind::Boolean,
        NodeKind::Datetime(_) => OverrideKind::Datetime,
        NodeKind::Array(items) => OverrideKind::Array {
            all_strings: items
                .iter()
                .all(|item| matches!(item.kind, NodeKind::String(_))),
        },
        NodeKind::Table(_) => OverrideKind::Table,
    }
}

fn default_auth() -> Auth {
    Auth::OAuth {
        client_id: None,
        scopes: Vec::new(),
        client_secret_env: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_redacts_suspicious_keys() {
        assert_eq!(join("", "name"), "name");
        assert_eq!(join("env", "LOG_LEVEL"), "env.LOG_LEVEL");
        assert_eq!(
            join("harness.codex", "ghp_abcdef"),
            "harness.codex.<redacted>"
        );
        assert_eq!(join("harness.codex", "a b"), "harness.codex.<redacted>");
        assert_eq!(join("harness.codex", ""), "harness.codex.<redacted>");
    }

    #[test]
    fn env_reference_keys() {
        assert!(is_env_reference(
            false,
            None,
            "bearer_token_env_var",
            "GITHUB_TOKEN"
        ));
        assert!(is_env_reference(
            false,
            None,
            "client_secret_env",
            "CLIENT_SECRET"
        ));
        assert!(is_env_reference(false, Some("x"), "token-ENV", "MY_TOKEN"));
        assert!(is_env_reference(
            false,
            Some("env_http_headers"),
            "X-Auth-Token",
            "MY_TOKEN_VAR"
        ));
        // Keys inside a literal `env` table, at any depth, are never exempt.
        assert!(!is_env_reference(
            true,
            Some("env"),
            "token_env",
            "MY_TOKEN"
        ));
        assert!(!is_env_reference(
            true,
            Some("sub"),
            "token_var",
            "MY_TOKEN"
        ));
        assert!(!is_env_reference(true, None, "password_var", "MY_TOKEN"));
        assert!(!is_env_reference(
            true,
            Some("env_http_headers"),
            "X-Auth-Token",
            "MY_TOKEN_VAR"
        ));
        // The value must be a variable name.
        assert!(!is_env_reference(false, None, "token_env", "not a name"));
        assert!(!is_env_reference(false, None, "token_env", "1ABC"));
        assert!(!is_env_reference(
            false,
            Some("env_http_headers"),
            "X-Auth-Token",
            "a-b"
        ));
        // Other keys and tables are not exempt.
        assert!(!is_env_reference(false, None, "api_key", "MY_KEY"));
        assert!(!is_env_reference(
            false,
            Some("headers"),
            "X-Auth-Token",
            "MY_KEY"
        ));
        assert!(!is_env_reference(
            false,
            None,
            "environment_token",
            "MY_TOKEN"
        ));
    }

    #[test]
    fn env_table_exemption_is_lost_at_any_depth() {
        let base = "name = \"x\"\n[transport]\ntype = \"stdio\"\ncommand = \"server\"\n";
        for overrides in [
            "[[harness.codex.env]]\npassword_var = \"hunter2\"\n",
            "[harness.codex]\nenv = [{ token_env = \"hunter2\" }]\n",
            "[harness.codex.env.sub]\ntoken_env = \"hunter2\"\n",
            "[harness.codex.ENV]\nnested = [[{ token_var = \"MY_TOKEN\" }]]\n",
        ] {
            let error = walk(&format!("{base}{overrides}"), "x").unwrap_err();
            assert_eq!(error.rule, Rule::SecretName, "{overrides}");
        }
        for overrides in [
            "[harness.codex]\nbearer_token_env_var = \"GITHUB_TOKEN\"\n",
            "[harness.codex.env_http_headers]\nX-Auth-Token = \"MY_TOKEN_VAR\"\n",
            "[harness.codex]\nservers = [{ token_env = \"MY_TOKEN\" }]\n",
        ] {
            assert!(
                walk(&format!("{base}{overrides}"), "x").is_ok(),
                "{overrides}"
            );
        }
    }

    #[test]
    fn loopback_hosts() {
        for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.255.0.9", "::1"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["example.com", "128.0.0.1", "::2", "localhost.example.com"] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}
