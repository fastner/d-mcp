//! The redacted view of harness entry content: an allow-list, default deny.
//!
//! Every place that renders the content of a harness entry (inventory, the
//! adoption preview, and later status and doctor) goes through
//! [`entry_view`]; a neutral definition that adoption would write goes
//! through [`definition_view`], with the same vocabulary.
//! Only these parts are visible:
//!
//! - the transport kind, and of a URL its scheme, host and port;
//! - the *names* of env entries, headers and referenced variables;
//! - the auth kind, the override *keys*, and whether `oauth.clientId`
//!   (`oauth.client_id`) is present, not its value.
//!
//! Entry names and project paths are shown by the callers, as sync's
//! shadowing notices already do. Everything else is redacted: `command`,
//! `args`, `cwd`, URL user info, path, query and fragment, env and header
//! values, `description`, the client id, scopes, override values, and any
//! key not listed. A redacted part that is present is named in
//! [`EntryView::hidden`], from a fixed vocabulary, so the view says *that*
//! there is content without showing it. A name that does not look like a
//! name (see [`safe_key`]) is replaced by [`REDACTED_KEY`].
//!
//! The existing invariants are kept, not re-implemented here: child stdout
//! is only parsed and child stderr is dropped ([`crate::process`]), sync
//! reports carry no field values, and parser errors carry only file, line
//! and column.

use std::borrow::Cow;
use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::adapter::{Imported, RawEntry};
use crate::config::Harness;
use crate::credential::{parse_url_shape, secret_value_heuristic};
use crate::definition::{Auth, Definition, EnvValue, Transport, is_valid_variable_name};

/// What a name that does not look like a name is shown as.
pub const REDACTED_KEY: &str = "<redacted-key>";

/// The longest name shown as it is.
const MAX_SHOWN_NAME: usize = 64;

/// A name as it may be shown: kept when it has 1 to 64 characters from
/// `[A-Za-z0-9_.-]` and does not look like a secret by
/// [`secret_value_heuristic`]; otherwise [`REDACTED_KEY`].
#[must_use]
pub fn safe_key(name: &str) -> Cow<'_, str> {
    let shaped = !name.is_empty()
        && name.len() <= MAX_SHOWN_NAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'));
    if shaped && secret_value_heuristic(name).is_none() {
        Cow::Borrowed(name)
    } else {
        Cow::Borrowed(REDACTED_KEY)
    }
}

/// The visible parts of a URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UrlView {
    /// The scheme, lower-cased.
    pub scheme: String,
    /// The host; [`REDACTED_KEY`] when it does not look like a host name.
    pub host: String,
    /// The port, when one is written.
    pub port: Option<u16>,
    /// The redacted parts that are present: `userinfo`, `path`, `query`,
    /// `fragment`.
    pub hidden: Vec<&'static str>,
}

impl UrlView {
    /// `scheme://host[:port]`.
    #[must_use]
    pub fn origin(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// The visible parts of a URL; `None` when the value is not URL-shaped
/// (it is then hidden as a whole).
#[must_use]
pub fn url_view(url: &str) -> Option<UrlView> {
    let shape = parse_url_shape(url)?;
    let mut hidden = Vec::new();
    if shape.userinfo.is_some() {
        hidden.push("userinfo");
    }
    if !shape.path.is_empty() && shape.path != "/" {
        hidden.push("path");
    }
    if shape.query.is_some() {
        hidden.push("query");
    }
    if shape.fragment.is_some() {
        hidden.push("fragment");
    }
    let port = shape.port.and_then(|port| port.parse::<u16>().ok());
    let host_shaped = !shape.host.is_empty()
        && shape.host.len() <= 253
        && shape
            .host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'));
    let host = if host_shaped && secret_value_heuristic(shape.host).is_none() {
        shape.host.to_ascii_lowercase()
    } else {
        REDACTED_KEY.to_owned()
    };
    Some(UrlView {
        scheme: shape.scheme.to_ascii_lowercase(),
        host,
        port,
        hidden,
    })
}

/// The visible parts of a URL, or a fully hidden one when the value is not
/// URL-shaped.
fn visible_url(url: &str) -> UrlView {
    url_view(url).unwrap_or_else(|| UrlView {
        scheme: REDACTED_KEY.to_owned(),
        host: REDACTED_KEY.to_owned(),
        port: None,
        hidden: vec!["url"],
    })
}

/// The redacted view of one harness entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EntryView {
    /// `stdio`, `http`, `sse` or `unknown`.
    pub transport: &'static str,
    /// The visible parts of the URL, for a remote entry.
    pub url: Option<UrlView>,
    /// The env entry names (literals and forwards alike), sorted.
    pub env: Vec<String>,
    /// The header names, sorted.
    pub headers: Vec<String>,
    /// The names of the variables the entry reads, sorted.
    pub variables: Vec<String>,
    /// The auth kind of the imported candidate (`oauth`, `bearer_env`,
    /// `header_env`, `none`), when the import succeeded.
    pub auth: Option<&'static str>,
    /// The `harness.<id>` override keys of the imported candidate, or the
    /// native override-like keys of the entry, sorted.
    pub override_keys: Vec<String>,
    /// Whether an OAuth client id is present (never its value).
    pub oauth_client_id: bool,
    /// The redacted parts that are present, sorted.
    pub hidden: Vec<&'static str>,
}

impl EntryView {
    /// A one-line text form: `transport=... url=... env=... ...`.
    #[must_use]
    pub fn text(&self) -> String {
        let mut parts = vec![format!("transport={}", self.transport)];
        let mut hidden: Vec<String> = self.hidden.iter().map(|part| (*part).to_owned()).collect();
        if let Some(url) = &self.url {
            parts.push(format!("url={}", url.origin()));
            hidden.extend(url.hidden.iter().map(|part| format!("url.{part}")));
        }
        if let Some(auth) = self.auth {
            parts.push(format!("auth={auth}"));
        }
        for (label, names) in [
            ("env", &self.env),
            ("headers", &self.headers),
            ("vars", &self.variables),
            ("overrides", &self.override_keys),
        ] {
            if !names.is_empty() {
                parts.push(format!("{label}={}", names.join(",")));
            }
        }
        if self.oauth_client_id {
            parts.push("client_id=set".to_owned());
        }
        if !hidden.is_empty() {
            parts.push(format!("hidden={}", hidden.join(",")));
        }
        parts.join(" ")
    }
}

/// Collects the view while walking the native keys.
#[derive(Default)]
struct Builder {
    env: BTreeSet<String>,
    headers: BTreeSet<String>,
    variables: BTreeSet<String>,
    override_keys: BTreeSet<String>,
    hidden: BTreeSet<&'static str>,
}

impl Builder {
    fn name(set: &mut BTreeSet<String>, name: &str) {
        set.insert(safe_key(name).into_owned());
    }

    fn variable(&mut self, name: &str) {
        if is_valid_variable_name(name) {
            Self::name(&mut self.variables, name);
        } else {
            self.variables.insert(REDACTED_KEY.to_owned());
        }
    }

    /// The variables a Claude Code value references: `${NAME}` and
    /// `${NAME:-...}`.
    fn references(&mut self, value: &str) {
        let mut rest = value;
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else { break };
            let inner = &after[..end];
            let name = inner.split(":-").next().unwrap_or(inner);
            self.variable(name);
            rest = &after[end + 1..];
        }
    }

    fn object_names(&mut self, value: &Value, into: fn(&mut Self) -> &mut BTreeSet<String>) {
        if let Some(object) = value.as_object() {
            for key in object.keys() {
                Self::name(into(self), key);
            }
        }
    }
}

/// Native override-like keys: Codex's allowed `harness.codex` keys and
/// Claude Code's `timeout`.
const CODEX_OVERRIDE_KEYS: &[&str] = &[
    "enabled",
    "required",
    "startup_timeout_sec",
    "startup_timeout_ms",
    "tool_timeout_sec",
    "enabled_tools",
    "disabled_tools",
];

fn claude_view(object: &Map<String, Value>, builder: &mut Builder) -> &'static str {
    let mut transport = "unknown";
    for (key, value) in object {
        match key.as_str() {
            "type" => {
                transport = match value.as_str() {
                    Some("http") => "http",
                    Some("sse") => "sse",
                    Some("stdio") => "stdio",
                    _ => "unknown",
                };
            }
            "command" => {
                builder.hidden.insert("command");
            }
            "args" => {
                builder.hidden.insert("args");
            }
            "env" => {
                builder.object_names(value, |b| &mut b.env);
                if let Some(env) = value.as_object() {
                    for item in env.values() {
                        builder.references(item.as_str().unwrap_or_default());
                    }
                    if !env.is_empty() {
                        builder.hidden.insert("env.values");
                    }
                }
            }
            "headers" => {
                builder.object_names(value, |b| &mut b.headers);
                if let Some(headers) = value.as_object() {
                    for item in headers.values() {
                        builder.references(item.as_str().unwrap_or_default());
                    }
                    if !headers.is_empty() {
                        builder.hidden.insert("headers.values");
                    }
                }
            }
            "oauth" => {
                if let Some(oauth) = value.as_object() {
                    for sub in oauth.keys() {
                        match sub.as_str() {
                            "clientId" => {
                                builder.hidden.insert("oauth.client_id");
                            }
                            "scopes" => {
                                builder.hidden.insert("oauth.scopes");
                            }
                            "callbackPort" => {
                                builder
                                    .override_keys
                                    .insert("oauth_callback_port".to_owned());
                                builder.hidden.insert("override.values");
                            }
                            _ => {
                                builder.hidden.insert("other");
                            }
                        }
                    }
                }
            }
            "timeout" => {
                builder.override_keys.insert("timeout".to_owned());
                builder.hidden.insert("override.values");
            }
            "url" => {}
            _ => {
                builder.hidden.insert("other");
            }
        }
    }
    if transport == "unknown" && object.contains_key("command") && !object.contains_key("type") {
        transport = "stdio";
    }
    transport
}

fn codex_view(object: &Map<String, Value>, builder: &mut Builder) -> &'static str {
    let transport = match (object.contains_key("command"), object.contains_key("url")) {
        (true, false) => "stdio",
        (false, true) => "http",
        _ => "unknown",
    };
    for (key, value) in object {
        match key.as_str() {
            "command" | "args" | "cwd" => {
                builder.hidden.insert(match key.as_str() {
                    "command" => "command",
                    "args" => "args",
                    _ => "cwd",
                });
            }
            "env" => {
                builder.object_names(value, |b| &mut b.env);
                if value.as_object().is_some_and(|env| !env.is_empty()) {
                    builder.hidden.insert("env.values");
                }
            }
            "env_vars" => {
                for item in value.as_array().into_iter().flatten() {
                    let name = item
                        .as_str()
                        .or_else(|| item.get("name").and_then(Value::as_str))
                        .unwrap_or_default();
                    Builder::name(&mut builder.env, name);
                    builder.variable(name);
                }
            }
            "bearer_token_env_var" => builder.variable(value.as_str().unwrap_or_default()),
            "env_http_headers" => {
                builder.object_names(value, |b| &mut b.headers);
                for item in value.as_object().into_iter().flat_map(Map::values) {
                    builder.variable(item.as_str().unwrap_or_default());
                }
            }
            "http_headers" => {
                builder.object_names(value, |b| &mut b.headers);
                builder.hidden.insert("headers.values");
            }
            "oauth" => {
                if let Some(oauth) = value.as_object() {
                    for sub in oauth.keys() {
                        builder.hidden.insert(if sub == "client_id" {
                            "oauth.client_id"
                        } else {
                            "other"
                        });
                    }
                }
            }
            "scopes" => {
                builder.hidden.insert("oauth.scopes");
            }
            "url" => {}
            other if CODEX_OVERRIDE_KEYS.contains(&other) => {
                builder.override_keys.insert(other.to_owned());
                builder.hidden.insert("override.values");
            }
            _ => {
                builder.hidden.insert("other");
            }
        }
    }
    transport
}

/// The redacted view of a native entry of `harness`, with the auth kind and
/// override keys of its imported candidate when the import succeeded.
#[must_use]
pub fn entry_view(harness: Harness, raw: &RawEntry, imported: Option<&Imported>) -> EntryView {
    let mut builder = Builder::default();
    let Some(object) = raw.value().as_object() else {
        return EntryView {
            transport: "unknown",
            hidden: vec!["other"],
            ..EntryView::default()
        };
    };
    let transport = match harness {
        Harness::Claude => claude_view(object, &mut builder),
        Harness::Codex => codex_view(object, &mut builder),
    };
    let url = object.get("url").and_then(Value::as_str).map(visible_url);
    let oauth_client_id = builder.hidden.contains("oauth.client_id");
    let mut override_keys = builder.override_keys;
    let auth = imported.and_then(|imported| {
        override_keys = imported
            .definition
            .overrides
            .values()
            .flat_map(|table| table.keys().map(|key| safe_key(key).into_owned()))
            .collect();
        imported.definition.auth_kind()
    });
    EntryView {
        transport,
        url,
        env: builder.env.into_iter().collect(),
        headers: builder.headers.into_iter().collect(),
        variables: builder.variables.into_iter().collect(),
        auth,
        override_keys: override_keys.into_iter().collect(),
        oauth_client_id,
        hidden: builder.hidden.into_iter().collect(),
    }
}

/// The redacted view of a neutral definition: the names of its env entries,
/// variables, header and override keys, its transport and auth kind and the
/// visible URL parts. `command`, `args`, env literals, `description`, the
/// client id, scopes and override values are named in `hidden` only.
#[must_use]
pub fn definition_view(definition: &Definition) -> EntryView {
    let mut builder = Builder::default();
    let mut oauth_client_id = false;
    let (transport, url) = match &definition.transport {
        Transport::Stdio { args, env, .. } => {
            builder.hidden.insert("command");
            if !args.is_empty() {
                builder.hidden.insert("args");
            }
            for (name, value) in env {
                Builder::name(&mut builder.env, name);
                match value {
                    EnvValue::Forward => builder.variable(name),
                    EnvValue::Literal(_) => {
                        builder.hidden.insert("env.values");
                    }
                }
            }
            ("stdio", None)
        }
        Transport::Http { url, auth } => {
            match auth {
                Auth::OAuth {
                    client_id,
                    scopes,
                    client_secret_env,
                } => {
                    if client_id.is_some() {
                        oauth_client_id = true;
                        builder.hidden.insert("oauth.client_id");
                    }
                    if !scopes.is_empty() {
                        builder.hidden.insert("oauth.scopes");
                    }
                    if let Some(var) = client_secret_env {
                        builder.variable(var);
                    }
                }
                Auth::BearerEnv { var, .. } => builder.variable(var),
                Auth::HeaderEnv { header, var } => {
                    Builder::name(&mut builder.headers, header);
                    builder.variable(var);
                }
                Auth::None => {}
            }
            ("http", Some(visible_url(url)))
        }
    };
    if definition.description.is_some() {
        builder.hidden.insert("description");
    }
    for table in definition.overrides.values() {
        for key in table.keys() {
            Builder::name(&mut builder.override_keys, key);
            builder.hidden.insert("override.values");
        }
    }
    EntryView {
        transport,
        url,
        env: builder.env.into_iter().collect(),
        headers: builder.headers.into_iter().collect(),
        variables: builder.variables.into_iter().collect(),
        auth: definition.auth_kind(),
        override_keys: builder.override_keys.into_iter().collect(),
        oauth_client_id,
        hidden: builder.hidden.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn definitions_show_names_only() {
        let stdio = crate::definition::parse_definition(
            "name = \"a\"\ndescription = \"CANARY\"\n[transport]\ntype = \"stdio\"\n\
             command = \"/opt/CANARY/bin\"\nargs = [\"CANARY\"]\n[env]\nFWD = { forward = true }\n\
             LIT = { value = \"CANARYVALUE\" }\n[harness.codex]\ncwd = \"/srv/CANARY\"\n",
            "a",
        )
        .unwrap();
        let view = definition_view(&stdio);
        assert_clean(&view);
        assert_eq!(view.env, ["FWD", "LIT"]);
        assert_eq!(view.variables, ["FWD"]);
        assert_eq!(view.override_keys, ["cwd"]);
        assert_eq!(
            view.hidden,
            [
                "args",
                "command",
                "description",
                "env.values",
                "override.values"
            ]
        );
        for (auth, auth_kind, variables, headers) in [
            (
                "type = \"oauth\"\nclient_id = \"CANARY\"\nscopes = [\"CANARY\"]\nclient_secret_env = \"SECRET\"",
                "oauth",
                vec!["SECRET"],
                vec![],
            ),
            (
                "type = \"bearer_env\"\nvar = \"TOKEN\"",
                "bearer_env",
                vec!["TOKEN"],
                vec![],
            ),
            (
                "type = \"header_env\"\nheader = \"X-Key\"\nvar = \"KEY\"",
                "header_env",
                vec!["KEY"],
                vec!["X-Key"],
            ),
            ("type = \"none\"", "none", vec![], vec![]),
        ] {
            let http = crate::definition::parse_definition(
                &format!(
                    "name = \"h\"\n[transport]\ntype = \"http\"\nurl = \"https://mcp.example.test/CANARY\"\n[auth]\n{auth}\n"
                ),
                "h",
            )
            .unwrap();
            let view = definition_view(&http);
            assert_clean(&view);
            assert_eq!(view.auth, Some(auth_kind));
            assert_eq!(view.variables, variables);
            assert_eq!(view.headers, headers);
            assert_eq!(
                view.url.as_ref().unwrap().origin(),
                "https://mcp.example.test"
            );
            assert_eq!(view.oauth_client_id, auth_kind == "oauth");
        }
    }

    fn assert_clean(view: &EntryView) {
        let text = format!("{} {}", view.text(), serde_json::to_string(view).unwrap());
        assert!(!text.contains("CANARY"), "{text}");
    }

    #[test]
    fn names_that_do_not_look_like_names_are_redacted() {
        assert_eq!(safe_key("GITHUB_TOKEN"), "GITHUB_TOKEN");
        assert_eq!(safe_key("X-Api-Key"), "X-Api-Key");
        assert_eq!(safe_key(""), REDACTED_KEY);
        assert_eq!(safe_key("a b"), REDACTED_KEY);
        assert_eq!(
            safe_key("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            REDACTED_KEY
        );
        assert_eq!(safe_key(&"a".repeat(65)), REDACTED_KEY);
        assert_eq!(safe_key("key=value"), REDACTED_KEY);
    }

    #[test]
    fn urls_show_scheme_host_and_port_only() {
        let view =
            url_view("https://user:CANARY@Mcp.Example.test:8443/CANARY/p?t=CANARY#CANARY").unwrap();
        assert_eq!(view.origin(), "https://mcp.example.test:8443");
        assert_eq!(view.hidden, ["userinfo", "path", "query", "fragment"]);
        let plain = url_view("http://127.0.0.1/").unwrap();
        assert_eq!(plain.origin(), "http://127.0.0.1");
        assert!(plain.hidden.is_empty());
        let v6 = url_view("http://[::1]:3000/mcp").unwrap();
        assert_eq!(v6.port, Some(3000));
        assert_eq!(url_view("not a url"), None);
        let odd = url_view("https://ghp_abcdefghijklmnopqrstuvwxyz0123456789.test/").unwrap();
        assert_eq!(odd.host, REDACTED_KEY);
    }

    #[test]
    fn claude_entries_show_names_only() {
        let raw = RawEntry::new(json!({
            "command": "/opt/CANARY/bin",
            "args": ["--token", "CANARY"],
            "env": {"TOKEN": "CANARY", "HOME_DIR": "${HOME_DIR:-}", "ghp_abcdefghijklmnopqrstuvwxyz0123456789": "x"},
            "timeout": 5000,
            "description": "CANARY",
            "weird": "CANARY",
        }));
        let view = entry_view(Harness::Claude, &raw, None);
        assert_clean(&view);
        assert_eq!(view.transport, "stdio");
        assert_eq!(view.env, ["<redacted-key>", "HOME_DIR", "TOKEN"]);
        assert_eq!(view.variables, ["HOME_DIR"]);
        assert_eq!(view.override_keys, ["timeout"]);
        assert_eq!(
            view.hidden,
            ["args", "command", "env.values", "other", "override.values"]
        );
        let remote = RawEntry::new(json!({
            "type": "http",
            "url": "https://u:CANARY@mcp.example.test/CANARY?k=CANARY",
            "headers": {"Authorization": "Bearer ${GH_TOKEN}"},
            "oauth": {"clientId": "CANARY", "scopes": "CANARY", "callbackPort": 3118},
        }));
        let view = entry_view(Harness::Claude, &remote, None);
        assert_clean(&view);
        assert_eq!(view.transport, "http");
        assert!(view.oauth_client_id);
        assert_eq!(view.headers, ["Authorization"]);
        assert_eq!(view.variables, ["GH_TOKEN"]);
        assert!(view.text().contains("url=https://mcp.example.test"));
        assert!(view.text().contains("url.userinfo,url.path,url.query"));
        assert_eq!(
            entry_view(
                Harness::Claude,
                &RawEntry::new(json!({"type": "sse", "url": "x"})),
                None
            )
            .transport,
            "sse"
        );
        let not_object = entry_view(Harness::Claude, &RawEntry::new(json!("CANARY")), None);
        assert_clean(&not_object);
        assert_eq!(not_object.hidden, ["other"]);
    }

    #[test]
    fn codex_entries_show_names_only() {
        let raw = RawEntry::new(json!({
            "command": "CANARY", "args": ["CANARY"], "cwd": "/srv/CANARY",
            "env": {"LIT": "CANARY"}, "env_vars": ["FWD", {"name": "TABLE", "source": "local"}],
            "enabled": false, "tool_timeout_sec": 5,
        }));
        let view = entry_view(Harness::Codex, &raw, None);
        assert_clean(&view);
        assert_eq!(view.transport, "stdio");
        assert_eq!(view.env, ["FWD", "LIT", "TABLE"]);
        assert_eq!(view.variables, ["FWD", "TABLE"]);
        assert_eq!(view.override_keys, ["enabled", "tool_timeout_sec"]);
        assert_eq!(
            view.hidden,
            ["args", "command", "cwd", "env.values", "override.values"]
        );
        let remote = RawEntry::new(json!({
            "url": "https://mcp.example.test/mcp", "bearer_token_env_var": "TOKEN",
            "http_headers": {"X-Key": "CANARY"}, "env_http_headers": {"X-Other": "OTHER"},
            "oauth": {"client_id": "CANARY"}, "scopes": ["CANARY"], "http_headers_helper": "CANARY",
        }));
        let view = entry_view(Harness::Codex, &remote, None);
        assert_clean(&view);
        assert_eq!(view.transport, "http");
        assert_eq!(view.headers, ["X-Key", "X-Other"]);
        assert_eq!(view.variables, ["OTHER", "TOKEN"]);
        assert!(view.oauth_client_id);
        assert!(view.hidden.contains(&"other"));
        let broken = RawEntry::new(json!({"url": "CANARY no url"}));
        let view = entry_view(Harness::Codex, &broken, None);
        assert_clean(&view);
        assert_eq!(view.url.unwrap().hidden, ["url"]);
    }
}
