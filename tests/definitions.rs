//! Definition fixtures and hash properties.
//!
//! Every fixture under `fixtures/definitions/invalid/<rule-code>/` must fail
//! with exactly that rule code, and every fixture under
//! `fixtures/definitions/valid/` must pass. The file stem is the expected
//! server name, exactly as in a source's `servers/` directory.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use d_mcp::definition::{Auth, Definition, Transport, parse_definition};
use d_mcp::rule::Rule;
use proptest::prelude::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/definitions")
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    files
}

fn parse_file(path: &Path) -> Result<Definition, d_mcp::definition::DefinitionError> {
    let stem = path.file_stem().unwrap().to_str().unwrap();
    parse_definition(&fs::read_to_string(path).unwrap(), stem)
}

#[test]
fn valid_fixtures_parse() {
    let files = toml_files(&fixtures().join("valid"));
    assert!(files.len() >= 10);
    for file in files {
        if let Err(error) = parse_file(&file) {
            panic!("{} failed: {error}", file.display());
        }
    }
}

#[test]
fn invalid_fixtures_fail_with_their_directory_code() {
    let mut covered = BTreeSet::new();
    let mut dirs: Vec<PathBuf> = fs::read_dir(fixtures().join("invalid"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    dirs.sort();
    for dir in dirs {
        let code = dir.file_name().unwrap().to_str().unwrap();
        let rule = Rule::from_code(code)
            .unwrap_or_else(|| panic!("fixture directory `{code}` is not a rule code"));
        let files = toml_files(&dir);
        assert!(!files.is_empty(), "{code} has no fixtures");
        for file in files {
            match parse_file(&file) {
                Ok(_) => panic!("{} parsed but should fail with {code}", file.display()),
                Err(error) => assert_eq!(error.rule, rule, "{}: {error}", file.display()),
            }
        }
        covered.insert(rule);
    }
    // Every rule the walker can report has a fixture directory.
    let walker_rules: BTreeSet<Rule> = [
        Rule::TomlSyntax,
        Rule::UnknownKey,
        Rule::MissingField,
        Rule::WrongType,
        Rule::UnsupportedType,
        Rule::InvalidEnvEntry,
        Rule::TransportMismatch,
        Rule::NonFiniteFloat,
        Rule::NameRule,
        Rule::NameMismatch,
        Rule::VariableName,
        Rule::HeaderName,
        Rule::ClientSecretWithoutClientId,
        Rule::CommandRule,
        Rule::UrlInvalid,
        Rule::UrlScheme,
        Rule::UrlQuery,
        Rule::UrlUserinfo,
        Rule::VariableSyntax,
        Rule::SecretName,
        Rule::SecretPrefix,
        Rule::SecretJwt,
        Rule::SecretHex,
        Rule::SecretEntropy,
    ]
    .into();
    assert_eq!(covered, walker_rules);
}

#[test]
fn normative_examples_and_auth_forms_parse() {
    let valid = fixtures().join("valid");
    let github = parse_file(&valid.join("github.toml")).unwrap();
    assert_eq!(github.description.as_deref(), Some("GitHub tools"));
    assert!(github.overrides.contains_key("codex"));
    let auth = |name: &str| match parse_file(&valid.join(format!("{name}.toml")))
        .unwrap()
        .transport
    {
        Transport::Http { auth, .. } => auth,
        Transport::Stdio { .. } => panic!("{name} is not http"),
    };
    assert_eq!(
        auth("linear"),
        Auth::OAuth {
            client_id: Some("d-mcp-linear".into()),
            scopes: vec!["read".into()],
            client_secret_env: Some("LINEAR_CLIENT_SECRET".into()),
        }
    );
    assert_eq!(
        auth("oauth-default"),
        Auth::OAuth {
            client_id: None,
            scopes: vec![],
            client_secret_env: None
        }
    );
    assert_eq!(
        auth("bearer-env"),
        Auth::BearerEnv {
            var: "EXAMPLE_TOKEN".into(),
            oauth_supported: true
        }
    );
    assert_eq!(
        auth("header-env"),
        Auth::HeaderEnv {
            header: "X-Api-Key".into(),
            var: "EXAMPLE_API_KEY".into()
        }
    );
    assert_eq!(auth("auth-none"), Auth::None);
    let defaults = parse_file(&valid.join("explicit-defaults.toml")).unwrap();
    assert!(defaults.overrides.is_empty());
}

#[test]
fn errors_carry_position_and_field() {
    let path = fixtures().join("invalid/secret-prefix/flag-value.toml");
    let error = parse_file(&path).unwrap_err();
    assert_eq!(error.field, "transport.args[0]");
    assert_eq!(error.line, 5);
    assert!(error.column > 1);
    let error =
        parse_file(&fixtures().join("invalid/unknown-key/secret-like-key.toml")).unwrap_err();
    assert_eq!(error.field, "<redacted>");
    assert!(!error.to_string().contains("CANARY"));
}

// --- Hash properties ------------------------------------------------------

/// A generated definition, rendered to TOML in several equivalent styles.
#[derive(Debug, Clone)]
struct Model {
    name: String,
    description: Option<String>,
    kind: Kind,
    codex: Vec<(String, i64)>,
}

#[derive(Debug, Clone)]
enum Kind {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, Option<String>)>,
    },
    Http {
        url_path: String,
        scopes: Vec<String>,
        client_id: Option<String>,
    },
}

/// Rendering options that must not change either hash.
#[derive(Debug, Clone)]
struct Style {
    reverse_keys: bool,
    comments: bool,
    inline_transport: bool,
    explicit_defaults: bool,
    shuffle_scopes: bool,
}

fn word() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]{0,7}"
}

fn upper() -> impl Strategy<Value = String> {
    "[A-Z][A-Z0-9]{0,6}".prop_filter("not a secret name", |name| {
        !d_mcp::credential::secret_name_heuristic(name)
    })
}

fn model() -> impl Strategy<Value = Model> {
    let stdio = (
        word(),
        prop::collection::vec(word(), 0..4),
        prop::collection::btree_map(upper(), prop::option::of(word()), 0..3),
    )
        .prop_map(|(command, args, env)| Kind::Stdio {
            command,
            args,
            env: env.into_iter().collect(),
        });
    let http = (
        word(),
        prop::collection::btree_set(word(), 0..4),
        prop::option::of(word()),
    )
        .prop_map(|(url_path, scopes, client_id)| Kind::Http {
            url_path,
            scopes: scopes.into_iter().collect(),
            client_id,
        });
    (
        word(),
        prop::option::of(word()),
        prop_oneof![stdio, http],
        prop::collection::btree_map(word(), any::<i64>(), 0..3),
    )
        .prop_map(|(name, description, kind, codex)| Model {
            name,
            description,
            kind,
            codex: codex.into_iter().collect(),
        })
}

fn style() -> impl Strategy<Value = Style> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(reverse_keys, comments, inline_transport, explicit_defaults, shuffle_scopes)| Style {
                reverse_keys,
                comments,
                inline_transport,
                explicit_defaults,
                shuffle_scopes,
            },
        )
}

fn quote(text: &str) -> String {
    format!("\"{text}\"")
}

fn list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| quote(item)).collect();
    format!("[{}]", quoted.join(", "))
}

fn render(model: &Model, style: &Style) -> String {
    let order = |mut lines: Vec<String>| {
        if style.reverse_keys {
            lines.reverse();
        }
        lines
    };
    let comment = if style.comments { "  # note\n" } else { "\n" };
    let mut root = vec![format!("name = {}", quote(&model.name))];
    if let Some(description) = &model.description {
        root.push(format!("description = {}", quote(description)));
    }
    let mut transport = Vec::new();
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    match &model.kind {
        Kind::Stdio { command, args, env } => {
            transport.push("type = \"stdio\"".to_owned());
            transport.push(format!("command = {}", quote(command)));
            if !args.is_empty() || style.explicit_defaults {
                transport.push(format!("args = {}", list(args)));
            }
            if !env.is_empty() || style.explicit_defaults {
                let entries = order(
                    env.iter()
                        .map(|(key, value)| match value {
                            None => format!("{key} = {{ forward = true }}"),
                            Some(literal) => format!("{key} = {{ value = {} }}", quote(literal)),
                        })
                        .collect(),
                );
                tables.push(("env".to_owned(), entries));
            }
        }
        Kind::Http {
            url_path,
            scopes,
            client_id,
        } => {
            transport.push("type = \"http\"".to_owned());
            transport.push(format!("url = \"https://mcp.example.com/{url_path}\""));
            let mut auth = Vec::new();
            if style.explicit_defaults || !scopes.is_empty() || client_id.is_some() {
                auth.push("type = \"oauth\"".to_owned());
                if !scopes.is_empty() || style.explicit_defaults {
                    let mut scopes = scopes.clone();
                    if style.shuffle_scopes && !scopes.is_empty() {
                        scopes.reverse();
                        scopes.push(scopes[0].clone());
                    }
                    auth.push(format!("scopes = {}", list(&scopes)));
                }
                if let Some(client_id) = client_id {
                    auth.push(format!("client_id = {}", quote(client_id)));
                }
                tables.push(("auth".to_owned(), order(auth)));
            }
        }
    }
    if !model.codex.is_empty() || style.explicit_defaults {
        let entries = order(
            model
                .codex
                .iter()
                .map(|(key, value)| format!("{key} = {value}"))
                .collect(),
        );
        tables.push(("harness.codex".to_owned(), entries));
    }
    if style.explicit_defaults {
        tables.push(("harness.claude".to_owned(), Vec::new()));
    }
    let transport = order(transport);
    let mut text = String::new();
    if style.comments {
        text.push_str("# generated\n");
    }
    if style.inline_transport {
        root.push(format!("transport = {{ {} }}", transport.join(", ")));
    }
    for line in order(root) {
        text.push_str(&line);
        text.push_str(comment);
    }
    if !style.inline_transport {
        text.push_str("\n[transport]\n");
        for line in transport {
            text.push_str(&line);
            text.push_str(comment);
        }
    }
    for (header, entries) in order_tables(tables, style.reverse_keys) {
        text.push_str(&format!("\n[{header}]{comment}"));
        for line in entries {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

fn order_tables(
    mut tables: Vec<(String, Vec<String>)>,
    reverse: bool,
) -> Vec<(String, Vec<String>)> {
    if reverse {
        tables.reverse();
    }
    tables
}

fn canonical_style() -> Style {
    Style {
        reverse_keys: false,
        comments: false,
        inline_transport: false,
        explicit_defaults: false,
        shuffle_scopes: false,
    }
}

fn parse_model(model: &Model, style: &Style) -> Definition {
    let text = render(model, style);
    parse_definition(&text, &model.name).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

/// A single-field mutation of a model.
fn mutate(model: &Model, choice: usize) -> Option<Model> {
    let mut changed = model.clone();
    match choice % 6 {
        0 => changed.name.push('x'),
        1 => match &mut changed.kind {
            Kind::Stdio { command, .. } => command.push('x'),
            Kind::Http { url_path, .. } => url_path.push('x'),
        },
        2 => match &mut changed.kind {
            Kind::Stdio { args, .. } => args.push("extra".to_owned()),
            Kind::Http { scopes, .. } => {
                if scopes.contains(&"extra".to_owned()) {
                    return None;
                }
                scopes.push("extra".to_owned());
            }
        },
        3 => match &mut changed.kind {
            Kind::Stdio { env, .. } => match env.first_mut() {
                Some((_, value @ None)) => *value = Some("v".to_owned()),
                Some((_, value)) => *value = None,
                None => env.push(("NEWVAR".to_owned(), None)),
            },
            Kind::Http { client_id, .. } => match client_id {
                Some(id) => id.push('x'),
                None => *client_id = Some("client".to_owned()),
            },
        },
        4 => match &mut changed.description {
            Some(description) => description.push('x'),
            None => changed.description = Some("about".to_owned()),
        },
        _ => match changed.codex.first_mut() {
            Some((_, value)) => *value = value.wrapping_add(1),
            None => changed.codex.push(("knob".to_owned(), 1)),
        },
    }
    Some(changed)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn formatting_and_explicit_defaults_keep_both_hashes(model in model(), style in style()) {
        let canonical = parse_model(&model, &canonical_style());
        let styled = parse_model(&model, &style);
        prop_assert_eq!(canonical.definition_hash(), styled.definition_hash());
        prop_assert_eq!(canonical.core_hash(), styled.core_hash());
        prop_assert_eq!(canonical, styled);
    }

    #[test]
    fn any_field_change_changes_definition_hash(model in model(), choice in 0usize..6) {
        let Some(changed) = mutate(&model, choice) else {
            return Ok(());
        };
        let before = parse_model(&model, &canonical_style());
        let after = parse_model(&changed, &canonical_style());
        prop_assert_ne!(before.definition_hash(), after.definition_hash());
        // Description and override changes leave the core hash alone; every
        // other field changes it.
        if choice % 6 >= 4 {
            prop_assert_eq!(before.core_hash(), after.core_hash());
        } else {
            prop_assert_ne!(before.core_hash(), after.core_hash());
        }
    }
}
