//! The definition serializer: a [`Definition`] as the text of
//! `servers/<name>.toml`, in the field order of the normative examples.
//!
//! Adoption (WP4) writes imported definitions with it into the built-in
//! `local` source. The output parses back into an equal definition with
//! [`super::parse_definition`]; adoption checks that before it writes.
//! Defaults are written only where the model cannot omit them (`auth` of an
//! http server is always written, so the file states it).

use serde_json::Value;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, value};

use super::{Auth, Definition, EnvValue, Transport};

/// Converts an override value into a TOML value. Objects become inline
/// tables; JSON has no null in override tables.
fn toml_value(json: &Value) -> toml_edit::Value {
    match json {
        Value::String(text) => text.as_str().into(),
        Value::Bool(flag) => (*flag).into(),
        Value::Number(number) => match number.as_i64() {
            Some(integer) => integer.into(),
            None => number.as_f64().unwrap_or_default().into(),
        },
        Value::Array(items) => {
            let mut array = Array::new();
            for item in items {
                array.push(toml_value(item));
            }
            array.into()
        }
        Value::Object(object) => {
            let mut table = InlineTable::new();
            for (key, item) in object {
                table.insert(key, toml_value(item));
            }
            table.into()
        }
        Value::Null => "".into(),
    }
}

fn strings(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}

fn auth_table(auth: &Auth) -> Table {
    let mut table = Table::new();
    table["type"] = value(auth.kind());
    match auth {
        Auth::OAuth {
            client_id,
            scopes,
            client_secret_env,
        } => {
            if let Some(client_id) = client_id {
                table["client_id"] = value(client_id.as_str());
            }
            if !scopes.is_empty() {
                table["scopes"] = value(strings(scopes));
            }
            if let Some(var) = client_secret_env {
                table["client_secret_env"] = value(var.as_str());
            }
        }
        Auth::BearerEnv {
            var,
            oauth_supported,
        } => {
            table["var"] = value(var.as_str());
            if *oauth_supported {
                table["oauth_supported"] = value(true);
            }
        }
        Auth::HeaderEnv { header, var } => {
            table["header"] = value(header.as_str());
            table["var"] = value(var.as_str());
        }
        Auth::None => {}
    }
    table
}

/// The TOML text of a definition.
#[must_use]
pub fn to_toml(definition: &Definition) -> String {
    let mut document = DocumentMut::new();
    document["name"] = value(definition.name.as_str());
    if let Some(description) = &definition.description {
        document["description"] = value(description.as_str());
    }
    let mut transport = Table::new();
    let mut env_table = None;
    let mut auth = None;
    match &definition.transport {
        Transport::Stdio { command, args, env } => {
            transport["type"] = value("stdio");
            transport["command"] = value(command.as_str());
            if !args.is_empty() {
                transport["args"] = value(strings(args));
            }
            if !env.is_empty() {
                let mut table = Table::new();
                for (name, entry) in env {
                    let mut inline = InlineTable::new();
                    match entry {
                        EnvValue::Forward => inline.insert("forward", true.into()),
                        EnvValue::Literal(literal) => inline.insert("value", literal.into()),
                    };
                    table[name.as_str()] = value(inline);
                }
                env_table = Some(table);
            }
        }
        Transport::Http { url, auth: method } => {
            transport["type"] = value("http");
            transport["url"] = value(url.as_str());
            auth = Some(auth_table(method));
        }
    }
    document["transport"] = Item::Table(transport);
    if let Some(table) = env_table {
        document["env"] = Item::Table(table);
    }
    if let Some(table) = auth {
        document["auth"] = Item::Table(table);
    }
    if !definition.overrides.is_empty() {
        let mut harness = Table::new();
        harness.set_implicit(true);
        for (id, overrides) in &definition.overrides {
            let mut table = Table::new();
            for (key, item) in overrides {
                table[key.as_str()] = value(toml_value(item));
            }
            harness[id.as_str()] = Item::Table(table);
        }
        document["harness"] = Item::Table(harness);
    }
    document.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::parse_definition;
    use std::collections::BTreeMap;

    fn round_trip(definition: &Definition) -> String {
        let text = to_toml(definition);
        assert_eq!(
            parse_definition(&text, &definition.name).as_ref(),
            Ok(definition),
            "{text}"
        );
        text
    }

    #[test]
    fn every_shape_parses_back_unchanged() {
        let mut env = BTreeMap::new();
        env.insert("A_TOKEN".to_owned(), EnvValue::Forward);
        env.insert(
            "MODE".to_owned(),
            EnvValue::Literal("fast \"x\"".to_owned()),
        );
        let mut codex = serde_json::Map::new();
        codex.insert("cwd".to_owned(), Value::String("/srv".to_owned()));
        codex.insert("enabled".to_owned(), Value::Bool(false));
        codex.insert("startup_timeout_sec".to_owned(), serde_json::json!(5.5));
        codex.insert("tool_timeout_sec".to_owned(), serde_json::json!(30));
        codex.insert("enabled_tools".to_owned(), serde_json::json!(["a", "b"]));
        let stdio = Definition {
            name: "gh".to_owned(),
            description: Some("GitHub".to_owned()),
            transport: Transport::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "gh".to_owned()],
                env,
            },
            overrides: BTreeMap::from([("codex".to_owned(), codex)]),
        };
        let text = round_trip(&stdio);
        assert!(text.starts_with("name = \"gh\"\ndescription = \"GitHub\"\n\n[transport]\n"));
        assert!(text.contains("[harness.codex]\n"), "{text}");
        assert!(!text.contains("[harness]\n"), "{text}");
        let bare = Definition {
            name: "a".to_owned(),
            description: None,
            transport: Transport::Stdio {
                command: "/bin/a".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            overrides: BTreeMap::new(),
        };
        round_trip(&bare);
        let auths = [
            Auth::OAuth {
                client_id: Some("cid".to_owned()),
                scopes: vec!["read".to_owned(), "write".to_owned()],
                client_secret_env: Some("CLIENT_SECRET".to_owned()),
            },
            Auth::OAuth {
                client_id: None,
                scopes: Vec::new(),
                client_secret_env: None,
            },
            Auth::BearerEnv {
                var: "TOKEN".to_owned(),
                oauth_supported: true,
            },
            Auth::BearerEnv {
                var: "TOKEN".to_owned(),
                oauth_supported: false,
            },
            Auth::HeaderEnv {
                header: "X-Api-Key".to_owned(),
                var: "KEY".to_owned(),
            },
            Auth::None,
        ];
        for auth in auths {
            let mut claude = serde_json::Map::new();
            claude.insert("timeout".to_owned(), serde_json::json!(30000));
            let http = Definition {
                name: "remote".to_owned(),
                description: None,
                transport: Transport::Http {
                    url: "https://mcp.example.test/mcp".to_owned(),
                    auth,
                },
                overrides: BTreeMap::from([("claude".to_owned(), claude)]),
            };
            round_trip(&http);
        }
        assert_eq!(toml_value(&Value::Null).as_str(), Some(""));
        let nested = toml_value(&serde_json::json!({"k": [1, true]}));
        assert!(nested.is_inline_table());
    }
}
