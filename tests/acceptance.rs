//! Acceptance-criteria gap tests: heuristic boundaries, hash semantics for
//! every auth form and formatting style, the canary guarantee across the
//! whole invalid-fixture matrix, and the documentation deliverables.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use d_mcp::credential::{SecretFinding, secret_name_heuristic, secret_value_heuristic};
use d_mcp::definition::{Definition, parse_definition};
use d_mcp::rule::Rule;

const REFERENCE: &str = include_str!("../docs/reference.md");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/definitions")
}

fn parse(text: &str, name: &str) -> Definition {
    parse_definition(text, name).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

// --- Heuristic rules ------------------------------------------------------

#[test]
fn every_known_prefix_is_detected_at_token_start() {
    for prefix in [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "sk-",
        "sk-ant-",
        "xoxa-",
        "xoxb-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "AKIA",
        "AIza",
    ] {
        assert_eq!(
            secret_value_heuristic(&format!("{prefix}x1")),
            Some(SecretFinding::Prefix),
            "{prefix}"
        );
        // Not at the token start: no prefix match.
        assert_ne!(
            secret_value_heuristic(&format!("a{prefix}x1")),
            Some(SecretFinding::Prefix),
            "mid-token {prefix}"
        );
    }
}

#[test]
fn every_secret_name_word_is_detected_per_segment() {
    for word in [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PASS",
        "APIKEY",
        "CREDENTIAL",
        "CREDENTIALS",
        "COOKIE",
        "AUTH",
    ] {
        for name in [
            format!("X_{word}"),
            format!("{word}-X"),
            word.to_lowercase(),
        ] {
            assert!(secret_name_heuristic(&name), "{name}");
        }
    }
    assert!(secret_name_heuristic("api_key"));
    assert!(secret_name_heuristic("X-API-KEY-Y"));
    assert!(!secret_name_heuristic("API_X_KEY"));
}

#[test]
fn length_jwt_and_entropy_boundaries() {
    // Hex: 31 characters pass, 32 are rejected.
    assert_eq!(secret_value_heuristic(&"a1".repeat(15)), None);
    assert_eq!(
        secret_value_heuristic(&"a1".repeat(16)),
        Some(SecretFinding::Hex)
    );
    assert_eq!(
        secret_value_heuristic(&"A1".repeat(16)),
        Some(SecretFinding::Hex)
    );
    // JWT: needs two separators.
    assert_eq!(secret_value_heuristic("eyJhbGciOi.payload"), None);
    assert_eq!(
        secret_value_heuristic("eyJhbGciOi.payload.sig"),
        Some(SecretFinding::Jwt)
    );
    // Entropy: 31 distinct characters pass, 32 are rejected.
    let distinct = "ghijklmnopqrstuvwxyzGHIJKLMNOPQRSTUVWXYZ";
    assert_eq!(secret_value_heuristic(&distinct[..31]), None);
    assert_eq!(
        secret_value_heuristic(&distinct[..32]),
        Some(SecretFinding::Entropy)
    );
    // Exactly 4.0 bits (16 symbols, uniform) is rejected; 3.0 bits passes.
    assert_eq!(
        secret_value_heuristic(&"ghijklmnopqrstuv".repeat(2)),
        Some(SecretFinding::Entropy)
    );
    assert_eq!(secret_value_heuristic(&"ghijklmn".repeat(4)), None);
    assert_eq!(secret_value_heuristic(&"z".repeat(64)), None);
    // Entropy counts Unicode scalars, not bytes.
    let scalars: String = ('\u{4e00}'..).take(32).collect();
    assert_eq!(
        secret_value_heuristic(&scalars),
        Some(SecretFinding::Entropy)
    );
}

// --- Hash semantics -------------------------------------------------------

fn http(auth: &str) -> String {
    format!("name = \"h\"\n[transport]\ntype = \"http\"\nurl = \"https://x.test/mcp\"\n{auth}")
}

#[test]
fn every_semantic_field_of_every_auth_form_changes_both_hashes() {
    let variants = [
        http(""),
        http("[auth]\ntype = \"none\"\n"),
        http("[auth]\ntype = \"oauth\"\nscopes = [\"a\"]\n"),
        http("[auth]\ntype = \"oauth\"\nscopes = [\"a\", \"b\"]\n"),
        http("[auth]\ntype = \"oauth\"\nclient_id = \"c\"\n"),
        http("[auth]\ntype = \"oauth\"\nclient_id = \"d\"\n"),
        http("[auth]\ntype = \"oauth\"\nclient_id = \"c\"\nclient_secret_env = \"S\"\n"),
        http("[auth]\ntype = \"oauth\"\nclient_id = \"c\"\nclient_secret_env = \"T\"\n"),
        http("[auth]\ntype = \"bearer_env\"\nvar = \"A\"\n"),
        http("[auth]\ntype = \"bearer_env\"\nvar = \"B\"\n"),
        http("[auth]\ntype = \"bearer_env\"\nvar = \"A\"\noauth_supported = true\n"),
        http("[auth]\ntype = \"header_env\"\nheader = \"X-A\"\nvar = \"A\"\n"),
        http("[auth]\ntype = \"header_env\"\nheader = \"X-B\"\nvar = \"A\"\n"),
        http("[auth]\ntype = \"header_env\"\nheader = \"X-A\"\nvar = \"B\"\n"),
        "name = \"h\"\n[transport]\ntype = \"http\"\nurl = \"https://x.test/other\"\n".to_owned(),
        "name = \"h\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n".to_owned(),
        "name = \"h\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\nargs = [\"a\", \"b\"]\n"
            .to_owned(),
        "name = \"h\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\nargs = [\"b\", \"a\"]\n"
            .to_owned(),
        "name = \"h\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n[env]\nV = { forward = true }\n"
            .to_owned(),
        "name = \"h\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n[env]\nV = { value = \"1\" }\n"
            .to_owned(),
        "name = \"h\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n[env]\nW = { value = \"1\" }\n"
            .to_owned(),
    ];
    let definitions: BTreeSet<String> = variants
        .iter()
        .map(|text| parse(text, "h").definition_hash())
        .collect();
    let cores: BTreeSet<String> = variants
        .iter()
        .map(|text| parse(text, "h").core_hash())
        .collect();
    assert_eq!(definitions.len(), variants.len());
    assert_eq!(cores.len(), variants.len());
}

#[test]
fn explicit_defaults_and_reformatting_keep_both_hashes() {
    let plain = parse(&http(""), "h");
    for text in [
        "name = \"h\"\nauth = { type = \"oauth\" }\n[transport]\ntype = \"http\"\nurl = \"https://x.test/mcp\"\n"
            .to_owned(),
        http("[auth]\ntype = \"oauth\"\nscopes = []\n"),
        http("[auth]\ntype = \"oauth\"\nscopes = []\n[harness.codex]\n[harness.claude]\n"),
        // Whitespace, CRLF, comments, literal strings, indentation.
        "# c\r\nname   =   'h'  # trailing\r\n\r\n[ transport ]\r\n  type = 'http'\r\n\turl = \"https://x.test/mcp\"\r\n"
            .to_owned(),
        "name = \"h\"\ntransport.type = \"http\"\ntransport.url = \"https://x.test/mcp\"\n"
            .to_owned(),
    ] {
        let other = parse(&text, "h");
        assert_eq!(plain.definition_hash(), other.definition_hash(), "{text}");
        assert_eq!(plain.core_hash(), other.core_hash(), "{text}");
    }
    let bearer = parse(&http("[auth]\ntype = \"bearer_env\"\nvar = \"A\"\n"), "h");
    let explicit = parse(
        &http("[auth]\ntype = \"bearer_env\"\nvar = \"A\"\noauth_supported = false\n"),
        "h",
    );
    assert_eq!(bearer.definition_hash(), explicit.definition_hash());
    let stdio = parse(
        "name = \"s\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n",
        "s",
    );
    let explicit = parse(
        "name = \"s\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\nargs = []\n[env]\n",
        "s",
    );
    assert_eq!(stdio.definition_hash(), explicit.definition_hash());
    // `scopes` order and duplicates do not matter.
    let a = parse(
        &http("[auth]\ntype = \"oauth\"\nscopes = [\"b\", \"a\", \"b\"]\n"),
        "h",
    );
    let b = parse(
        &http("[auth]\ntype = \"oauth\"\nscopes = [\"a\", \"b\"]\n"),
        "h",
    );
    assert_eq!(a.definition_hash(), b.definition_hash());
    assert_eq!(a.core_hash(), b.core_hash());
}

#[test]
fn description_and_overrides_change_only_the_definition_hash() {
    let base = "name = \"s\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n";
    let plain = parse(base, "s");
    for extra in [
        "description = \"d\"\n",
        "[harness.codex]\nk = 1\n",
        "[harness.claude]\nk = \"v\"\n[harness.claude.sub]\nx = [1, 2]\n",
    ] {
        let text = format!("{extra}{base}");
        let text = if extra.starts_with('[') {
            format!("{base}{extra}")
        } else {
            text
        };
        let other = parse(&text, "s");
        assert_ne!(plain.definition_hash(), other.definition_hash(), "{extra}");
        assert_eq!(plain.core_hash(), other.core_hash(), "{extra}");
    }
    // Nested override key order is irrelevant.
    let a = parse(
        &format!("{base}[harness.codex]\na = 1\nb = {{ x = 1, y = 2 }}\n"),
        "s",
    );
    let b = parse(
        &format!("{base}[harness.codex]\nb = {{ y = 2, x = 1 }}\na = 1\n"),
        "s",
    );
    assert_eq!(a.definition_hash(), b.definition_hash());
    // Array order inside an override is significant.
    let c = parse(&format!("{base}[harness.codex]\nt = [1, 2]\n"), "s");
    let d = parse(&format!("{base}[harness.codex]\nt = [2, 1]\n"), "s");
    assert_ne!(c.definition_hash(), d.definition_hash());
}

#[test]
fn equal_datetimes_hash_equally_in_every_rfc3339_spelling() {
    let base = "name = \"s\"\n[transport]\ntype = \"stdio\"\ncommand = \"c\"\n[harness.codex]\n";
    let canonical = parse(&format!("{base}t = 1979-05-27T07:32:00Z\n"), "s");
    for spelling in [
        "1979-05-27T07:32:00Z",
        "1979-05-27 07:32:00Z",
        "1979-05-27t07:32:00z",
        "1979-05-27T07:32:00+00:00",
        "1979-05-27 07:32:00+00:00",
    ] {
        let other = parse(&format!("{base}t = {spelling}\n"), "s");
        assert_eq!(
            canonical.definition_hash(),
            other.definition_hash(),
            "{spelling}"
        );
    }
    let later = parse(&format!("{base}t = 1979-05-27T07:32:01Z\n"), "s");
    assert_ne!(canonical.definition_hash(), later.definition_hash());
    // RFC 3339 gives `-00:00` its own meaning (unknown local offset).
    let unknown = parse(&format!("{base}t = 1979-05-27T07:32:00-00:00\n"), "s");
    assert_ne!(canonical.definition_hash(), unknown.definition_hash());
}

// --- Canary across the fixture matrix ---------------------------------------

/// Every double-quoted string of at least 8 characters in a fixture.
fn long_strings(text: &str) -> Vec<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .filter(|piece| piece.chars().count() >= 8)
        // Plain lowercase words may legitimately be field names.
        .filter(|piece| {
            piece
                .chars()
                .any(|c| c.is_ascii_digit() || c.is_ascii_uppercase() || ":/@?=$".contains(c))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn rejection_messages_never_echo_fixture_values() {
    let mut checked = 0;
    for dir in fs::read_dir(fixtures().join("invalid")).unwrap() {
        for file in fs::read_dir(dir.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            let text = fs::read_to_string(&path).unwrap();
            let stem = path.file_stem().unwrap().to_str().unwrap();
            let error = parse_definition(&text, stem).unwrap_err();
            let message = error.to_string();
            for value in long_strings(&text) {
                assert!(
                    !message.contains(&value),
                    "{} echoes `{value}`: {message}",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 90);
}

// --- Documentation deliverables ---------------------------------------------

#[test]
fn reference_documents_every_rule_and_adrs_exist() {
    for rule in Rule::ALL {
        assert!(
            REFERENCE.contains(&format!("| `{}` |", rule.code())),
            "rule `{}` is not documented",
            rule.code()
        );
    }
    let adr = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/adr");
    for name in [
        "0001-store-centred-management.md",
        "0002-secret-references-only.md",
        "0003-neutral-core-plus-overrides.md",
        "0004-oauth-first.md",
    ] {
        let text = fs::read_to_string(adr.join(name)).unwrap_or_else(|_| panic!("{name} missing"));
        assert!(text.lines().next().unwrap().starts_with('#'), "{name}");
        assert!(text.len() > 200, "{name} is a stub");
    }
}
