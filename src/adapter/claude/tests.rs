//! Unit tests of the Claude Code adapter: declarations, rendering, the
//! credential-name check and the structural read-only guard.

use super::*;
use crate::definition::parse_definition;
use serde_json::json;

fn render(text: &str) -> Rendered {
    let name = text
        .lines()
        .find_map(|line| line.strip_prefix("name = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap();
    ClaudeAdapter.render(&parse_definition(text, name).unwrap())
}

fn object(rendered: &Rendered) -> Value {
    cli::entry_object(&rendered.entry)
}

fn http(auth: &str) -> String {
    format!("name = \"h\"\n[transport]\ntype = \"http\"\nurl = \"https://x.test/mcp\"\n{auth}")
}

#[test]
fn declarations() {
    let adapter = ClaudeAdapter;
    assert_eq!(adapter.harness(), Harness::Claude);
    assert_eq!(adapter.apply_mode(), ApplyMode::HarnessCli);
    assert_eq!(adapter.discover_scopes(), [Scope::User]);
    assert_eq!(adapter.write_scopes(), [Scope::User]);
    assert!(adapter.hashes_comparable());
    assert!(adapter.entries_can_vanish());
    assert!(adapter.reserved_names().contains(&"workspace"));
    assert_eq!(adapter.verified_versions(), ["2.1.310 (Claude Code)"]);
    assert_eq!(adapter.override_keys().len(), 2);
    assert!(adapter.excluded_override_keys().contains(&"headersHelper"));
    assert_eq!(
        adapter.foreign_security_keys(),
        [
            (Harness::Codex, "enabled_tools"),
            (Harness::Codex, "disabled_tools")
        ]
    );
    assert_eq!(adapter.texts().location_variable, "CLAUDE_CONFIG_DIR");
    let read = HarnessRead {
        location: Location {
            scope: Scope::User,
            file: "/c/.claude.json".into(),
            file_exists: false,
            binary_found: false,
        },
        entries: BTreeMap::new(),
        root_supported: true,
        private: PrivateRead::default(),
        shadowed: BTreeMap::new(),
    };
    assert!(adapter.import(&read, "x").is_none());
}

#[test]
fn stdio_rendering_forwards_with_an_empty_default() {
    let rendered = render(
        "name = \"s\"\n[transport]\ntype = \"stdio\"\ncommand = \"/bin/s\"\nargs = [\"-v\"]\n\
         [env]\nTOKEN = { forward = true }\nMODE = { value = \"fast\" }\n\
         [harness.claude]\ntimeout = 30000\n[harness.codex]\ncwd = \"/srv\"\n",
    );
    assert_eq!(
        object(&rendered),
        json!({"command": "/bin/s", "args": ["-v"], "env": {"MODE": "fast", "TOKEN": "${TOKEN:-}"}, "timeout": 30000})
    );
    assert_eq!(rendered.gaps.len(), 1);
    assert_eq!(rendered.gaps[0].field, "harness.codex.cwd");
    assert_eq!(rendered.gaps[0].severity, GapSeverity::Note);
    assert_eq!(rendered.secret_env, None);
    let bare = render("name = \"b\"\n[transport]\ntype = \"stdio\"\ncommand = \"b\"\n");
    assert_eq!(object(&bare), json!({"command": "b"}));
    // Claude Code adds `args = []`; the hash does not change.
    assert_eq!(
        bare.sent_hash,
        entry_hash(&json!({"command": "b", "args": []}))
    );
}

#[test]
fn http_rendering_covers_every_auth_form() {
    let oauth = render(&http(
        "[auth]\ntype = \"oauth\"\nclient_id = \"cid\"\nscopes = [\"write\", \"read\"]\n\
         client_secret_env = \"LINEAR_SECRET\"\n[harness.claude]\noauth_callback_port = 8123\n",
    ));
    assert_eq!(
        object(&oauth),
        json!({"type": "http", "url": "https://x.test/mcp", "oauth": {"clientId": "cid", "scopes": "read write", "callbackPort": 8123}})
    );
    assert_eq!(oauth.secret_env.as_deref(), Some("LINEAR_SECRET"));
    assert!(oauth.gaps.is_empty());
    let default = render(&http(""));
    assert_eq!(
        object(&default),
        json!({"type": "http", "url": "https://x.test/mcp"})
    );
    let bearer = render(&http(
        "[auth]\ntype = \"bearer_env\"\nvar = \"API_KEY\"\noauth_supported = true\n\
         [harness.claude]\noauth_callback_port = 8123\n",
    ));
    assert_eq!(
        object(&bearer)["headers"],
        json!({"Authorization": "Bearer ${API_KEY}"})
    );
    let fields: Vec<&str> = bearer.gaps.iter().map(|gap| gap.field.as_str()).collect();
    assert_eq!(
        fields,
        ["auth.oauth_supported", "harness.claude.oauth_callback_port"]
    );
    let header = render(&http(
        "[auth]\ntype = \"header_env\"\nheader = \"X-Api-Key\"\nvar = \"MY_KEY\"\n",
    ));
    assert_eq!(
        object(&header)["headers"],
        json!({"X-Api-Key": "${MY_KEY}"})
    );
    assert!(header.gaps.is_empty());
    let none = render(&http("[auth]\ntype = \"none\"\n"));
    assert_eq!(none.gaps[0].field, "auth");
    assert_eq!(none.gaps[0].severity, GapSeverity::Note);
    // No rendering carries a secret value, only variable names.
    for rendered in [&oauth, &default, &bearer, &header, &none] {
        let text = object(rendered).to_string();
        assert!(!text.contains("secret"), "{text}");
    }
}

#[test]
fn credential_names_block_remote_headers_only() {
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "HTTPS_PROXY",
        "NPM_TOKEN",
        "ANTHROPIC_OTHER",
        "AWS_ACCESS_KEY_ID",
        "NO_PROXY",
        "https_proxy",
    ] {
        assert!(is_credential_name(name), "{name}");
        for auth in [
            format!("[auth]\ntype = \"bearer_env\"\nvar = \"{name}\"\n"),
            format!("[auth]\ntype = \"header_env\"\nheader = \"X-K\"\nvar = \"{name}\"\n"),
        ] {
            let rendered = render(&http(&auth));
            assert_eq!(rendered.gaps.len(), 1, "{name}");
            assert_eq!(rendered.gaps[0].severity, GapSeverity::Important);
            assert_eq!(rendered.gaps[0].field, "auth.var");
            assert!(rendered.gaps[0].reason.contains(name));
        }
    }
    for name in ["API_KEY", "ANTHROPIC_BASE_URL", "GITHUB_TOKEN", "PROXY_URL"] {
        assert!(!is_credential_name(name), "{name}");
    }
    // stdio forwards are not checked.
    let stdio = render(
        "name = \"s\"\n[transport]\ntype = \"stdio\"\ncommand = \"s\"\n[env]\nNPM_TOKEN = { forward = true }\n",
    );
    assert!(stdio.gaps.is_empty());
}

#[test]
fn reading_hashes_every_key_and_reports_shadows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".claude.json");
    std::fs::write(
        &path,
        r#"{"mcpServers":{"a":{"command":"x","args":[]},"odd":"text"},
            "projects":{"/p":{"mcpServers":{"a":{}}}}}"#,
    )
    .unwrap();
    let location = Location {
        scope: Scope::User,
        file: path.clone(),
        file_exists: true,
        binary_found: false,
    };
    let read = ClaudeAdapter.read(&location).unwrap();
    assert_eq!(read.entries["a"].hash, entry_hash(&json!({"command": "x"})));
    assert!(read.entries["a"].supported_style);
    assert!(!read.entries["odd"].supported_style);
    assert_eq!(read.shadowed["a"], BTreeSet::from(["/p".to_owned()]));
    assert!(read.private.bytes.is_empty());
    std::fs::write(&path, r#"{"mcpServers":[1]}"#).unwrap();
    assert!(!ClaudeAdapter.read(&location).unwrap().root_supported);
}

#[test]
fn locate_follows_the_environment() {
    let temp = tempfile::tempdir().unwrap();
    let env = Environment {
        home: Some(temp.path().as_os_str().to_owned()),
        claude_config_dir: Some(temp.path().join("cfg").into_os_string()),
        ..Environment::default()
    };
    let location = ClaudeAdapter.locate(&env, Scope::User).unwrap();
    assert_eq!(location.file, temp.path().join("cfg/.claude.json"));
    assert!(!location.present());
    assert!(matches!(
        ClaudeAdapter.locate(&Environment::default(), Scope::User),
        Err(Error::HomeUnset)
    ));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let env = Environment {
            claude_config_dir: Some(std::ffi::OsStr::from_bytes(b"/c/\xff").to_owned()),
            ..Environment::default()
        };
        assert_eq!(
            ClaudeAdapter.locate(&env, Scope::User).unwrap_err().code(),
            "harness-path-not-utf8"
        );
    }
}

/// Structural guard: the Claude Code adapter never opens its configuration
/// file for writing. Its sources contain no write API; the only file access
/// is [`ReadOnlyFile`], which opens with read access only.
#[test]
fn the_adapter_never_writes_its_configuration_file() {
    let sources = [
        include_str!("../claude.rs"),
        include_str!("file.rs"),
        include_str!("cli.rs"),
    ];
    // Built at run time, so this test's own source does not match.
    let forbidden = [
        [".write", "(true)"].concat(),
        [".append", "(true)"].concat(),
        [".create", "(true)"].concat(),
        [".truncate", "(true)"].concat(),
        ["File::", "create"].concat(),
        ["fs::", "write"].concat(),
        ["write_", "atomic"].concat(),
        ["fs::", "copy"].concat(),
        ["fs::", "rename"].concat(),
        ["remove_", "file"].concat(),
    ];
    for source in sources {
        // Test modules may write fixtures into their own tempdirs.
        let production = source.split("#[cfg(test)]\nmod tests").next().unwrap();
        for token in &forbidden {
            assert!(!production.contains(token.as_str()), "found `{token}`");
        }
    }
    let file = include_str!("file.rs");
    let production = file.split("#[cfg(test)]\nmod tests").next().unwrap();
    assert_eq!(production.matches("OpenOptions::new()").count(), 1);
    assert!(production.contains("options.read(true);"));
}
