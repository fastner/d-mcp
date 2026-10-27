//! Canonical JSON and the two definition hashes.
//!
//! The canonical JSON is the model after defaults with sorted keys and no
//! whitespace, written by the shared [`crate::canonical`] writer.
//! Integers and floats stay distinct; floats use `serde_json`'s shortest
//! round-trip form, and NaN or infinity never reach this module because the
//! walker rejects them.

use serde_json::{Map, Value, json};

use crate::canonical::{tagged_sha256, write_canonical};

use super::{Auth, Definition, EnvValue, Transport};

/// Domain tag of `definition_hash`.
pub const DEFINITION_DOMAIN_TAG: &str = "d-mcp-definition-v1";
/// Domain tag of `core_hash`.
pub const CORE_DOMAIN_TAG: &str = "d-mcp-core-v1";

/// Returns the canonical JSON of a definition. With `full = false`,
/// `description` and the override tables are left out (the `core_hash`
/// input).
#[must_use]
pub fn canonical_json(definition: &Definition, full: bool) -> String {
    let mut object = Map::new();
    object.insert("name".to_owned(), Value::String(definition.name.clone()));
    match &definition.transport {
        Transport::Stdio { command, args, env } => {
            object.insert(
                "transport".to_owned(),
                json!({ "type": "stdio", "command": command, "args": args }),
            );
            let env: Map<String, Value> = env
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        EnvValue::Forward => json!({ "forward": true }),
                        EnvValue::Literal(literal) => json!({ "value": literal }),
                    };
                    (name.clone(), value)
                })
                .collect();
            object.insert("env".to_owned(), Value::Object(env));
        }
        Transport::Http { url, auth } => {
            object.insert(
                "transport".to_owned(),
                json!({ "type": "http", "url": url }),
            );
            object.insert("auth".to_owned(), auth_value(auth));
        }
    }
    if full {
        if let Some(description) = &definition.description {
            object.insert("description".to_owned(), Value::String(description.clone()));
        }
        if !definition.overrides.is_empty() {
            let harness: Map<String, Value> = definition
                .overrides
                .iter()
                .map(|(harness, table)| (harness.clone(), Value::Object(table.clone())))
                .collect();
            object.insert("harness".to_owned(), Value::Object(harness));
        }
    }
    let mut out = String::new();
    write_canonical(&Value::Object(object), &mut out);
    out
}

fn auth_value(auth: &Auth) -> Value {
    match auth {
        Auth::OAuth {
            client_id,
            scopes,
            client_secret_env,
        } => {
            let mut object = Map::new();
            object.insert("type".to_owned(), json!("oauth"));
            object.insert("scopes".to_owned(), json!(scopes));
            if let Some(client_id) = client_id {
                object.insert("client_id".to_owned(), json!(client_id));
            }
            if let Some(variable) = client_secret_env {
                object.insert("client_secret_env".to_owned(), json!(variable));
            }
            Value::Object(object)
        }
        Auth::BearerEnv {
            var,
            oauth_supported,
        } => json!({ "type": "bearer_env", "var": var, "oauth_supported": oauth_supported }),
        Auth::HeaderEnv { header, var } => {
            json!({ "type": "header_env", "header": header, "var": var })
        }
        Auth::None => json!({ "type": "none" }),
    }
}

pub(super) fn definition_hash(definition: &Definition) -> String {
    tagged_sha256(DEFINITION_DOMAIN_TAG, &canonical_json(definition, true))
}

pub(super) fn core_hash(definition: &Definition) -> String {
    tagged_sha256(CORE_DOMAIN_TAG, &canonical_json(definition, false))
}

#[cfg(test)]
mod tests {
    use super::super::parse_definition;
    use super::*;

    const GITHUB: &str = include_str!("../../tests/fixtures/definitions/valid/github.toml");

    #[test]
    fn canonical_json_of_the_normative_example() {
        let definition = parse_definition(GITHUB, "github").unwrap();
        assert_eq!(
            canonical_json(&definition, true),
            concat!(
                r#"{"description":"GitHub tools","env":{"GITHUB_TOKEN":{"forward":true},"#,
                r#""LOG_LEVEL":{"value":"info"}},"harness":{"codex":{"cwd":"/srv/github"}},"#,
                r#""name":"github","transport":{"args":["-y","@modelcontextprotocol/server-github@1.2.3"],"#,
                r#""command":"npx","type":"stdio"}}"#
            )
        );
        assert_eq!(
            canonical_json(&definition, false),
            concat!(
                r#"{"env":{"GITHUB_TOKEN":{"forward":true},"LOG_LEVEL":{"value":"info"}},"#,
                r#""name":"github","transport":{"args":["-y","@modelcontextprotocol/server-github@1.2.3"],"#,
                r#""command":"npx","type":"stdio"}}"#
            )
        );
        let hash = definition.definition_hash();
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(hash, definition.core_hash());
    }

    #[test]
    fn integers_and_floats_stay_distinct() {
        let base =
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n[harness.claude]\n";
        let int = parse_definition(&format!("{base}n = 1\n"), "a").unwrap();
        let float = parse_definition(&format!("{base}n = 1.0\n"), "a").unwrap();
        assert_ne!(int.definition_hash(), float.definition_hash());
        assert_eq!(int.core_hash(), float.core_hash());
        assert!(canonical_json(&float, true).contains(r#""n":1.0"#));
    }
}
