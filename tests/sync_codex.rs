//! Integration tests of `sync` and `unmanage` with the Codex adapter.
//!
//! Every test runs in an isolated `TestEnv` with `CODEX_HOME` in a tempdir.
//! Most tests control `PATH` (`without_codex`), so a `codex` binary on the
//! developer's machine never changes the outcome; Codex counts as present
//! through its config file instead.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{TestEnv, snapshot};
use d_mcp::adapter::codex::CodexAdapter;
use d_mcp::adapter::{Adapter, Hooks, LoginState, Scope, registry};
use d_mcp::definition::{parse_definition, parse_definition_with_sites};
use d_mcp::error::Error;
use d_mcp::process::{Invocation, ProcessRunner, RunError};
use d_mcp::rule::Rule;
use d_mcp::store::StorePaths;
use d_mcp::sync::{SyncHooks, SyncOptions, sync};
use d_mcp::validate::validate_definition;
use serde_json::Value;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> String {
    fs::read_to_string(format!("{FIXTURES}/codex/{name}")).expect("read fixture")
}

/// An initialized environment without a `codex` binary on `PATH`.
fn setup() -> TestEnv {
    let env = TestEnv::new().without_codex();
    env.init();
    env
}

fn write_config(env: &TestEnv, text: &str) {
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), text).unwrap();
}

fn config(env: &TestEnv) -> String {
    fs::read_to_string(env.codex_config()).unwrap()
}

fn state(env: &TestEnv) -> String {
    fs::read_to_string(env.default_store().join("state.toml")).unwrap_or_default()
}

fn select(env: &TestEnv, name: &str) {
    env.cmd()
        .args(["select", name, "--harness", "codex"])
        .assert()
        .success();
}

fn sync_json(env: &TestEnv, args: &[&str]) -> (i32, Value, String) {
    let mut all = vec!["sync"];
    all.extend_from_slice(args);
    env.json(&all)
}

fn harness(value: &Value) -> &Value {
    &value["data"]["harnesses"][0]
}

fn entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    harness(value)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no entry `{name}` in {value:#}"))
}

fn op(value: &Value, name: &str) -> String {
    entry(value, name)["operation"].as_str().unwrap().to_owned()
}

/// `original` followed by the separator d-mcp inserts before appended
/// entries.
fn with_separator(original: &str) -> String {
    let eol = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = original.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    if !out.is_empty() && !out.ends_with(&format!("{eol}{eol}")) {
        out.push_str(eol);
    }
    out
}

const NEW: &str = "name = \"dmcp-new\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n\
                   args = [\"-y\", \"pkg@1\"]\n[env]\nTOKEN_VAR = { forward = true }\n";

#[test]
fn every_fixture_keeps_all_bytes_outside_the_managed_subtree() {
    for name in [
        "comments.toml",
        "crlf.toml",
        "no-trailing-newline.toml",
        "last-table.toml",
        "subtables.toml",
        "foreign-canaries.toml",
    ] {
        let env = setup();
        let original = fixture(name);
        write_config(&env, &original);
        env.write_local("dmcp-new", NEW);
        select(&env, "dmcp-new");
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{name}: {value:#}");
        assert_eq!(op(&value, "dmcp-new"), "add", "{name}");
        let added = config(&env);
        let prefix = with_separator(&original);
        assert!(added.starts_with(&prefix), "{name}: {added:?}");
        let eol = if original.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let expected_entry =
            "[mcp_servers.dmcp-new]\ncommand = \"npx\"\nargs = [\"-y\", \"pkg@1\"]\n\
                              env_vars = [\"TOKEN_VAR\"]\n"
                .replace('\n', eol);
        assert_eq!(&added[prefix.len()..], expected_entry, "{name}");

        // A second sync right after the first plans only Unchanged.
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0);
        assert_eq!(op(&value, "dmcp-new"), "unchanged", "{name}");
        assert_eq!(config(&env), added);

        env.cmd().args(["unselect", "dmcp-new"]).assert().success();
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0);
        assert_eq!(op(&value, "dmcp-new"), "remove", "{name}");
        assert_eq!(config(&env), prefix, "{name}");
        assert!(!state(&env).contains("dmcp-new"));
    }
}

/// The local `github` definition used by the take-over tests.
const GITHUB: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n\
                      [env]\nLOG_LEVEL = { value = \"info\" }\n";

#[test]
fn an_update_in_the_middle_keeps_comments_and_neighbours() {
    let env = setup();
    let original = fixture("comments.toml");
    write_config(&env, &original);
    env.write_local("github", GITHUB);
    select(&env, "github");

    // The hand-written `github` entry is a collision: reported, untouched.
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "sync-blocked");
    assert_eq!(op(&value, "github"), "collision");
    assert_eq!(entry(&value, "github")["hash"], Value::Null);
    assert_eq!(config(&env), original);
    assert!(!state(&env).contains("github"));

    // The take-over accepts it and updates it in the same run.
    let (code, value, _) = sync_json(&env, &["--take-over", "github"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "update");
    assert!(
        entry(&value, "github")["notice"]
            .as_str()
            .unwrap()
            .contains("taken over")
    );
    let updated = config(&env);
    let header = original.find("[mcp_servers.github]").unwrap();
    let next = original
        .find("\n# Comment above the next foreign table.")
        .unwrap();
    assert_eq!(&updated[..header], &original[..header]);
    assert!(updated.ends_with(&original[next..]));
    assert_eq!(
        &updated[header..updated.len() - (original.len() - next)],
        "[mcp_servers.github]\ncommand = \"npx\"\n\n[mcp_servers.github.env]\nLOG_LEVEL = \"info\"\n"
    );

    // Remove keeps the comment above the header.
    env.cmd().args(["unselect", "github"]).assert().success();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "remove");
    let removed = config(&env);
    assert_eq!(
        removed,
        format!("{}{}", &original[..header], &original[next..])
    );
    assert!(removed.contains("# Comment directly above a header d-mcp will manage"));
}

#[test]
fn drift_is_reported_and_never_modified_until_taken_over() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    assert_eq!(sync_json(&env, &[]).0, 0);
    let state_before = state(&env);
    let edited = config(&env).replace("command = \"npx\"", "command = \"edited\"");
    fs::write(env.codex_config(), &edited).unwrap();
    // A definition change does not override drift either.
    env.write_local("github", &GITHUB.replace("info", "debug"));
    for _ in 0..2 {
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 1);
        assert_eq!(op(&value, "github"), "drift");
        assert_eq!(entry(&value, "github")["outcome"], "blocked");
        assert!(
            entry(&value, "github")["notice"]
                .as_str()
                .unwrap()
                .contains("--take-over github")
        );
        assert_eq!(config(&env), edited);
        assert_eq!(state(&env), state_before);
    }
    let (code, value, _) = sync_json(&env, &["--take-over", "github"]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "update");
    assert!(config(&env).contains("LOG_LEVEL = \"debug\""));
    assert!(config(&env).contains("command = \"npx\""));
    assert_eq!(op(&sync_json(&env, &[]).1, "github"), "unchanged");
}

#[test]
fn an_identical_collision_is_taken_over_without_a_write() {
    let env = setup();
    let text = "[mcp_servers.github]\ncommand = \"npx\"\n\n[mcp_servers.github.env]\nLOG_LEVEL = \"info\"\n";
    // The same content in another style is identical too.
    let inline =
        "[mcp_servers.github]\nargs = []\ncommand = \"npx\"\nenv = { LOG_LEVEL = \"info\" }\n";
    for existing in [text, inline] {
        let env_case = setup();
        write_config(&env_case, existing);
        env_case.write_local("github", GITHUB);
        select(&env_case, "github");
        let (code, value, _) = sync_json(&env_case, &[]);
        assert_eq!(code, 1);
        assert_eq!(op(&value, "github"), "identical-collision");
        assert_eq!(entry(&value, "github")["hash"], Value::Null);
        let (code, value, _) = sync_json(&env_case, &["--take-over", "github"]);
        assert_eq!(code, 0, "{value:#}");
        assert_eq!(op(&value, "github"), "unchanged");
        assert_eq!(entry(&value, "github")["outcome"], "recorded");
        assert_eq!(config(&env_case), existing);
        assert!(state(&env_case).contains("name = \"github\""));
    }
    drop(env);
}

#[test]
fn update_changes_the_sent_hash_and_a_definition_only_change_refreshes() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let first = state(&env);
    env.write_local("github", &GITHUB.replace("info", "debug"));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "update");
    assert_eq!(entry(&value, "github")["outcome"], "applied");
    let second = state(&env);
    assert_ne!(first, second);
    assert!(config(&env).contains("LOG_LEVEL = \"debug\""));
    // A description change alters only the definition hash.
    env.write_local(
        "github",
        &GITHUB
            .replace("info", "debug")
            .replace("[transport]", "description = \"GitHub\"\n[transport]"),
    );
    let before = config(&env);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "unchanged");
    assert_eq!(entry(&value, "github")["outcome"], "recorded");
    assert_eq!(config(&env), before);
    assert_ne!(state(&env), second);
}

#[test]
fn vanished_entries_are_re_added_with_a_notice() {
    let env = setup();
    write_config(&env, "model = \"o3\"\n");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    fs::write(env.codex_config(), "model = \"o3\"\n").unwrap();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "vanished");
    assert_eq!(entry(&value, "github")["outcome"], "applied");
    let notice = entry(&value, "github")["notice"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(notice.contains("removed outside d-mcp"), "{notice}");
    assert!(
        notice.contains("d-mcp unmanage github --harness codex"),
        "{notice}"
    );
    assert!(config(&env).contains("[mcp_servers.github]"));
    assert_eq!(op(&sync_json(&env, &[]).1, "github"), "unchanged");
}

#[test]
fn held_servers_keep_their_entries_and_resolve_agrees() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let written = config(&env);
    // An invalid Codex override invalidates the whole definition.
    env.write_local("github", &format!("{GITHUB}[harness.codex]\nknob = 1\n"));
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    let server = &value["data"]["servers"][0];
    assert_eq!(server["status"], "hold");
    assert_eq!(server["reason"], "invalid-winner");
    let problem = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|problem| problem["code"] == "invalid-winner")
        .unwrap();
    assert_eq!(problem["rule"], "override-key");
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "held");
    assert_eq!(entry(&value, "github")["reason"], "invalid-winner");
    assert_eq!(entry(&value, "github")["outcome"], "kept");
    assert!(
        value["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == "invalid-winner")
    );
    assert_eq!(config(&env), written);

    // A recorded, still-selected server that no source offers is held too.
    fs::remove_file(env.default_store().join("local/servers/github.toml")).unwrap();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "held");
    assert_eq!(entry(&value, "github")["reason"], "not-offered");
    assert_eq!(config(&env), written);

    // A held server without a record plans nothing.
    select(&env, "nothing");
    let (_, value, _) = sync_json(&env, &[]);
    assert_eq!(harness(&value)["entries"].as_array().unwrap().len(), 1);
}

#[test]
fn orphaned_records_are_dropped() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    fs::write(env.codex_config(), "").unwrap();
    env.cmd().args(["unselect", "github"]).assert().success();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "orphaned");
    assert_eq!(entry(&value, "github")["outcome"], "recorded");
    assert!(!state(&env).contains("github"));
    assert_eq!(config(&env), "");
}

#[test]
fn a_changed_codex_home_is_a_location_change_until_relocate() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let old = config(&env);
    let other = env.root().join("codex-other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("config.toml"), "").unwrap();
    let run = |args: &[&str]| {
        let output = env
            .cmd()
            .env("CODEX_HOME", &other)
            .arg("--json")
            .arg("sync")
            .args(args)
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.code().unwrap(), value)
    };
    let (code, value) = run(&[]);
    assert_eq!(code, 1);
    assert_eq!(harness(&value)["status"], "location-changed");
    assert_eq!(op(&value, "github"), "location-changed");
    assert_eq!(fs::read_to_string(other.join("config.toml")).unwrap(), "");
    let (code, value) = run(&["--relocate"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "vanished");
    let notices = harness(&value)["notices"].as_array().unwrap();
    assert!(
        notices
            .iter()
            .any(|n| n.as_str().unwrap().contains("still contains"))
    );
    assert!(
        fs::read_to_string(other.join("config.toml"))
            .unwrap()
            .contains("[mcp_servers.github]")
    );
    // The old file is never edited.
    assert_eq!(config(&env), old);
    let other_text = other.to_string_lossy();
    assert!(state(&env).contains(other_text.as_ref()));
}

#[test]
fn unsupported_styles_and_roots_are_left_untouched() {
    for root in ["inline-root.toml", "dotted-root.toml"] {
        let env = setup();
        let original = fixture(root);
        write_config(&env, &original);
        env.write_local("github", GITHUB);
        select(&env, "github");
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 1, "{root}");
        assert_eq!(harness(&value)["status"], "unsupported");
        assert_eq!(value["error"]["code"], "sync-blocked");
        assert_eq!(config(&env), original);
    }
    // An owned entry turned into an inline table.
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let inline = "[mcp_servers]\ngithub = { command = \"npx\", args = [], env = { LOG_LEVEL = \"info\" } }\n";
    fs::write(env.codex_config(), inline).unwrap();
    assert_eq!(op(&sync_json(&env, &[]).1, "github"), "unchanged");
    env.write_local("github", &GITHUB.replace("info", "debug"));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "github"), "unsupported");
    assert_eq!(config(&env), inline);
}

#[test]
fn invalid_configs_abort_with_exit_3_and_never_echo_the_line() {
    let env = setup();
    let original = fixture("invalid.toml");
    write_config(&env, &original);
    env.write_local("github", GITHUB);
    select(&env, "github");
    let (code, value, stderr) = sync_json(&env, &[]);
    assert_eq!(code, 3);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "harness-config-invalid");
    assert_eq!(harness(&value)["status"], "failed");
    assert!(value["error"]["hint"].as_str().unwrap().contains("line"));
    let text = format!("{value}{stderr}");
    assert!(!text.contains("CANARY"), "{text}");
    assert_eq!(config(&env), original);
    let (code, stdout, stderr) = env.text(&["sync"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("[harness-config-invalid]"));
    assert!(!format!("{stdout}{stderr}").contains("CANARY"));
    // Non-UTF-8 and non-regular files.
    fs::write(env.codex_config(), b"\xff\xfe").unwrap();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3);
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("not UTF-8")
    );
    fs::remove_file(env.codex_config()).unwrap();
    fs::create_dir(env.codex_config()).unwrap();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3);
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("not a regular file")
    );
}

#[test]
fn foreign_canaries_and_hashes_never_reach_output_or_store() {
    let env = setup();
    write_config(&env, &fixture("foreign-canaries.toml"));
    env.write_local("github", GITHUB);
    select(&env, "github");
    env.write_local(
        "foreign-http",
        "name = \"foreign-http\"\n[transport]\ntype = \"http\"\nurl = \"https://x.example/mcp\"\n",
    );
    select(&env, "foreign-http");
    let (code, json, json_err) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&json, "foreign-http"), "collision");
    let (_, text, text_err) = env.text(&["sync"]);
    let (_, dry, dry_err) = env.text(&["--dry-run", "sync"]);
    let location = CodexAdapter
        .locate(&env.environment(), Scope::User)
        .unwrap();
    let read = CodexAdapter.read(&location).unwrap();
    let output = format!("{json}{json_err}{text}{text_err}{dry}{dry_err}");
    assert!(!output.contains("CANARY"), "{output}");
    for name in ["foreign-http", "foreign-stdio", "foreign-oauth"] {
        assert!(!output.contains(&read.entries[name].hash), "{name}");
    }
    for (path, content) in snapshot(&env.default_store()) {
        if let Some(bytes) = content {
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("CANARY"), "{}", path.display());
            for name in ["foreign-http", "foreign-stdio", "foreign-oauth"] {
                assert!(!text.contains(&read.entries[name].hash));
            }
        }
    }
}

#[test]
fn rendering_covers_every_auth_form_and_never_writes_secret_values() {
    let env = setup();
    write_config(&env, "");
    let definitions = [
        (
            "stdio-all",
            "name = \"stdio-all\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"pkg\"]\n\
             [env]\nGITHUB_TOKEN = { forward = true }\nAPI_VAR = { forward = true }\nLOG_LEVEL = { value = \"info\" }\n\
             [harness.codex]\ncwd = \"/srv/app\"\nstartup_timeout_sec = 20\nenabled_tools = [\"search\"]\n",
            "[mcp_servers.stdio-all]\ncommand = \"npx\"\nargs = [\"-y\", \"pkg\"]\nenv_vars = [\"API_VAR\", \"GITHUB_TOKEN\"]\n\
             cwd = \"/srv/app\"\nstartup_timeout_sec = 20\nenabled_tools = [\"search\"]\n\n\
             [mcp_servers.stdio-all.env]\nLOG_LEVEL = \"info\"\n",
        ),
        (
            "bearer",
            "name = \"bearer\"\n[transport]\ntype = \"http\"\nurl = \"https://b.example/mcp\"\n\
             [auth]\ntype = \"bearer_env\"\nvar = \"B_TOKEN\"\noauth_supported = true\n",
            "[mcp_servers.bearer]\nurl = \"https://b.example/mcp\"\nbearer_token_env_var = \"B_TOKEN\"\n",
        ),
        (
            "header",
            "name = \"header\"\n[transport]\ntype = \"http\"\nurl = \"https://h.example/mcp\"\n\
             [auth]\ntype = \"header_env\"\nheader = \"X-Api-Key\"\nvar = \"H_KEY\"\n",
            "[mcp_servers.header]\nurl = \"https://h.example/mcp\"\n\n[mcp_servers.header.env_http_headers]\nX-Api-Key = \"H_KEY\"\n",
        ),
        (
            "oauth",
            "name = \"oauth\"\n[transport]\ntype = \"http\"\nurl = \"https://o.example/mcp\"\n\
             [auth]\ntype = \"oauth\"\nclient_id = \"d-mcp-client\"\nscopes = [\"write\", \"read\"]\nclient_secret_env = \"O_SECRET\"\n",
            "[mcp_servers.oauth]\nurl = \"https://o.example/mcp\"\nscopes = [\"read\", \"write\"]\n\n[mcp_servers.oauth.oauth]\nclient_id = \"d-mcp-client\"\n",
        ),
        (
            "open",
            "name = \"open\"\n[transport]\ntype = \"http\"\nurl = \"https://n.example/mcp\"\n[auth]\ntype = \"none\"\n",
            "[mcp_servers.open]\nurl = \"https://n.example/mcp\"\n",
        ),
    ];
    for (name, definition, _) in &definitions {
        env.write_local(name, definition);
        select(&env, name);
    }
    let output = env
        .cmd()
        .env("GITHUB_TOKEN", "CANARY_PROCESS_ENV_SECRET")
        .env("O_SECRET", "CANARY_PROCESS_CLIENT_SECRET")
        .args(["--json", "sync"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let written = config(&env);
    for (name, _, expected) in &definitions {
        assert!(written.contains(expected), "{name}:\n{written}");
    }
    assert!(!written.contains("CANARY"));
    assert!(!written.contains("auth ="));
    let gaps = |name: &str| -> Vec<String> {
        entry(&value, name)["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|gap| {
                assert_eq!(gap["severity"], "note");
                gap["field"].as_str().unwrap().to_owned()
            })
            .collect()
    };
    assert_eq!(gaps("open"), ["auth"]);
    assert_eq!(gaps("bearer"), ["auth.oauth_supported"]);
    assert_eq!(gaps("oauth"), ["auth.client_secret_env"]);
    assert!(gaps("stdio-all").is_empty());

    // The manual step: a hand-entered client secret is preserved, never
    // hashed (no drift) and kept on Update; the gap disappears.
    let with_secret = written.replace(
        "client_id = \"d-mcp-client\"\n",
        "client_id = \"d-mcp-client\"\nclient_secret = \"CANARY_HAND_ENTERED_SECRET\"\n",
    );
    fs::write(env.codex_config(), &with_secret).unwrap();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "oauth"), "unchanged");
    assert!(gaps_of(&value, "oauth").is_empty());
    env.write_local(
        "oauth",
        &definitions[3]
            .1
            .replace("[\"write\", \"read\"]", "[\"read\"]"),
    );
    let (code, value, stderr) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "oauth"), "update");
    let updated = config(&env);
    assert!(updated.contains(
        "[mcp_servers.oauth]\nurl = \"https://o.example/mcp\"\nscopes = [\"read\"]\n\n\
         [mcp_servers.oauth.oauth]\nclient_id = \"d-mcp-client\"\nclient_secret = \"CANARY_HAND_ENTERED_SECRET\"\n"
    ));
    assert!(!format!("{value}{stderr}{}", state(&env)).contains("CANARY"));
}

/// Codex's own load rules for `oauth` keys (verified against Codex
/// 0.165.0): it refuses its whole configuration when an `oauth` table sits on
/// a stdio entry, or when `oauth.client_secret` has no `oauth.client_id`.
fn assert_codex_accepts_oauth(text: &str) {
    let document: toml_edit::DocumentMut = text.parse().expect("valid TOML");
    let Some(servers) = document.get("mcp_servers").and_then(|s| s.as_table()) else {
        return;
    };
    for (name, entry) in servers {
        let Some(oauth) = entry.get("oauth") else {
            continue;
        };
        assert!(
            entry.get("command").is_none(),
            "{name}: oauth on stdio\n{text}"
        );
        if oauth.get("client_secret").is_some() {
            assert!(
                oauth.get("client_id").is_some(),
                "{name}: secret without id\n{text}"
            );
        }
    }
}

#[test]
fn an_update_removes_preserved_keys_that_no_longer_fit() {
    const SECRET: &str = "CANARY_STALE_CLIENT_SECRET";
    const OAUTH: &str = "name = \"x\"\n[transport]\ntype = \"http\"\nurl = \"https://x.example/mcp\"\n\
                         [auth]\ntype = \"oauth\"\nclient_id = \"cid\"\n";
    let http = |auth: &str| {
        format!(
            "name = \"x\"\n[transport]\ntype = \"http\"\nurl = \"https://x.example/mcp\"\n{auth}"
        )
    };
    let cases: [(&str, String, &[&str]); 5] = [
        (
            "stdio",
            "name = \"x\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n".to_owned(),
            &[
                "oauth.callback_port",
                "oauth.callback_url",
                "oauth.client_secret",
            ],
        ),
        (
            "bearer",
            http("[auth]\ntype = \"bearer_env\"\nvar = \"X_TOKEN\"\n"),
            &[
                "oauth.callback_port",
                "oauth.callback_url",
                "oauth.client_secret",
            ],
        ),
        (
            "header",
            http("[auth]\ntype = \"header_env\"\nheader = \"X-Key\"\nvar = \"X_KEY\"\n"),
            &[
                "oauth.callback_port",
                "oauth.callback_url",
                "oauth.client_secret",
            ],
        ),
        (
            "no-client-id",
            http("[auth]\ntype = \"oauth\"\nscopes = [\"read\"]\n"),
            &["oauth.client_secret"],
        ),
        (
            "compatible",
            http("[auth]\ntype = \"oauth\"\nclient_id = \"cid\"\nscopes = [\"read\"]\n"),
            &[],
        ),
    ];
    for (case, definition, dropped) in cases {
        let env = setup();
        write_config(&env, "");
        env.write_local("x", OAUTH);
        select(&env, "x");
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{case}: {value:#}");
        // The hand-entered local OAuth client fields.
        let local = config(&env).replace(
            "client_id = \"cid\"\n",
            &format!(
                "client_id = \"cid\"\nclient_secret = \"{SECRET}\"\n\
                 callback_url = \"http://127.0.0.1:5555/cb\"\ncallback_port = 5555\n"
            ),
        );
        assert!(local.contains(SECRET), "{case}");
        write_config(&env, &local);

        env.write_local("x", &definition);
        let (code, value, stderr) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{case}: {value:#}");
        assert_eq!(op(&value, "x"), "update", "{case}");
        let written = config(&env);
        assert_codex_accepts_oauth(&written);
        let notes: Vec<String> = gaps_of(&value, "x")
            .iter()
            .filter(|gap| gap["field"].as_str().unwrap().starts_with("oauth."))
            .map(|gap| {
                assert_eq!(gap["severity"], "note", "{case}");
                let field = gap["field"].as_str().unwrap().to_owned();
                assert_eq!(
                    gap["reason"],
                    format!("{field} removed: no longer used by this definition")
                );
                field
            })
            .collect();
        assert_eq!(notes, dropped, "{case}");
        for key in ["client_secret", "callback_url", "callback_port"] {
            let gone = dropped.contains(&format!("oauth.{key}").as_str());
            assert_eq!(
                !written.contains(&format!("{key} = ")),
                gone,
                "{case}: {key}\n{written}"
            );
        }
        // The secret value is never shown or recorded, kept or not.
        assert!(
            !format!("{value}{stderr}{}", state(&env)).contains(SECRET),
            "{case}"
        );
        assert!(!format!("{value}{stderr}").contains("127.0.0.1"), "{case}");

        // The read-back verification recorded what is on disk: the next run
        // sees no drift and reports no removal again.
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{case}: {value:#}");
        assert_eq!(op(&value, "x"), "unchanged", "{case}");
        assert!(
            gaps_of(&value, "x")
                .iter()
                .all(|gap| !gap["field"].as_str().unwrap().starts_with("oauth.")),
            "{case}"
        );
        assert_eq!(config(&env), written, "{case}");
    }
}

fn gaps_of(value: &Value, name: &str) -> Vec<Value> {
    entry(value, name)["gaps"].as_array().unwrap().clone()
}

#[test]
fn override_fixtures_validate_by_rule_code() {
    let root = Path::new(FIXTURES).join("codex-overrides");
    let mut checked = 0;
    for path in fs::read_dir(root.join("valid")).unwrap() {
        let path = path.unwrap().path();
        let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let (definition, sites) =
            parse_definition_with_sites(&fs::read_to_string(&path).unwrap(), &stem).unwrap();
        assert!(
            validate_definition(&definition, &sites, registry()).is_ok(),
            "{stem}"
        );
        checked += 1;
    }
    for rule_dir in fs::read_dir(root.join("invalid")).unwrap() {
        let rule_dir = rule_dir.unwrap().path();
        let rule = Rule::from_code(rule_dir.file_name().unwrap().to_str().unwrap()).unwrap();
        for path in fs::read_dir(&rule_dir).unwrap() {
            let path = path.unwrap().path();
            let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
            // Every fixture is a valid parse; only `validate` rejects it.
            let (definition, sites) =
                parse_definition_with_sites(&fs::read_to_string(&path).unwrap(), &stem).unwrap();
            let problem = validate_definition(&definition, &sites, registry()).unwrap_err();
            assert_eq!(problem.rule, rule, "{}", path.display());
            assert!(problem.line.is_some() && problem.column.is_some());
            checked += 1;
        }
    }
    assert!(checked >= 20);
    // WP1's valid parse fixtures with a `harness.codex` table stay valid
    // parses; `validate` is a separate step.
    let github = fs::read_to_string(format!("{FIXTURES}/definitions/valid/github.toml")).unwrap();
    let (definition, sites) = parse_definition_with_sites(&github, "github").unwrap();
    assert!(validate_definition(&definition, &sites, registry()).is_ok());
}

#[test]
fn codex_absent_creates_nothing() {
    for env in [
        TestEnv::new().without_codex(),
        TestEnv::new().without_codex().without_codex_home(),
    ] {
        env.init();
        env.write_local("github", GITHUB);
        select(&env, "github");
        let before = snapshot(env.root());
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{value:#}");
        assert_eq!(harness(&value)["status"], "not-present");
        assert_eq!(harness(&value)["notices"][0], "harness not present");
        assert!(!env.codex_config().exists());
        let after = snapshot(env.root());
        let store_state = env.default_store().join("state.toml");
        assert_eq!(
            before,
            after
                .into_iter()
                .filter(|(path, _)| path != &store_state)
                .collect::<Vec<_>>()
        );
        let (_, text, _) = env.text(&["sync"]);
        assert!(text.contains("not-present"), "{text}");
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn a_present_binary_without_a_file_creates_it_with_safe_modes() {
    let env = TestEnv::new();
    let bin = env.fake_codex_dir();
    let env = env.with_path(&[bin]);
    env.init();
    fs::remove_dir(env.codex_home()).unwrap();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let (code, value, _) = sync_json(&env, &["--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(entry(&value, "github")["outcome"], "planned");
    assert!(!env.codex_home().exists());
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(harness(&value)["created"], true);
    assert_eq!(mode(&env.codex_config()), 0o600);
    assert_eq!(mode(&env.codex_home()), 0o700);
    // `sync` never runs `codex`.
    assert!(!env.codex_home().join("fake-codex.log").exists());
}

#[cfg(unix)]
#[test]
fn symlinks_and_modes_are_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let env = setup();
    let dotfiles = env.root().join("dotfiles");
    fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("codex.toml");
    fs::write(&target, "model = \"o3\"\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    fs::create_dir_all(env.codex_home()).unwrap();
    std::os::unix::fs::symlink(&target, env.codex_config()).unwrap();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert!(
        fs::symlink_metadata(env.codex_config())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("[mcp_servers.github]")
    );
    assert_eq!(mode(&target), 0o640);
    // The recorded path is the link, not its target.
    let link = env.codex_config();
    assert!(state(&env).contains(&format!("file = \"{}\"", link.display())));
    // Retargeting the dotfile link is not a location change.
    let moved = dotfiles.join("moved.toml");
    fs::copy(&target, &moved).unwrap();
    fs::remove_file(env.codex_config()).unwrap();
    std::os::unix::fs::symlink(&moved, env.codex_config()).unwrap();
    assert_eq!(op(&sync_json(&env, &[]).1, "github"), "unchanged");
}

#[test]
fn dry_runs_leave_every_file_byte_identical() {
    let env = setup();
    write_config(&env, &fixture("comments.toml"));
    env.write_local("github", GITHUB);
    env.write_local("dmcp-new", NEW);
    select(&env, "github");
    select(&env, "dmcp-new");
    let before = snapshot(env.root());
    let (code, value, _) = sync_json(&env, &["--dry-run"]);
    assert_eq!(code, 1);
    assert_eq!(value["data"]["dry_run"], true);
    assert_eq!(entry(&value, "dmcp-new")["outcome"], "planned");
    let (_, text, _) = env.text(&["--dry-run", "sync", "--take-over", "github"]);
    assert!(text.ends_with("Dry run: nothing was written.\n"), "{text}");
    assert_eq!(snapshot(env.root()), before);
    // With a record in place, `unmanage --dry-run` only reports.
    sync_json(&env, &["--take-over", "github"]);
    let before = snapshot(env.root());
    let (code, value, _) = env.json(&["--dry-run", "unmanage", "github"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["dry_run"], true);
    let (_, value, _) = sync_json(&env, &["--dry-run"]);
    assert_eq!(op(&value, "github"), "unchanged");
    assert_eq!(snapshot(env.root()), before);
}

#[test]
fn unmanage_drops_records_and_harnesses() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    env.write_local("dmcp-new", NEW);
    env.cmd().args(["select", "github"]).assert().success();
    select(&env, "dmcp-new");
    sync_json(&env, &[]);
    let written = config(&env);

    let (code, value, _) = env.json(&["unmanage", "github"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(value["data"]["harnesses"], serde_json::json!(["codex"]));
    assert_eq!(value["data"]["selection"], serde_json::json!(["claude"]));
    assert_eq!(value["data"]["converted_from_all"], true);
    assert_eq!(value["data"]["selection_removed"], false);
    let config_text = fs::read_to_string(env.default_store().join("config.toml")).unwrap();
    assert!(config_text.contains("server = \"github\"\nharnesses = [\"claude\"]"));

    let (code, stdout, _) = env.text(&["unmanage", "dmcp-new", "--harness", "codex"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("was removed"), "{stdout}");
    let config_text = fs::read_to_string(env.default_store().join("config.toml")).unwrap();
    assert!(!config_text.contains("dmcp-new"));
    assert!(!state(&env).contains("name = "));
    // The entries are left as they are, and the next sync reports nothing.
    assert_eq!(config(&env), written);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert!(harness(&value)["entries"].as_array().unwrap().is_empty());
    assert_eq!(config(&env), written);

    let (code, value, _) = env.json(&["unmanage", "github"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "not-managed");
    // `claude` has an adapter now; without a record it is not managed there.
    let (code, value, _) = env.json(&["unmanage", "github", "--harness", "claude"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "not-managed");
    let (code, value, _) = sync_json(&env, &["--harness", "claude"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(harness(&value)["harness"], "claude");
    assert_eq!(harness(&value)["status"], "not-present");
    let output = env
        .cmd()
        .args(["sync", "--harness", "cursor"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn sync_rejections() {
    let env = TestEnv::new().without_codex();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "store-not-initialized");
    assert!(!env.default_store().exists());
    env.init();
    let (code, value, _) = sync_json(&env, &["--take-over", "nobody"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "unknown-take-over");
    assert_eq!(value["data"], Value::Null);
    fs::write(
        env.default_store().join("state.toml"),
        "schema_version = 1\nx = 1\n",
    )
    .unwrap();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3);
    assert_eq!(value["error"]["code"], "state-corrupt");
    fs::write(
        env.default_store().join("state.toml"),
        "schema_version = 9\n",
    )
    .unwrap();
    let (code, value, _) = env.json(&["unmanage", "x"]);
    assert_eq!(code, 3);
    assert_eq!(value["error"]["code"], "schema-unsupported");
    fs::remove_file(env.default_store().join("state.toml")).unwrap();
    let (code, value, _) = sync_json(&env, &["--harness", "codex"]);
    assert_eq!(code, 0);
    assert_eq!(harness(&value)["harness"], "codex");
}

// ---------------------------------------------------------------------------
// Failure injection through the library API.

fn options<'a>(hooks: SyncHooks<'a>) -> SyncOptions<'a> {
    SyncOptions {
        hooks,
        ..SyncOptions::default()
    }
}

fn run_sync(
    env: &TestEnv,
    options: &SyncOptions<'_>,
) -> d_mcp::error::Result<(Value, Option<Error>)> {
    let paths = StorePaths::new(env.default_store());
    let mut warnings = Vec::new();
    sync(&paths, &env.environment(), options, &mut warnings)
        .map(|(report, error)| (report.data, error))
}

#[test]
fn a_file_changed_before_persist_aborts_with_exit_3() {
    let env = setup();
    write_config(&env, "model = \"o3\"\n");
    env.write_local("github", GITHUB);
    select(&env, "github");
    let file = env.codex_config();
    let change = || -> d_mcp::error::Result<()> {
        fs::write(&file, "model = \"changed\"\n").unwrap();
        Ok(())
    };
    let hooks = SyncHooks {
        adapter: Hooks {
            before_persist: Some(&change),
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let (data, error) = run_sync(&env, &options(hooks)).unwrap();
    let error = error.unwrap();
    assert_eq!(error.code(), "harness-file-changed");
    assert_eq!(error.exit_code(), 3);
    assert_eq!(data["harnesses"][0]["entries"][0]["outcome"], "failed");
    assert_eq!(config(&env), "model = \"changed\"\n");
    // The pending Add was settled: its entry is absent, so it is dropped.
    assert!(!state(&env).contains("github"));
}

#[cfg(unix)]
#[test]
fn a_retargeted_link_before_persist_aborts_with_exit_3() {
    let env = setup();
    let a = env.root().join("a.toml");
    let b = env.root().join("b.toml");
    fs::write(&a, "model = \"a\"\n").unwrap();
    fs::write(&b, "model = \"a\"\n").unwrap();
    fs::create_dir_all(env.codex_home()).unwrap();
    std::os::unix::fs::symlink(&a, env.codex_config()).unwrap();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let link = env.codex_config();
    let retarget = || -> d_mcp::error::Result<()> {
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        Ok(())
    };
    let hooks = SyncHooks {
        adapter: Hooks {
            before_persist: Some(&retarget),
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let (_, error) = run_sync(&env, &options(hooks)).unwrap();
    let error = error.unwrap();
    assert_eq!(error.code(), "harness-file-changed");
    assert_eq!(error.exit_code(), 3);
    assert!(!state(&env).contains("github"));
    assert_eq!(fs::read_to_string(&a).unwrap(), "model = \"a\"\n");
    assert_eq!(fs::read_to_string(&b).unwrap(), "model = \"a\"\n");
}

#[test]
fn a_crash_between_persist_and_promotion_is_promoted_on_the_next_run() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    let crash = || -> d_mcp::error::Result<()> {
        Err(Error::Io {
            operation: "simulate a crash before",
            path: PathBuf::from("/promotion"),
            kind: std::io::ErrorKind::Interrupted,
        })
    };
    let hooks = SyncHooks {
        before_promote: Some(&crash),
        ..SyncHooks::default()
    };
    assert!(run_sync(&env, &options(hooks)).is_err());
    assert!(config(&env).contains("[mcp_servers.github]"));
    assert!(state(&env).contains("status = \"pending\""));
    assert!(state(&env).contains("op = \"add\""));
    // A dry run treats the settled record as owned, and writes nothing.
    let (_, value, _) = sync_json(&env, &["--dry-run"]);
    assert_eq!(op(&value, "github"), "unchanged");
    assert!(state(&env).contains("status = \"pending\""));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "unchanged");
    assert!(
        harness(&value)["notices"][0]
            .as_str()
            .unwrap()
            .contains("was completed")
    );
    assert!(state(&env).contains("status = \"owned\""));
}

#[test]
fn a_failed_persist_of_an_update_keeps_ownership() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let owned_state = state(&env);
    let written = config(&env);
    env.write_local("github", &GITHUB.replace("info", "debug"));
    let fail = || -> d_mcp::error::Result<()> {
        Err(Error::Io {
            operation: "persist",
            path: PathBuf::from("/injected"),
            kind: std::io::ErrorKind::StorageFull,
        })
    };
    let hooks = SyncHooks {
        adapter: Hooks {
            before_persist: Some(&fail),
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let (_, error) = run_sync(&env, &options(hooks)).unwrap();
    let error = error.unwrap();
    assert_eq!(error.code(), "io-error");
    assert_eq!(error.exit_code(), 4);
    assert_eq!(config(&env), written);
    let after = state(&env);
    assert!(after.contains("status = \"owned\""));
    // The previous owned hashes are back.
    let hashes = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with("sent") || line.starts_with("readback"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(hashes(&after), hashes(&owned_state));
    assert_eq!(op(&sync_json(&env, &[]).1, "github"), "update");
}

#[test]
fn a_failed_verification_restores_the_original_bytes() {
    let env = setup();
    let original = fixture("crlf.toml");
    write_config(&env, &original);
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        adapter: Hooks {
            force_verify_failure: true,
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let (_, error) = run_sync(&env, &options(hooks)).unwrap();
    let error = error.unwrap();
    assert_eq!(error.code(), "harness-verify-failed");
    assert_eq!(error.exit_code(), 3);
    assert!(error.hint().contains("restored"));
    assert_eq!(config(&env), original);
    assert!(!state(&env).contains("github"));

    // A file changed after the persist is not overwritten by the restore.
    let file = env.codex_config();
    let tamper = || -> d_mcp::error::Result<()> {
        fs::write(&file, "model = \"tampered\"\n").unwrap();
        Ok(())
    };
    let hooks = SyncHooks {
        adapter: Hooks {
            after_persist: Some(&tamper),
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let (_, error) = run_sync(&env, &options(hooks)).unwrap();
    let error = error.unwrap();
    assert_eq!(error.code(), "harness-verify-failed");
    assert!(error.hint().contains("FAILED"));
    assert_eq!(config(&env), "model = \"tampered\"\n");
}

#[test]
fn a_failed_verification_of_a_new_file_removes_it() {
    let env = TestEnv::new();
    let bin = env.fake_codex_dir();
    let env = env.with_path(&[bin]);
    env.init();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        adapter: Hooks {
            force_verify_failure: true,
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let (_, error) = run_sync(&env, &options(hooks)).unwrap();
    assert!(error.unwrap().hint().contains("restored"));
    assert!(!env.codex_config().exists());
}

// ---------------------------------------------------------------------------
// Login state through the fake `codex`.

fn login_env(mode: Option<&str>) -> (TestEnv, Vec<String>) {
    let env = TestEnv::new();
    let bin = env.fake_codex_dir();
    let env = env.with_path(&[bin]);
    if let Some(mode) = mode {
        fs::write(env.codex_home().join("fake-codex-mode"), mode).unwrap();
    }
    let names = ["stdio1", "oauth1", "oauth2", "bearer1", "odd1", "missing"]
        .map(str::to_owned)
        .to_vec();
    (env, names)
}

#[test]
fn login_state_maps_the_listing() {
    let (env, names) = login_env(None);
    let states = CodexAdapter.login_state(&env.environment(), &ProcessRunner::default(), &names);
    assert_eq!(states["stdio1"], LoginState::NotApplicable);
    assert_eq!(states["oauth1"], LoginState::NeedsLogin);
    assert_eq!(states["oauth2"], LoginState::LoggedIn);
    assert_eq!(states["bearer1"], LoginState::NotApplicable);
    assert_eq!(states["odd1"], LoginState::Unknown);
    assert_eq!(states["missing"], LoginState::Unknown);
    let log = fs::read_to_string(env.codex_home().join("fake-codex.log")).unwrap();
    assert_eq!(log, "called: mcp list --json\n");
}

#[test]
fn login_state_is_unknown_on_every_failure() {
    let quick = ProcessRunner {
        timeout: Duration::from_millis(800),
        max_output: 4096,
    };
    for mode in ["timeout", "grandchild", "oversized", "fail", "garbage"] {
        let (env, names) = login_env(Some(mode));
        let started = Instant::now();
        let states = CodexAdapter.login_state(&env.environment(), &quick, &names);
        assert!(
            states.values().all(|state| *state == LoginState::Unknown),
            "{mode}: {states:?}"
        );
        // The fake sleeps 30 s (timeout) and 15 s (grandchild); a generous
        // bound keeps slow CI machines green.
        assert!(started.elapsed() < Duration::from_secs(10), "{mode}");
    }
    // The fake can sleep under the restricted PATH, so the timeout and
    // grandchild modes above really exercised the timeout.
    let (env, names) = login_env(Some("probe"));
    let codex = env.root().join("bin/codex");
    let output = ProcessRunner::default()
        .run(
            &Invocation::new(&codex).env("CODEX_HOME", env.codex_home()),
            env.environment().path.as_deref(),
        )
        .unwrap();
    assert!(output.success() && output.stdout == b"[]\n", "{output:?}");
    let timed = Instant::now();
    fs::write(env.codex_home().join("fake-codex-mode"), "timeout").unwrap();
    let states = CodexAdapter.login_state(&env.environment(), &quick, &names);
    assert!(states.values().all(|state| *state == LoginState::Unknown));
    assert!(timed.elapsed() >= Duration::from_millis(700));
    let (env, names) = login_env(None);
    let without = env.without_codex();
    let states = CodexAdapter.login_state(&without.environment(), &quick, &names);
    assert!(states.values().all(|state| *state == LoginState::Unknown));
}

#[test]
fn the_runner_reports_timeouts_and_caps_output() {
    let (env, _) = login_env(Some("timeout"));
    let codex = env.root().join("bin/codex");
    let invocation = Invocation::new(&codex)
        .args(["mcp", "list"])
        .env("CODEX_HOME", env.codex_home());
    let quick = ProcessRunner {
        timeout: Duration::from_millis(300),
        max_output: 64,
    };
    assert!(matches!(
        quick.run(&invocation, None),
        Err(RunError::Timeout { .. })
    ));
    fs::write(env.codex_home().join("fake-codex-mode"), "grandchild").unwrap();
    let started = Instant::now();
    assert!(matches!(
        quick.run(&invocation, None),
        Err(RunError::Timeout { .. })
    ));
    // The grandchild sleeps 15 s; the bound is generous for slow CI.
    assert!(started.elapsed() < Duration::from_secs(8));
    fs::write(env.codex_home().join("fake-codex-mode"), "oversized").unwrap();
    let output = ProcessRunner {
        timeout: Duration::from_secs(5),
        max_output: 64,
    }
    .run(&invocation, None)
    .unwrap();
    assert!(output.truncated);
    assert_eq!(output.stdout.len(), 64);
    // Errors never carry child output.
    fs::write(env.codex_home().join("fake-codex-mode"), "fail").unwrap();
    let output = ProcessRunner::default().run(&invocation, None).unwrap();
    assert!(!output.success());
    assert!(!format!("{output:?}").contains("CANARY"));
}

#[test]
fn sync_never_calls_codex_and_never_prints_its_output() {
    let env = TestEnv::new();
    let bin = env.fake_codex_dir();
    let env = env.with_path(&[bin]);
    env.init();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    let (code, value, stderr) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    let (_, text, text_err) = env.text(&["sync"]);
    assert!(!env.codex_home().join("fake-codex.log").exists());
    assert!(!format!("{value}{stderr}{text}{text_err}").contains("CANARY"));
}

// ---------------------------------------------------------------------------
// Added by the acceptance audit.

#[test]
fn update_and_remove_in_the_middle_keep_every_other_byte_on_every_fixture() {
    for name in [
        "crlf.toml",
        "no-trailing-newline.toml",
        "last-table.toml",
        "subtables.toml",
        "comments.toml",
    ] {
        let env = setup();
        let original = fixture(name);
        write_config(&env, &original);
        env.write_local("dmcp-new", NEW);
        select(&env, "dmcp-new");
        assert_eq!(sync_json(&env, &[]).0, 0, "{name}");
        let added = config(&env);
        let head = with_separator(&original);
        let entry_text = &added[head.len()..];
        let eol = if original.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };

        // A hand-written comment above the header, and foreign content
        // (without a trailing newline) after the entry.
        let above = format!("# above the managed header{eol}");
        let tail = format!("{eol}# foreign tail{eol}[tail-table]{eol}k = 1");
        fs::write(
            env.codex_config(),
            format!("{head}{above}{entry_text}{tail}"),
        )
        .unwrap();
        assert_eq!(
            op(&sync_json(&env, &[]).1, "dmcp-new"),
            "unchanged",
            "{name}"
        );

        env.write_local("dmcp-new", &NEW.replace("pkg@1", "pkg@2"));
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{name}: {value:#}");
        assert_eq!(op(&value, "dmcp-new"), "update", "{name}");
        let updated = config(&env);
        let start = head.len() + above.len();
        assert!(updated.starts_with(&format!("{head}{above}")), "{name}");
        assert!(updated.ends_with(&tail), "{name}");
        let middle = &updated[start..updated.len() - tail.len()];
        assert!(
            middle.contains("pkg@2") && !middle.contains("pkg@1"),
            "{name}"
        );
        assert_eq!(middle.contains("\r\n"), eol == "\r\n", "{name}");

        env.cmd().args(["unselect", "dmcp-new"]).assert().success();
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{name}");
        assert_eq!(op(&value, "dmcp-new"), "remove", "{name}");
        assert_eq!(config(&env), format!("{head}{above}{tail}"), "{name}");
    }
}

#[test]
fn every_wp1_parse_fixture_stays_a_valid_parse() {
    let mut seen = 0;
    for path in fs::read_dir(format!("{FIXTURES}/definitions/valid")).unwrap() {
        let path = path.unwrap().path();
        let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let (definition, sites) =
            parse_definition_with_sites(&fs::read_to_string(&path).unwrap(), &stem)
                .unwrap_or_else(|error| panic!("{stem}: {error:?}"));
        assert_eq!(
            parse_definition(&fs::read_to_string(&path).unwrap(), &stem).unwrap(),
            definition
        );
        // WP1 accepts any `harness.<id>` content; only `validate` narrows
        // the Codex keys, so WP1 fixtures with such a table may fail there.
        if definition.overrides.is_empty() {
            validate_definition(&definition, &sites, registry())
                .unwrap_or_else(|problem| panic!("{stem}: {problem:?}"));
        }
        seen += 1;
    }
    assert!(seen >= 15);
}

#[test]
fn dry_runs_create_no_lock_file_and_an_unknown_harness_exits_2() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let lock = env.default_store().join(".lock");
    fs::remove_file(&lock).unwrap();
    let before = snapshot(env.root());
    for args in [
        vec!["--dry-run", "sync"],
        vec!["--dry-run", "sync", "--take-over", "github"],
        vec!["--dry-run", "unmanage", "github"],
    ] {
        let output = env.cmd().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(!lock.exists(), "{args:?}");
    }
    assert_eq!(snapshot(env.root()), before);
    for args in [
        vec!["sync", "--harness", "nope"],
        vec!["unmanage", "github", "--harness", "nope"],
    ] {
        let output = env.cmd().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
    }
    assert_eq!(snapshot(env.root()), before);
}

// ---------------------------------------------------------------------------
// Added by the review pass.

#[test]
fn trailing_comments_after_the_last_managed_table_survive_update_and_remove() {
    for name in ["trailing-comments.toml", "trailing-comments-crlf.toml"] {
        let env = setup();
        let original = fixture(name);
        let eol = if original.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        write_config(&env, &original);
        env.write_local("github", GITHUB);
        select(&env, "github");
        let header = original.find("[mcp_servers.github]").unwrap();
        let last_line = format!("OLD = \"value\" # same-line comment{eol}");
        let tail_start = original.find(&last_line).unwrap() + last_line.len();
        let tail = &original[tail_start..];
        assert!(
            tail.contains("# Trailing comment after the last table"),
            "{name}"
        );

        let (code, value, _) = sync_json(&env, &["--take-over", "github"]);
        assert_eq!(code, 0, "{name}: {value:#}");
        assert_eq!(op(&value, "github"), "update", "{name}");
        let updated = config(&env);
        let expected_entry = "[mcp_servers.github]\ncommand = \"npx\"\n\n\
                              [mcp_servers.github.env]\nLOG_LEVEL = \"info\"\n"
            .replace('\n', eol);
        assert_eq!(
            updated,
            format!("{}{expected_entry}{tail}", &original[..header]),
            "{name}"
        );
        assert_eq!(op(&sync_json(&env, &[]).1, "github"), "unchanged", "{name}");

        env.cmd().args(["unselect", "github"]).assert().success();
        let (code, value, _) = sync_json(&env, &[]);
        assert_eq!(code, 0, "{name}");
        assert_eq!(op(&value, "github"), "remove", "{name}");
        assert_eq!(
            config(&env),
            format!("{}{tail}", &original[..header]),
            "{name}"
        );
    }
}

fn crash() -> d_mcp::error::Result<()> {
    Err(Error::Io {
        operation: "simulate a crash before",
        path: PathBuf::from("/promotion"),
        kind: std::io::ErrorKind::Interrupted,
    })
}

fn injected_persist_failure() -> d_mcp::error::Result<()> {
    Err(Error::Io {
        operation: "persist",
        path: PathBuf::from("/injected"),
        kind: std::io::ErrorKind::StorageFull,
    })
}

/// A hand-written `github` entry that differs from the rendering, so a
/// take-over leads to an Update.
const HAND_WRITTEN: &str = "[mcp_servers.github]\ncommand = \"old-command\"\n";

#[test]
fn a_crash_after_a_take_over_update_is_settled_by_the_next_sync() {
    let env = setup();
    write_config(&env, HAND_WRITTEN);
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        before_promote: Some(&crash),
        ..SyncHooks::default()
    };
    let take_over = SyncOptions {
        take_over: ["github".to_owned()].into(),
        ..options(hooks)
    };
    assert!(run_sync(&env, &take_over).is_err());
    assert!(config(&env).contains("LOG_LEVEL = \"info\""));
    let pending = state(&env);
    assert!(pending.contains("status = \"pending\""), "{pending}");
    assert!(pending.contains("op = \"update\""), "{pending}");
    // The journal holds the taken-over hashes as the previous ones, so the
    // state stays valid.
    assert!(pending.contains("prev_readback = "), "{pending}");
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "unchanged");
    assert!(
        harness(&value)["notices"][0]
            .as_str()
            .unwrap()
            .contains("was completed")
    );
    assert!(state(&env).contains("status = \"owned\""));

    // A crash whose write did not take effect reverts to the taken-over
    // hashes, so the entry stays owned.
    let env = setup();
    write_config(&env, HAND_WRITTEN);
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        before_promote: Some(&crash),
        ..SyncHooks::default()
    };
    let take_over = SyncOptions {
        take_over: ["github".to_owned()].into(),
        ..options(hooks)
    };
    assert!(run_sync(&env, &take_over).is_err());
    write_config(&env, HAND_WRITTEN);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "update");
    assert!(
        harness(&value)["notices"][0]
            .as_str()
            .unwrap()
            .contains("was reverted")
    );
}

#[test]
fn a_failed_persist_after_a_take_over_keeps_the_taken_over_ownership() {
    let env = setup();
    write_config(&env, HAND_WRITTEN);
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        adapter: Hooks {
            before_persist: Some(&injected_persist_failure),
            ..Hooks::default()
        },
        ..SyncHooks::default()
    };
    let take_over = SyncOptions {
        take_over: ["github".to_owned()].into(),
        ..options(hooks)
    };
    let (_, error) = run_sync(&env, &take_over).unwrap();
    assert_eq!(error.unwrap().code(), "io-error");
    assert_eq!(config(&env), HAND_WRITTEN);
    let after = state(&env);
    assert!(after.contains("status = \"owned\""), "{after}");
    // Without `--take-over`, the entry is owned: an Update, no Collision.
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "update");
    assert!(config(&env).contains("LOG_LEVEL = \"info\""));
}

#[test]
fn a_pending_record_for_another_file_is_left_for_its_location() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        before_promote: Some(&crash),
        ..SyncHooks::default()
    };
    assert!(run_sync(&env, &options(hooks)).is_err());
    assert!(state(&env).contains("status = \"pending\""));

    // `CODEX_HOME` moved: the pending record is not settled against the new
    // file; it is reported as a location change and stays pending.
    let other = env.root().join("other-codex");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("config.toml"), "").unwrap();
    let mut moved = env.environment();
    moved.codex_home = Some(other.clone().into_os_string());
    let paths = StorePaths::new(env.default_store());
    let (report, error) = sync(&paths, &moved, &SyncOptions::default(), &mut Vec::new()).unwrap();
    assert_eq!(error.unwrap().code(), "sync-blocked");
    let value = serde_json::json!({ "data": report.data });
    assert_eq!(harness(&value)["status"], "location-changed");
    assert_eq!(op(&value, "github"), "location-changed");
    assert!(harness(&value)["notices"].as_array().unwrap().is_empty());
    assert!(state(&env).contains("status = \"pending\""));
    assert_eq!(fs::read_to_string(other.join("config.toml")).unwrap(), "");

    // Back at the recorded location, the record is settled as completed.
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "unchanged");
    assert!(state(&env).contains("status = \"owned\""));
}

#[test]
fn an_interrupted_add_over_changed_content_is_a_collision() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    let hooks = SyncHooks {
        before_promote: Some(&crash),
        ..SyncHooks::default()
    };
    assert!(run_sync(&env, &options(hooks)).is_err());
    assert!(state(&env).contains("op = \"add\""));
    // The entry was changed before the next sync: d-mcp never owned this
    // content, so it is a collision, not drift.
    write_config(&env, HAND_WRITTEN);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "github"), "collision");
    assert_eq!(entry(&value, "github")["hash"], Value::Null);
    assert!(!state(&env).contains("github"));
    assert_eq!(config(&env), HAND_WRITTEN);
}

#[test]
fn a_broken_splice_writes_nothing_and_is_not_a_verify_failure() {
    let foreign = "[mcp_servers.foreign]\ncommand = \"foreign\"\n";
    type Splice<'a> = &'a dyn Fn(&str) -> String;
    let cases: [(&str, Splice<'_>); 3] = [
        ("unparsable", &|_: &str| "[[".to_owned()),
        ("foreign entry changed", &|text: &str| {
            text.replace("command = \"foreign\"", "command = \"changed\"")
        }),
        ("root made inline", &|_: &str| {
            "mcp_servers = { foreign = { command = \"foreign\" } }\n".to_owned()
        }),
    ];
    for (case, splice) in cases {
        let env = setup();
        write_config(&env, foreign);
        env.write_local("github", GITHUB);
        select(&env, "github");
        let hooks = SyncHooks {
            adapter: Hooks {
                after_splice: Some(splice),
                ..Hooks::default()
            },
            ..SyncHooks::default()
        };
        let (data, error) = run_sync(&env, &options(hooks)).unwrap();
        let error = error.unwrap();
        assert_eq!(error.code(), "harness-edit-failed", "{case}");
        assert_eq!(error.exit_code(), 3);
        assert!(error.hint().contains("nothing was written"), "{case}");
        assert_eq!(data["harnesses"][0]["entries"][0]["outcome"], "failed");
        assert_eq!(config(&env), foreign, "{case}");
        // No intent was journaled.
        assert!(!state(&env).contains("github"), "{case}");
    }
}

#[cfg(unix)]
#[test]
fn a_non_utf8_codex_home_is_an_environment_error() {
    use std::os::unix::ffi::OsStringExt;
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let mut bytes = env.root().join("codex").into_os_string().into_vec();
    bytes.extend_from_slice(b"-\xff");
    let mut environment = env.environment();
    environment.codex_home = Some(std::ffi::OsString::from_vec(bytes));
    let paths = StorePaths::new(env.default_store());
    let (report, error) = sync(
        &paths,
        &environment,
        &SyncOptions::default(),
        &mut Vec::new(),
    )
    .unwrap();
    let error = error.unwrap();
    assert_eq!(error.code(), "harness-path-not-utf8");
    assert_eq!(error.exit_code(), 4);
    assert_eq!(report.data["harnesses"][0]["status"], "failed");
    assert!(!state(&env).contains("github"));
}

#[test]
fn drift_of_an_unselected_entry_points_to_unmanage() {
    let env = setup();
    write_config(&env, "");
    env.write_local("github", GITHUB);
    select(&env, "github");
    assert_eq!(sync_json(&env, &[]).0, 0);
    let edited = config(&env).replace("command = \"npx\"", "command = \"edited\"");
    fs::write(env.codex_config(), &edited).unwrap();
    env.cmd().args(["unselect", "github"]).assert().success();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "github"), "drift");
    let notice = entry(&value, "github")["notice"].as_str().unwrap();
    assert!(!notice.contains("take-over"), "{notice}");
    assert!(
        notice.contains("d-mcp unmanage github --harness codex"),
        "{notice}"
    );
    assert_eq!(config(&env), edited);
}

/// `tests/fixtures/codex-rewrite`: three definitions, the file d-mcp wrote
/// for them, and the same file after the real Codex 0.165.0 re-saved it
/// for `codex mcp add foreign --env FOREIGN=... -- /bin/echo hi` (captured
/// in a temporary `CODEX_HOME`). Codex reorders keys, rewrites `30` as
/// `30.0` and drops default values; none of that may count as drift.
#[test]
fn a_real_codex_rewrite_plans_unchanged() {
    let dir = Path::new(FIXTURES).join("codex-rewrite");
    let names = ["dmcp-header", "dmcp-oauth", "dmcp-stdio"];
    let env = setup();
    write_config(&env, "");
    for name in names {
        let text = fs::read_to_string(dir.join(format!("servers/{name}.toml"))).unwrap();
        env.write_local(name, &text);
        select(&env, name);
    }
    assert_eq!(sync_json(&env, &[]).0, 0);
    let written = fs::read_to_string(dir.join("written-by-d-mcp.toml")).unwrap();
    assert_eq!(
        config(&env),
        written,
        "the fixture matches today's rendering"
    );

    let rewritten = fs::read_to_string(dir.join("rewritten-by-codex.toml")).unwrap();
    assert_ne!(rewritten, written);
    write_config(&env, &rewritten);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    for name in names {
        assert_eq!(op(&value, name), "unchanged", "{name}");
    }
    assert_eq!(config(&env), rewritten);

    // The OAuth client fields Codex keeps locally (the client secret and
    // the callback URL it writes next to it) are preserved keys: adding them
    // is no drift either, and an Update keeps them verbatim.
    let local = rewritten.replace(
        "client_id = \"dmcp-client\"\n",
        "client_id = \"dmcp-client\"\nclient_secret = \"CANARY_CLIENT_SECRET\"\n\
         callback_url = \"http://127.0.0.1/callback/abc\"\n",
    );
    write_config(&env, &local);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "dmcp-oauth"), "unchanged");
    let oauth = fs::read_to_string(dir.join("servers/dmcp-oauth.toml")).unwrap();
    env.write_local("dmcp-oauth", &oauth.replace("\"write\"", "\"admin\""));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "dmcp-oauth"), "update");
    let updated = config(&env);
    assert!(updated.contains("client_secret = \"CANARY_CLIENT_SECRET\"\n"));
    assert!(updated.contains("callback_url = \"http://127.0.0.1/callback/abc\"\n"));
    assert!(updated.contains("\"admin\""));
    assert!(!format!("{value}{}", state(&env)).contains("CANARY"));
    assert!(!format!("{value}").contains("127.0.0.1"));
}
