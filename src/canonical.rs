//! Canonical JSON and tagged SHA-256 hashes, shared by the definition hashes
//! and the harness entry hashes.
//!
//! The canonical form has no whitespace and every object's keys sorted
//! byte-wise. Keys are sorted explicitly by [`write_canonical`], so the result
//! does not depend on whether `serde_json`'s `preserve_order` feature is
//! enabled somewhere in the dependency graph. Strings and numbers use
//! `serde_json`'s encoding; floats use the shortest round-trip form.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Writes a JSON value without whitespace and with every object's keys
/// sorted byte-wise.
pub fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (position, (key, item)) in entries.into_iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The canonical JSON text of a value.
#[must_use]
pub fn canonical_string(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

/// SHA-256 over the domain tag, a newline and the body, as 64 lowercase hex
/// characters.
#[must_use]
pub fn tagged_sha256(tag: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tag.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    hex(&hasher.finalize())
}

/// [`tagged_sha256`] over the canonical JSON of a value.
#[must_use]
pub fn tagged_hash(tag: &str, value: &Value) -> String {
    tagged_sha256(tag, &canonical_string(value))
}

/// Lowercase hex of a byte slice.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether a text is a 64-character lowercase hex digest.
#[must_use]
pub fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    #[test]
    fn canonical_writer_sorts_keys_regardless_of_insertion_order() {
        // Build objects whose insertion order is reversed; with
        // `preserve_order` a plain `to_string` would keep that order.
        let mut inner = Map::new();
        inner.insert("z".to_owned(), json!(1));
        inner.insert("a".to_owned(), json!([{"y": 1.5, "b": null}, "s\"q"]));
        let mut outer = Map::new();
        outer.insert("b".to_owned(), Value::Object(inner));
        outer.insert("a".to_owned(), json!(true));
        assert_eq!(
            canonical_string(&Value::Object(outer)),
            r#"{"a":true,"b":{"a":[{"b":null,"y":1.5},"s\"q"],"z":1}}"#
        );
    }

    #[test]
    fn tagged_hashes_depend_on_tag_and_body() {
        let value = json!({"a": 1});
        let one = tagged_hash("t1", &value);
        assert!(is_digest(&one));
        assert_ne!(one, tagged_hash("t2", &value));
        assert_ne!(one, tagged_hash("t1", &json!({"a": 2})));
        assert_eq!(one, tagged_sha256("t1", r#"{"a":1}"#));
        assert!(!is_digest("ABC"));
        assert!(!is_digest(&"A".repeat(64)));
        assert_eq!(hex(&[0, 255]), "00ff");
    }
}
