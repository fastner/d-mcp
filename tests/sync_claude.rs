//! Integration tests of `sync` with the Claude Code adapter.
//!
//! Every test runs in an isolated `TestEnv`: `HOME`, `CLAUDE_CONFIG_DIR` and
//! `CODEX_HOME` are tempdirs, and `PATH` holds at most the fake `claude`
//! (`tests/support/fake_claude.rs`), never a real `claude` or `codex`. The
//! fake logs every call and takes its behaviour from directives; it refuses
//! to run outside a marked test root.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestEnv, snapshot};
use d_mcp::adapter::claude::ClaudeAdapter;
use d_mcp::adapter::{Adapter, Hooks, LoginState, registry};
use d_mcp::definition::parse_definition_with_sites;
use d_mcp::error::Error;
use d_mcp::process::ProcessRunner;
use d_mcp::rule::Rule;
use d_mcp::store::{Environment, StorePaths};
use d_mcp::sync::{SyncHooks, SyncOptions, sync};
use d_mcp::validate::validate_definition;
use serde_json::{Map, Value, json};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

const GITHUB: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\n\
    command = \"/usr/local/bin/github-mcp\"\n[env]\nGITHUB_TOKEN = { forward = true }\n\
    LOG_LEVEL = { value = \"info\" }\n";
const GITHUB_V2: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\n\
    command = \"/usr/local/bin/github-mcp\"\nargs = [\"--v2\"]\n[env]\n\
    GITHUB_TOKEN = { forward = true }\nLOG_LEVEL = { value = \"info\" }\n";
const LINEAR: &str = include_str!("fixtures/definitions/valid/linear.toml");

fn fixture(name: &str) -> String {
    fs::read_to_string(format!("{FIXTURES}/claude/{name}")).expect("read fixture")
}

fn stdio(name: &str, command: &str) -> String {
    format!("name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"{command}\"\n")
}

fn http(name: &str, auth: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"http\"\nurl = \"https://{name}.example.test/mcp\"\n{auth}"
    )
}

/// An initialized environment with the fake `claude` as the only program on
/// `PATH`.
fn setup() -> TestEnv {
    let env = TestEnv::new();
    let bin = env.fake_claude_dir();
    let env = env.with_path(&[bin]);
    env.init();
    env
}

fn install(env: &TestEnv, text: &str) {
    let file = env.claude_config();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, text).unwrap();
}

fn select(env: &TestEnv, name: &str) {
    env.cmd()
        .args(["select", name, "--harness", "claude"])
        .assert()
        .success();
}

fn sync_json(env: &TestEnv, args: &[&str]) -> (i32, Value, String) {
    let mut all = vec!["sync"];
    all.extend_from_slice(args);
    let (code, value, stderr) = env.json(&all);
    let text = format!("{value}{stderr}");
    assert!(!text.contains("CANARY"), "a canary leaked: {text}");
    (code, value, stderr)
}

fn harness<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|harness| harness["harness"] == name)
        .unwrap_or_else(|| panic!("no harness `{name}` in {value:#}"))
}

fn claude(value: &Value) -> &Value {
    harness(value, "claude")
}

fn entry<'a>(value: &'a Value, name: &str) -> &'a Value {
    claude(value)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no entry `{name}` in {value:#}"))
}

fn op(value: &Value, name: &str) -> String {
    entry(value, name)["operation"].as_str().unwrap().to_owned()
}

fn outcome(value: &Value, name: &str) -> String {
    entry(value, name)["outcome"].as_str().unwrap().to_owned()
}

fn config(env: &TestEnv) -> Map<String, Value> {
    match serde_json::from_str(&fs::read_to_string(env.claude_config()).unwrap()).unwrap() {
        Value::Object(object) => object,
        other => panic!("not an object: {other}"),
    }
}

fn servers(env: &TestEnv) -> Map<String, Value> {
    config(env)
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn state(env: &TestEnv) -> String {
    fs::read_to_string(env.default_store().join("state.toml")).unwrap_or_default()
}

/// The fake's calls of one subcommand.
fn calls(env: &TestEnv, subcommand: &str) -> Vec<Value> {
    env.claude_calls()
        .into_iter()
        .filter(|call| call["args"][1] == subcommand)
        .collect()
}

fn canonical(path: impl AsRef<Path>) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn add_then_unchanged_keeps_every_other_key() {
    let env = setup();
    install(&env, &fixture("account.json"));
    let before = config(&env);
    env.write_local("github", GITHUB);
    env.write_local("linear", LINEAR);
    select(&env, "github");
    select(&env, "linear");
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(claude(&value)["status"], "synced");
    for name in ["github", "linear"] {
        assert_eq!(op(&value, name), "add");
        assert_eq!(outcome(&value, name), "applied");
        assert!(entry(&value, name)["hash"].as_str().unwrap().len() == 64);
        assert_eq!(entry(&value, name)["error"], Value::Null);
    }
    let servers = servers(&env);
    // The fake normalizes like Claude Code: it adds `args = []`.
    assert_eq!(
        servers["github"],
        json!({"command": "/usr/local/bin/github-mcp", "args": [],
               "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN:-}", "LOG_LEVEL": "info"}})
    );
    assert_eq!(
        servers["linear"],
        json!({"type": "http", "url": "https://mcp.linear.app/mcp",
               "oauth": {"clientId": "d-mcp-linear", "scopes": "read"}})
    );
    // Every other key is semantically unchanged.
    let mut after = config(&env);
    let managed = after["mcpServers"].as_object_mut().unwrap();
    managed.remove("github");
    managed.remove("linear");
    assert_eq!(after, before);
    // The unset client secret variable is a manual step of that write.
    let gaps = entry(&value, "linear")["gaps"].as_array().unwrap().clone();
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    assert_eq!(gaps[0]["field"], "auth.client_secret_env");
    // `github` exists in local scope of a project: the user entry is
    // shadowed there, and `add-json -s user` accepted the name.
    let notices = claude(&value)["notices"].as_array().unwrap();
    assert!(
        notices.iter().any(|notice| notice
            .as_str()
            .unwrap()
            .contains("`github` is shadowed by a local-scope entry in the project /work/app")),
        "{notices:?}"
    );
    assert!(state(&env).contains("harness = \"claude\""));
    assert!(state(&env).contains("written = true"));
    for call in calls(&env, "add-json") {
        assert_eq!(call["args"][2], "-s");
        assert_eq!(call["args"][3], "user");
    }

    // A second sync plans only Unchanged and calls nothing.
    let adds = calls(&env, "add-json").len();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "unchanged");
    assert_eq!(op(&value, "linear"), "unchanged");
    assert_eq!(calls(&env, "add-json").len(), adds);
    assert!(calls(&env, "remove").is_empty());
    assert!(calls(&env, "get").is_empty(), "sync never calls get");
}

#[test]
fn nothing_to_do_and_dry_runs_leave_the_file_byte_identical() {
    let env = setup();
    let text = fixture("account.json");
    install(&env, &text);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert!(claude(&value)["entries"].as_array().unwrap().is_empty());
    env.write_local("github", GITHUB);
    select(&env, "github");
    let before = snapshot(env.root());
    let (code, value, _) = sync_json(&env, &["--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(outcome(&value, "github"), "planned");
    assert_eq!(snapshot(env.root()), before);
    assert_eq!(fs::read_to_string(env.claude_config()).unwrap(), text);
    assert!(env.claude_calls().is_empty());
}

#[test]
fn a_failed_re_add_of_an_own_entry_is_rolled_back() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let old = servers(&env)["github"].clone();
    let owned = state(&env);
    env.write_local("github", GITHUB_V2);
    env.claude_mode(&["fail-once add-json github"]);
    let adds = calls(&env, "add-json").len();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(op(&value, "github"), "update");
    assert_eq!(outcome(&value, "github"), "rolled-back");
    assert_eq!(
        entry(&value, "github")["error"]["code"],
        "harness-cli-failed"
    );
    assert!(
        entry(&value, "github")["notice"]
            .as_str()
            .unwrap()
            .contains("previous entry was restored")
    );
    assert_eq!(claude(&value)["status"], "failed");
    // The failed add and the rollback.
    assert_eq!(calls(&env, "add-json").len(), adds + 2);
    assert_eq!(servers(&env)["github"], old);
    // The record reverted to the previous hashes, so the next sync updates.
    assert_eq!(
        strip_times(&state(&env)),
        strip_times(&owned),
        "the record is back to its previous hashes"
    );
    env.claude_mode(&[]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "update");
    assert_eq!(outcome(&value, "github"), "applied");
    assert_eq!(servers(&env)["github"]["args"], json!(["--v2"]));
}

fn strip_times(state: &str) -> String {
    state
        .lines()
        .filter(|line| !line.starts_with("time = "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_double_failure_is_lost_and_re_added_as_vanished() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    env.write_local("github", GITHUB_V2);
    env.claude_mode(&["fail add-json github"]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(outcome(&value, "github"), "lost");
    let notice = entry(&value, "github")["notice"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(notice.contains("fields: args, command, env"), "{notice}");
    assert!(!servers(&env).contains_key("github"));
    // The record is never dropped; it reverted to the previous hashes.
    assert!(
        state(&env).contains("status = \"owned\""),
        "{}",
        state(&env)
    );
    env.claude_mode(&[]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "vanished");
    assert_eq!(outcome(&value, "github"), "applied");
    assert_eq!(servers(&env)["github"]["args"], json!(["--v2"]));
}

#[test]
fn a_failed_re_add_after_a_take_over_is_never_rolled_back() {
    let env = setup();
    install(
        &env,
        r#"{"mcpServers": {"github": {"command": "/opt/hand-written", "env": {"T": "CANARY_CLAUDE_HAND"}}}}"#,
    );
    env.write_local("github", GITHUB);
    select(&env, "github");
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "github"), "collision");
    assert_eq!(entry(&value, "github")["hash"], Value::Null);
    env.claude_mode(&["fail add-json github"]);
    let (code, value, _) = sync_json(&env, &["--take-over", "github"]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(op(&value, "github"), "update");
    assert_eq!(outcome(&value, "github"), "lost");
    // Only the new rendering was tried: foreign content never reaches argv.
    let adds = calls(&env, "add-json");
    assert_eq!(adds.len(), 1);
    for call in env.claude_calls() {
        assert!(!call.to_string().contains("CANARY"), "{call}");
    }
    assert!(!servers(&env).contains_key("github"));
    // The taken-over record survives and the next sync re-adds the entry.
    assert!(state(&env).contains("status = \"owned\""));
    assert!(!state(&env).contains("written"), "{}", state(&env));
    env.claude_mode(&[]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "vanished");
}

#[test]
fn removals_verify_absence() {
    let env = setup();
    env.write_local("github", GITHUB);
    env.write_local("tools", &stdio("tools", "/usr/bin/tools"));
    select(&env, "github");
    select(&env, "tools");
    sync_json(&env, &[]);
    env.cmd().args(["unselect", "github"]).assert().success();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "remove");
    assert_eq!(outcome(&value, "github"), "applied");
    assert!(!servers(&env).contains_key("github"));
    assert!(!state(&env).contains("name = \"github\""));
    let removes = calls(&env, "remove");
    assert_eq!(removes.len(), 1);
    assert_eq!(
        removes[0]["args"],
        json!(["mcp", "remove", "-s", "user", "github"])
    );
    // A removal that does not take effect fails verification.
    env.claude_mode(&["noop remove tools"]);
    env.cmd().args(["unselect", "tools"]).assert().success();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3, "{value:#}");
    assert_eq!(outcome(&value, "tools"), "failed");
    assert_eq!(
        entry(&value, "tools")["error"]["code"],
        "harness-verify-failed"
    );
    assert!(servers(&env).contains_key("tools"));
    assert!(state(&env).contains("name = \"tools\""));
    assert!(state(&env).contains("status = \"owned\""));
    // A failing removal is a per-entry CLI failure, exit 1.
    env.claude_mode(&["fail remove tools"]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(
        entry(&value, "tools")["error"]["code"],
        "harness-cli-failed"
    );
    assert!(
        entry(&value, "tools")["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("`claude mcp remove` failed (exit status 1)")
    );
}

#[test]
fn drift_is_reported_also_when_it_happens_between_plan_and_remove() {
    let env = setup();
    env.write_local("beta", &stdio("beta", "/usr/bin/beta"));
    select(&env, "beta");
    sync_json(&env, &[]);
    // A hand edit: drift, no call.
    let mut edited = config(&env);
    edited["mcpServers"]["beta"]["timeout"] = json!(5000);
    install(
        &env,
        &serde_json::to_string(&Value::Object(edited)).unwrap(),
    );
    let before = env.claude_calls().len();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "beta"), "drift");
    assert_eq!(env.claude_calls().len(), before);
    // The take-over plans an Update (remove, then add) of the edited entry.
    sync_json(&env, &["--take-over", "beta"]);
    let removes = calls(&env, "remove").len();
    // Now `beta` changes while `alpha` is being added, after planning.
    env.write_local("beta", &stdio("beta", "/usr/bin/beta-v2"));
    env.write_local("alpha", &stdio("alpha", "/usr/bin/alpha"));
    select(&env, "alpha");
    env.claude_mode(&["edit-on add-json alpha beta"]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(outcome(&value, "alpha"), "applied");
    assert_eq!(op(&value, "beta"), "drift");
    assert_eq!(outcome(&value, "beta"), "blocked");
    assert!(
        entry(&value, "beta")["notice"]
            .as_str()
            .unwrap()
            .contains("changed outside d-mcp")
    );
    assert_eq!(calls(&env, "remove").len(), removes);
    assert_eq!(servers(&env)["beta"]["edited"], json!(true));
}

#[test]
fn collisions_and_identical_collisions() {
    let env = setup();
    install(
        &env,
        r#"{"mcpServers": {"github": {"command": "/opt/other"},
            "tools": {"command": "/usr/bin/tools", "args": [], "type": "stdio"}}}"#,
    );
    env.write_local("github", GITHUB);
    env.write_local("tools", &stdio("tools", "/usr/bin/tools"));
    select(&env, "github");
    select(&env, "tools");
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "github"), "collision");
    assert_eq!(op(&value, "tools"), "identical-collision");
    assert_eq!(entry(&value, "tools")["hash"], Value::Null);
    let (code, value, _) = sync_json(&env, &["--take-over", "tools"]);
    assert_eq!(code, 1);
    assert_eq!(op(&value, "tools"), "unchanged");
    assert_eq!(outcome(&value, "tools"), "recorded");
    assert!(calls(&env, "add-json").is_empty());
    assert!(calls(&env, "remove").is_empty());
}

#[test]
fn vanished_entries_are_re_added() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let mut edited = config(&env);
    edited["mcpServers"]
        .as_object_mut()
        .unwrap()
        .remove("github");
    install(
        &env,
        &serde_json::to_string(&Value::Object(edited)).unwrap(),
    );
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0);
    assert_eq!(op(&value, "github"), "vanished");
    assert!(
        entry(&value, "github")["notice"]
            .as_str()
            .unwrap()
            .contains("d-mcp unmanage github --harness claude")
    );
    assert!(servers(&env).contains_key("github"));
}

#[test]
fn a_missing_cli_fails_only_with_operations() {
    let env = TestEnv::new();
    env.init();
    install(&env, &fixture("account.json"));
    let text = fs::read_to_string(env.claude_config()).unwrap();
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    env.write_local("github", GITHUB);
    // Without operations: unchanged plus a notice, exit 0.
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(claude(&value)["status"], "synced");
    assert!(
        claude(&value)["notices"]
            .to_string()
            .contains("the harness CLI is not on PATH")
    );
    env.cmd().args(["select", "github"]).assert().success();
    // A dry run plans as usual.
    let (code, value, _) = sync_json(&env, &["--dry-run"]);
    assert_eq!(code, 0);
    assert_eq!(outcome(&value, "github"), "planned");
    // With operations: harness-cli-not-found, exit 4; Codex proceeds.
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 4, "{value:#}");
    assert_eq!(value["error"]["code"], "harness-cli-not-found");
    assert_eq!(claude(&value)["status"], "failed");
    assert_eq!(claude(&value)["error"]["code"], "harness-cli-not-found");
    assert_eq!(outcome(&value, "github"), "failed");
    assert_eq!(harness(&value, "codex")["entries"][0]["outcome"], "applied");
    assert_eq!(fs::read_to_string(env.claude_config()).unwrap(), text);
    // Nothing was journaled for Claude Code.
    assert!(!state(&env).contains("harness = \"claude\""));
}

#[test]
fn neither_file_nor_binary_is_not_present() {
    let env = TestEnv::new();
    env.init();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let before = snapshot(env.root());
    let (code, value, _) = sync_json(&env, &["--harness", "claude"]);
    assert_eq!(code, 0);
    assert_eq!(claude(&value)["status"], "not-present");
    let after: Vec<_> = snapshot(env.root())
        .into_iter()
        .filter(|(path, _)| path != &env.default_store().join("state.toml"))
        .collect();
    assert_eq!(after, before);
}

#[test]
fn a_missing_file_is_created_by_the_first_add() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    assert!(!env.claude_config().exists());
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(claude(&value)["created"], true);
    assert!(servers(&env).contains_key("github"));
}

#[test]
fn rendering_secrets_and_gaps() {
    let env = setup();
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    let definitions = [
        (
            "bearer",
            http(
                "bearer",
                "[auth]\ntype = \"bearer_env\"\nvar = \"API_KEY\"\noauth_supported = true\n",
            ),
        ),
        (
            "header",
            http(
                "header",
                "[auth]\ntype = \"header_env\"\nheader = \"X-Api-Key\"\nvar = \"MY_KEY\"\n",
            ),
        ),
        (
            "oauth",
            http(
                "oauth",
                "[auth]\ntype = \"oauth\"\nclient_id = \"cid\"\nscopes = [\"write\", \"read\"]\n\
                 client_secret_env = \"OAUTH_SECRET\"\n[harness.claude]\noauth_callback_port = 8123\n",
            ),
        ),
        ("open", http("open", "[auth]\ntype = \"none\"\n")),
        (
            "filtered",
            format!(
                "{}[harness.codex]\ndisabled_tools = [\"delete\"]\ncwd = \"/srv\"\n",
                stdio("filtered", "/usr/bin/filtered")
            ),
        ),
        (
            "worker",
            format!(
                "{}[env]\nTOKEN = {{ forward = true }}\n[harness.codex]\ncwd = \"/srv\"\n\
                 [harness.claude]\ntimeout = 20000\n",
                stdio("worker", "/usr/bin/worker")
            ),
        ),
    ];
    for (name, text) in &definitions {
        env.write_local(name, text);
        env.cmd().args(["select", name]).assert().success();
    }
    let output = env
        .cmd()
        .args(["--json", "sync"])
        .env("OAUTH_SECRET", "CANARY_CLIENT_SECRET_VALUE")
        .env("MCP_CLIENT_SECRET", "CANARY_INHERITED_SECRET")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stdout.contains("CANARY") && !stderr.contains("CANARY"),
        "{stdout}{stderr}"
    );
    let value: Value = serde_json::from_str(&stdout).unwrap();
    // The Codex tool filter blocks the entry for Claude Code only.
    assert_eq!(output.status.code(), Some(1), "{value:#}");
    assert_eq!(op(&value, "filtered"), "blocked");
    let gaps = entry(&value, "filtered")["gaps"]
        .as_array()
        .unwrap()
        .clone();
    assert!(gaps.iter().any(
        |gap| gap["field"] == "harness.codex.disabled_tools" && gap["severity"] == "important"
    ));
    assert!(gaps.iter().any(|gap| gap["field"] == "harness.codex.cwd"));
    let codex = harness(&value, "codex")["entries"].as_array().unwrap();
    assert!(codex.iter().all(|entry| entry["outcome"] == "applied"));
    let servers = servers(&env);
    assert!(!servers.contains_key("filtered"));
    assert_eq!(
        servers["bearer"]["headers"],
        json!({"Authorization": "Bearer ${API_KEY}"})
    );
    assert_eq!(
        servers["header"]["headers"],
        json!({"X-Api-Key": "${MY_KEY}"})
    );
    assert_eq!(
        servers["oauth"]["oauth"],
        json!({"clientId": "cid", "scopes": "read write", "callbackPort": 8123})
    );
    assert_eq!(
        servers["worker"],
        json!({"command": "/usr/bin/worker", "args": [], "env": {"TOKEN": "${TOKEN:-}"}, "timeout": 20000})
    );
    let note = |name: &str, field: &str| {
        entry(&value, name)["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap["field"] == field && gap["severity"] == "note")
    };
    assert!(note("open", "auth"));
    assert!(note("bearer", "auth.oauth_supported"));
    assert!(note("worker", "harness.codex.cwd"));
    // With the variable set there is no manual step.
    assert!(
        entry(&value, "oauth")["gaps"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The client secret is passed through the child environment only, and
    // the inherited `MCP_CLIENT_SECRET` never reaches the other calls.
    for call in calls(&env, "add-json") {
        let name = call["args"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .nth(1)
            .unwrap()
            .clone();
        let with_secret = call["args"]
            .as_array()
            .unwrap()
            .contains(&json!("--client-secret"));
        assert_eq!(with_secret, name == "oauth", "{call}");
        let expected = if name == "oauth" { "present" } else { "absent" };
        assert_eq!(call["secret"], expected, "{call}");
    }
    for text in [
        fs::read_to_string(env.root().join("fake-claude.log")).unwrap(),
        fs::read_to_string(env.claude_config()).unwrap(),
        state(&env),
        fs::read_to_string(env.codex_config()).unwrap(),
    ] {
        assert!(!text.contains("CANARY_CLIENT_SECRET_VALUE"));
        assert!(!text.contains("CANARY_INHERITED_SECRET"));
    }
}

#[test]
fn credential_and_reserved_names_are_blocked_for_claude_only() {
    let env = setup();
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    for (name, text) in [
        (
            "npm",
            http(
                "npm",
                "[auth]\ntype = \"bearer_env\"\nvar = \"NPM_TOKEN\"\n",
            ),
        ),
        (
            "proxy",
            http(
                "proxy",
                "[auth]\ntype = \"header_env\"\nheader = \"X-P\"\nvar = \"HTTPS_PROXY\"\n",
            ),
        ),
        (
            "anthropic",
            http(
                "anthropic",
                "[auth]\ntype = \"bearer_env\"\nvar = \"ANTHROPIC_API_KEY\"\n",
            ),
        ),
        (
            "apikey",
            http(
                "apikey",
                "[auth]\ntype = \"bearer_env\"\nvar = \"API_KEY\"\n",
            ),
        ),
        ("workspace", stdio("workspace", "/usr/bin/ws")),
    ] {
        env.write_local(name, &text);
        env.cmd().args(["select", name]).assert().success();
    }
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1, "{value:#}");
    for name in ["npm", "proxy", "anthropic", "workspace"] {
        assert_eq!(op(&value, name), "blocked", "{name}");
        let gap = &entry(&value, name)["gaps"][0];
        assert_eq!(gap["severity"], "important");
    }
    assert_eq!(entry(&value, "npm")["gaps"][0]["field"], "auth.var");
    assert_eq!(entry(&value, "workspace")["gaps"][0]["field"], "name");
    assert_eq!(outcome(&value, "apikey"), "applied");
    let codex = harness(&value, "codex")["entries"].as_array().unwrap();
    assert_eq!(codex.len(), 5);
    assert!(codex.iter().all(|entry| entry["outcome"] == "applied"));
    // The reserved name never reached the CLI.
    assert!(calls(&env, "add-json").iter().all(|call| {
        !call["args"]
            .as_array()
            .unwrap()
            .contains(&json!("workspace"))
    }));
    assert_eq!(servers(&env).keys().collect::<Vec<_>>(), ["apikey"]);
}

#[test]
fn override_fixtures_validate_by_rule_code() {
    let root = Path::new(FIXTURES).join("claude-overrides");
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
            let (definition, sites) =
                parse_definition_with_sites(&fs::read_to_string(&path).unwrap(), &stem).unwrap();
            let problem = validate_definition(&definition, &sites, registry()).unwrap_err();
            assert_eq!(problem.rule, rule, "{}", path.display());
            assert!(problem.line.is_some() && problem.column.is_some());
            checked += 1;
        }
    }
    assert!(checked >= 15);
}

#[test]
fn an_unknown_claude_override_holds_the_server_for_every_harness() {
    let env = setup();
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    env.write_local(
        "unknown",
        &fs::read_to_string(format!(
            "{FIXTURES}/claude-overrides/invalid/override-key/unknown.toml"
        ))
        .unwrap(),
    );
    env.cmd().args(["select", "unknown"]).assert().success();
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert!(claude(&value)["entries"].as_array().unwrap().is_empty());
    assert!(
        harness(&value, "codex")["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(value["problems"][0]["code"], "invalid-definition");
    assert_eq!(value["problems"][0]["rule"], "override-key");
}

#[test]
fn every_call_runs_in_a_controlled_child_environment() {
    for with_dir in [true, false] {
        let env = if with_dir {
            TestEnv::new()
        } else {
            TestEnv::new().without_claude_config_dir()
        };
        let bin = env.fake_claude_dir();
        let env = env.with_path(std::slice::from_ref(&bin));
        env.init();
        env.write_local("github", GITHUB);
        env.write_local("tools", &stdio("tools", "/usr/bin/tools"));
        select(&env, "github");
        select(&env, "tools");
        sync_json(&env, &[]);
        env.write_local("github", GITHUB_V2);
        env.cmd().args(["unselect", "tools"]).assert().success();
        let output = env
            .cmd()
            .arg("sync")
            .env("MCP_CLIENT_SECRET", "CANARY_INHERITED_SECRET")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        let calls = env.claude_calls();
        assert_eq!(calls.len(), 5, "{calls:?}");
        let store = canonical(env.default_store());
        for call in &calls {
            assert_eq!(canonical(call["cwd"].as_str().unwrap()), store, "{call}");
            assert_eq!(call["path"], json!(bin.to_string_lossy()), "{call}");
            assert_eq!(call["home"], json!(env.home().to_string_lossy()), "{call}");
            assert_eq!(call["autoupdater"], "1", "{call}");
            assert_eq!(call["secret"], "absent", "{call}");
            if with_dir {
                assert_eq!(
                    call["config_dir"],
                    json!(env.claude_dir().to_string_lossy())
                );
            } else {
                assert_eq!(call["config_dir"], Value::Null);
            }
        }
        assert!(env.claude_config().exists());
        // No project entry was created for the working directory.
        assert!(config(&env).get("projects").is_none());
    }
}

#[test]
fn the_fake_refuses_to_run_outside_a_test_root() {
    let env = TestEnv::new();
    let bin = env.fake_claude_dir();
    let unmarked = tempfile::tempdir().unwrap();
    fs::create_dir_all(unmarked.path().join("home")).unwrap();
    for home in [unmarked.path().join("home"), PathBuf::from("/")] {
        let output = std::process::Command::new(bin.join("claude"))
            .args(["mcp", "list"])
            .env("HOME", &home)
            .env_remove("CLAUDE_CONFIG_DIR")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(97), "{}", home.display());
    }
    // A CLAUDE_CONFIG_DIR outside the test root is refused as well.
    let output = std::process::Command::new(bin.join("claude"))
        .args(["mcp", "list"])
        .env("HOME", env.home())
        .env("CLAUDE_CONFIG_DIR", unmarked.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(97));
    assert!(!unmarked.path().join("fake-claude.log").exists());
    assert!(env.claude_calls().is_empty());
}

#[test]
fn invalid_and_unsupported_files_never_echo_content() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    install(&env, &fixture("invalid.json"));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3);
    assert_eq!(claude(&value)["error"]["code"], "harness-config-invalid");
    assert!(
        claude(&value)["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("not valid JSON at line 4, column")
    );
    let (code, stdout, stderr) = env.text(&["sync"]);
    assert_eq!(code, 3);
    assert!(!stdout.contains("CANARY") && !stderr.contains("CANARY"));
    assert!(!stdout.contains("expected"), "no serde message: {stdout}");
    install(&env, &fixture("array-root.json"));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 1);
    assert_eq!(claude(&value)["status"], "unsupported");
    assert!(
        claude(&value)["notices"][0]
            .as_str()
            .unwrap()
            .contains("`mcpServers`")
    );
    assert!(env.claude_calls().is_empty());
}

// ---------------------------------------------------------------------------
// Through the library API: crashes, timeouts, location changes, login state.

fn run_sync(
    env: &TestEnv,
    environment: &Environment,
    options: &SyncOptions<'_>,
) -> d_mcp::error::Result<(Value, Option<Error>)> {
    let paths = StorePaths::new(env.default_store());
    let mut warnings = Vec::new();
    sync(&paths, environment, options, &mut warnings).map(|(report, error)| (report.data, error))
}

fn harness_data<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|harness| harness["harness"] == name)
        .unwrap()
}

fn data_entry<'a>(data: &'a Value, name: &str) -> &'a Value {
    harness_data(data, "claude")["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap()
}

#[test]
fn a_crash_between_add_and_promotion_is_promoted_on_the_next_run() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let crash = || -> d_mcp::error::Result<()> {
        Err(Error::Io {
            operation: "simulate a crash before",
            path: PathBuf::from("/promotion"),
            kind: std::io::ErrorKind::Interrupted,
        })
    };
    let options = SyncOptions {
        hooks: SyncHooks {
            before_promote: Some(&crash),
            ..SyncHooks::default()
        },
        ..SyncOptions::default()
    };
    assert!(run_sync(&env, &env.environment(), &options).is_err());
    assert!(servers(&env).contains_key("github"));
    assert!(state(&env).contains("status = \"pending\""));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "unchanged");
    assert!(
        claude(&value)["notices"]
            .to_string()
            .contains("was completed")
    );
    assert!(state(&env).contains("status = \"owned\""));
}

#[test]
fn a_crash_after_the_removal_of_an_update_re_adds_the_entry() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    env.write_local("github", GITHUB_V2);
    let crash = || -> d_mcp::error::Result<()> {
        Err(Error::Io {
            operation: "simulate a crash after",
            path: PathBuf::from("/remove"),
            kind: std::io::ErrorKind::Interrupted,
        })
    };
    let options = SyncOptions {
        hooks: SyncHooks {
            adapter: Hooks {
                after_remove: Some(&crash),
                ..Hooks::default()
            },
            ..SyncHooks::default()
        },
        ..SyncOptions::default()
    };
    let (data, error) = run_sync(&env, &env.environment(), &options).unwrap();
    assert_eq!(error.unwrap().code(), "io-error");
    assert_eq!(data_entry(&data, "github")["outcome"], "failed");
    assert!(!servers(&env).contains_key("github"));
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "vanished");
    assert_eq!(servers(&env)["github"]["args"], json!(["--v2"]));
}

fn with_timeout<'a>() -> SyncOptions<'a> {
    SyncOptions {
        hooks: SyncHooks {
            adapter: Hooks {
                cli_timeout: Some(Duration::from_secs(5)),
                ..Hooks::default()
            },
            ..SyncHooks::default()
        },
        ..SyncOptions::default()
    }
}

#[test]
fn timeouts_are_settled_from_the_file_and_never_rolled_back() {
    let env = setup();
    env.write_local("github", GITHUB);
    env.write_local("linear", LINEAR);
    select(&env, "github");
    select(&env, "linear");
    // `github` is written before the timeout, `linear` is not.
    env.claude_mode(&["sleep-after add-json github", "sleep add-json linear"]);
    let (data, error) = run_sync(&env, &env.environment(), &with_timeout()).unwrap();
    assert_eq!(error.unwrap().code(), "harness-cli-timeout");
    assert_eq!(data_entry(&data, "github")["outcome"], "applied");
    assert_eq!(data_entry(&data, "linear")["outcome"], "failed");
    assert_eq!(
        data_entry(&data, "linear")["error"]["code"],
        "harness-cli-timeout"
    );
    assert!(state(&env).contains("name = \"github\""));
    assert!(!state(&env).contains("name = \"linear\""));
    assert!(!state(&env).contains("pending"));
    // An Update whose add times out without writing is lost, not rolled
    // back.
    env.claude_mode(&["sleep add-json github"]);
    env.cmd().args(["unselect", "linear"]).assert().success();
    env.write_local("github", GITHUB_V2);
    let adds = calls(&env, "add-json").len();
    let (data, _) = run_sync(&env, &env.environment(), &with_timeout()).unwrap();
    assert_eq!(data_entry(&data, "github")["outcome"], "lost");
    let after = calls(&env, "add-json");
    assert_eq!(after.len(), adds + 1, "{after:#?}");
    assert!(state(&env).contains("name = \"github\""));
    assert!(!state(&env).contains("pending"));
}

#[test]
fn a_changed_claude_config_dir_is_a_location_change() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let moved = env.root().join("claude-moved");
    fs::create_dir_all(&moved).unwrap();
    let environment = Environment {
        claude_config_dir: Some(moved.into_os_string()),
        ..env.environment()
    };
    let (data, error) = run_sync(&env, &environment, &SyncOptions::default()).unwrap();
    assert_eq!(error.unwrap().code(), "sync-blocked");
    assert_eq!(harness_data(&data, "claude")["status"], "location-changed");
    assert!(
        data_entry(&data, "github")["notice"]
            .as_str()
            .unwrap()
            .contains("CLAUDE_CONFIG_DIR")
    );
}

#[test]
fn login_state_reads_get_for_oauth_entries_only() {
    let env = setup();
    env.write_local("github", GITHUB);
    env.write_local("linear", LINEAR);
    env.write_local(
        "bearer",
        &http(
            "bearer",
            "[auth]\ntype = \"bearer_env\"\nvar = \"API_KEY\"\n",
        ),
    );
    for name in ["github", "linear", "bearer"] {
        select(&env, name);
    }
    sync_json(&env, &[]);
    assert!(calls(&env, "get").is_empty(), "sync never calls get");
    let names: Vec<String> = ["linear", "github", "bearer", "missing"]
        .map(str::to_owned)
        .to_vec();
    let runner = ProcessRunner::default();
    for (mode, expected) in [
        ("needs-authentication", LoginState::NeedsLogin),
        ("connected", LoginState::LoggedIn),
        ("failed", LoginState::Unknown),
    ] {
        env.claude_mode(&[&format!("login linear {mode}")]);
        let states = ClaudeAdapter.login_state(&env.environment(), &runner, &names);
        assert_eq!(states["linear"], expected, "{mode}");
        assert_eq!(states["github"], LoginState::NotApplicable);
        assert_eq!(states["bearer"], LoginState::NotApplicable);
        assert_eq!(states["missing"], LoginState::Unknown);
    }
    let gets = calls(&env, "get");
    assert_eq!(gets.len(), 3, "one call per query, for `linear` only");
    let store = canonical(env.default_store());
    for call in &gets {
        assert_eq!(call["args"], json!(["mcp", "get", "linear"]));
        assert_eq!(canonical(call["cwd"].as_str().unwrap()), store);
        assert_eq!(call["secret"], "absent");
    }
    // Without the binary every queried entry is unknown.
    let environment = Environment {
        path: Some(env.root().join("empty-bin").into_os_string()),
        ..env.environment()
    };
    let states = ClaudeAdapter.login_state(&environment, &runner, &names);
    assert_eq!(states["linear"], LoginState::Unknown);
    assert_eq!(states["github"], LoginState::NotApplicable);
    let names: BTreeSet<_> = states.keys().cloned().collect();
    assert_eq!(names.len(), 4);
}

#[test]
fn a_write_that_changes_nothing_fails_verification_and_leaves_the_file_byte_identical() {
    let env = setup();
    let text = fixture("account.json");
    install(&env, &text);
    env.write_local("github", GITHUB);
    select(&env, "github");
    env.claude_mode(&["noop add-json github"]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3, "{value:#}");
    assert_eq!(outcome(&value, "github"), "failed");
    assert_eq!(
        entry(&value, "github")["error"]["code"],
        "harness-verify-failed"
    );
    assert_eq!(calls(&env, "add-json").len(), 1);
    // The fake changed nothing: the file is byte-identical, and the pending
    // record is settled instead of left behind.
    assert_eq!(fs::read_to_string(env.claude_config()).unwrap(), text);
    assert!(!state(&env).contains("pending"), "{}", state(&env));
    assert!(!state(&env).contains("name = \"github\""));
    // Once the fake behaves, the next sync adds the entry.
    env.claude_mode(&[]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(op(&value, "github"), "add");
}

#[test]
fn a_write_that_reads_back_differently_fails_verification() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    env.claude_mode(&["alter add-json github"]);
    let (code, value, _) = sync_json(&env, &[]);
    assert_eq!(code, 3, "{value:#}");
    assert_eq!(
        entry(&value, "github")["error"]["code"],
        "harness-verify-failed"
    );
    assert!(!state(&env).contains("pending"), "{}", state(&env));
}

#[test]
fn a_missing_cli_leaves_managed_entries_unchanged_without_operations() {
    let env = setup();
    env.write_local("github", GITHUB);
    select(&env, "github");
    sync_json(&env, &[]);
    let text = fs::read_to_string(env.claude_config()).unwrap();
    let without = env.root().join("empty-bin");
    let output = env
        .cmd()
        .env("PATH", &without)
        .args(["--json", "sync", "--harness", "claude"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(claude(&value)["status"], "synced");
    assert_eq!(op(&value, "github"), "unchanged");
    assert_eq!(fs::read_to_string(env.claude_config()).unwrap(), text);
}

#[test]
fn no_neutral_working_directory_fails_before_any_call() {
    // The store root (under `HOME`) and the temporary directory (`TMPDIR`)
    // both lie below the test root; a `.mcp.json` or a project entry there
    // leaves no neutral working directory.
    for marker in ["mcp.json", "projects"] {
        let env = setup();
        env.write_local("github", GITHUB);
        select(&env, "github");
        let tmp = env.root().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        if marker == "mcp.json" {
            fs::write(env.root().join(".mcp.json"), "{}").unwrap();
        } else {
            let projects = json!({"projects": {env.root().to_string_lossy(): {}}});
            install(&env, &projects.to_string());
        }
        let output = env
            .cmd()
            .env("TMPDIR", &tmp)
            .args(["--json", "sync"])
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(1), "{value:#}");
        let error = &claude(&value)["error"];
        assert_eq!(error["code"], "harness-no-neutral-dir", "{value:#}");
        let hint = error["hint"].as_str().unwrap();
        assert!(
            hint.starts_with("no neutral working directory for `claude`: "),
            "{hint}"
        );
        assert!(
            hint.contains("has a `.mcp.json` or a `projects` entry of the claude configuration"),
            "{hint}"
        );
        assert!(hint.contains("nothing was run for claude"), "{hint}");
        assert!(!hint.contains("claude mcp mcp"), "{hint}");
        assert!(env.claude_calls().is_empty());
    }
}

/// An OAuth definition whose client secret comes from `OAUTH_SECRET`.
fn oauth(scopes: &str) -> String {
    http(
        "oauth",
        &format!(
            "[auth]\ntype = \"oauth\"\nclient_id = \"cid\"\nscopes = [{scopes}]\n\
             client_secret_env = \"OAUTH_SECRET\"\n"
        ),
    )
}

fn sync_with_secret(env: &TestEnv, secret: Option<&str>) -> (i32, Value) {
    let mut cmd = env.cmd();
    cmd.env_remove("OAUTH_SECRET");
    if let Some(secret) = secret {
        cmd.env("OAUTH_SECRET", secret);
    }
    let output = cmd.args(["--json", "sync"]).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("CANARY"), "{stdout}");
    (
        output.status.code().unwrap(),
        serde_json::from_str(&stdout).unwrap(),
    )
}

fn gap_reasons(value: &Value, name: &str) -> Vec<String> {
    entry(value, name)["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gap| format!("{}: {}", gap["field"].as_str().unwrap(), gap["reason"]))
        .collect()
}

#[test]
fn a_rolled_back_oauth_entry_reports_the_missing_client_secret_only() {
    // Without the variable: the add reports the manual step; the rollback
    // does not (the new entry is not in place), but the restored entry went
    // without a secret.
    let env = setup();
    env.write_local("oauth", &oauth("\"read\""));
    select(&env, "oauth");
    let (code, value) = sync_with_secret(&env, None);
    assert_eq!(code, 0, "{value:#}");
    let reasons = gap_reasons(&value, "oauth");
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("was added without a client secret")),
        "{reasons:?}"
    );
    env.write_local("oauth", &oauth("\"read\", \"write\""));
    env.claude_mode(&["fail-once add-json oauth"]);
    let (code, value) = sync_with_secret(&env, None);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(outcome(&value, "oauth"), "rolled-back");
    let reasons = gap_reasons(&value, "oauth");
    assert!(
        !reasons
            .iter()
            .any(|reason| reason.contains("was added without a client secret")),
        "{reasons:?}"
    );
    assert!(
        reasons.iter().any(|reason| reason.starts_with(
            "auth.client_secret_env: \"client secret needed; the restored entry has none"
        ) && reason.contains("set `OAUTH_SECRET`")),
        "{reasons:?}"
    );
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("re-login needed")),
        "{reasons:?}"
    );
    let rollback = calls(&env, "add-json").pop().unwrap();
    assert_eq!(rollback["secret"], "absent", "{rollback}");

    // With the variable set: the rollback passes the secret, so there is no
    // note about a missing one.
    let env = setup();
    env.write_local("oauth", &oauth("\"read\""));
    select(&env, "oauth");
    let (code, _) = sync_with_secret(&env, Some("CANARY_SECRET"));
    assert_eq!(code, 0);
    env.write_local("oauth", &oauth("\"read\", \"write\""));
    env.claude_mode(&["fail-once add-json oauth"]);
    let (code, value) = sync_with_secret(&env, Some("CANARY_SECRET"));
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(outcome(&value, "oauth"), "rolled-back");
    let reasons = gap_reasons(&value, "oauth");
    assert!(
        !reasons
            .iter()
            .any(|reason| reason.contains("client_secret_env")),
        "{reasons:?}"
    );
    let rollback = calls(&env, "add-json").pop().unwrap();
    assert_eq!(rollback["secret"], "present", "{rollback}");
    assert!(
        rollback["args"]
            .as_array()
            .unwrap()
            .contains(&json!("--client-secret"))
    );
}

#[test]
fn an_empty_claude_config_dir_skips_the_legacy_file() {
    let env = setup();
    let legacy = env.home().join(".claude/.config.json");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "{}").unwrap();
    env.write_local("github", GITHUB);
    select(&env, "github");
    let output = env
        .cmd()
        .env("CLAUDE_CONFIG_DIR", "")
        .args(["--json", "sync"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value:#}");
    assert_eq!(outcome(&value, "github"), "applied");
    // Like Claude Code 2.1.310: `$HOME/.claude.json`, not the legacy file.
    let file = env.home().join(".claude.json");
    assert_eq!(
        claude(&value)["file"],
        json!(file.to_string_lossy()),
        "{value:#}"
    );
    let written: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert!(written["mcpServers"]["github"].is_object());
    assert_eq!(fs::read_to_string(&legacy).unwrap(), "{}");
    // The child gets the empty value, not an unset variable.
    for call in env.claude_calls() {
        assert_eq!(call["config_dir"], "", "{call}");
    }
}

#[test]
fn an_empty_claude_config_dir_never_runs_claude_from_the_store() {
    // With an empty `CLAUDE_CONFIG_DIR`, Claude Code 2.1.310 treats its
    // working directory as its configuration home: it writes backups of the
    // whole file into `<cwd>/backups/` and prefers a `<cwd>/.config.json`.
    let env = setup();
    let store = env.default_store();
    fs::write(store.join(".config.json"), "{}").unwrap();
    let sync = |definition: &str| {
        env.write_local("github", definition);
        let output = env
            .cmd()
            .env("CLAUDE_CONFIG_DIR", "")
            .args(["--json", "sync"])
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output.status.code(), Some(0), "{value:#}");
        assert_eq!(outcome(&value, "github"), "applied", "{value:#}");
        value
    };
    select(&env, "github");
    sync(GITHUB);
    let before = snapshot(&store);
    // The update writes the existing file twice (remove, then add), so the
    // fake writes backups into its working directory.
    sync(GITHUB_V2);
    let file = env.home().join(".claude.json");
    let written: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert!(written["mcpServers"]["github"].is_object());
    assert_eq!(
        fs::read_to_string(store.join(".config.json")).unwrap(),
        "{}"
    );
    assert!(!store.join("backups").exists());
    // No new paths in the store; only d-mcp's own files may change.
    let paths = |entries: Vec<(std::path::PathBuf, Option<Vec<u8>>)>| -> Vec<_> {
        let mut paths: Vec<_> = entries.into_iter().map(|(path, _)| path).collect();
        paths.sort();
        paths
    };
    assert_eq!(paths(snapshot(&store)), paths(before));
    let calls = env.claude_calls();
    assert!(!calls.is_empty());
    for call in calls {
        assert_eq!(call["config_dir"], "", "{call}");
        let cwd = std::path::PathBuf::from(call["cwd"].as_str().unwrap());
        assert!(!cwd.starts_with(env.root()), "{call}");
        assert!(
            !cwd.exists(),
            "the temporary working directory is removed: {call}"
        );
    }
}

#[test]
fn the_fake_claude_lookup_ignores_stale_builds() {
    let built = common::fake_claude_binary();
    let name = built.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(name.starts_with("fake_claude-"), "{name}");
    let hash = name
        .strip_prefix("fake_claude-")
        .unwrap()
        .trim_end_matches(std::env::consts::EXE_SUFFIX);
    let dep_info = built.with_file_name(format!("fake_claude-{hash}.d"));
    let modified = fs::metadata(&built).unwrap().modified().unwrap();
    assert!(common::is_fresh(&dep_info, modified));
    // A build older than its sources is stale.
    assert!(!common::is_fresh(&dep_info, std::time::UNIX_EPOCH));
    // So is one without dep-info.
    assert!(!common::is_fresh(
        &built.with_file_name("fake_claude-missing.d"),
        modified
    ));
}
