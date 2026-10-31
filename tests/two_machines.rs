//! The personal multi-machine flow: machine 1 turns its built-in `local`
//! source into a Git repository and pushes it; machine 2 runs exactly the
//! documented setup sequence (`source add-git --trusted [--commit]`,
//! `approve --all-from --expect`, `select --all-from`, `sync`) and ends up
//! with the same desired servers and the same rendered harness entries.
//! Two stores, two isolated harness homes and one local bare repository; no
//! test uses the network.

mod common;

use std::collections::BTreeMap;
use std::fs;

use common::{Remote, TestEnv};
use serde_json::Value;

/// A clean stdio definition: `npx` with an exact version and a forwarded
/// variable.
const GITHUB: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n\
                      args = [\"-y\", \"@modelcontextprotocol/server-github@2025.4.8\"]\n\
                      [env]\nGITHUB_TOKEN = { forward = true }\n";

/// A clean remote definition with OAuth.
const LINEAR: &str = "name = \"linear\"\n[transport]\ntype = \"http\"\n\
                      url = \"https://mcp.linear.app/mcp\"\n[auth]\ntype = \"oauth\"\n";

/// A machine: an initialized store, Codex present, and `git` plus the fake
/// `claude` as the only programs on `PATH`.
fn machine() -> TestEnv {
    let env = TestEnv::new().with_git();
    let git = env.git_bin();
    let claude = env.fake_claude_dir();
    let env = env.with_path(&[git, claude]);
    env.init();
    fs::write(env.codex_config(), "").unwrap();
    env
}

fn ok(env: &TestEnv, args: &[&str]) -> Value {
    let (code, value, err) = env.json(args);
    assert_eq!(code, 0, "{args:?}: {value:#}\n{err}");
    value
}

/// Machine 1: two definitions in `local`, selected for every harness and
/// synced; then `local` becomes a Git repository pushed to a bare remote.
/// Returns the remote and the pushed commit.
fn first_machine(env: &TestEnv) -> (Remote, String) {
    env.write_local("github", GITHUB);
    env.write_local("linear", LINEAR);
    ok(env, &["select", "github"]);
    ok(env, &["select", "linear"]);
    ok(env, &["sync"]);
    // By hand, as documented: d-mcp never commits or pushes.
    let remote = env.remote("personal");
    let local = env.default_store().join("local");
    remote.git_in(&local, &["init", "-q", "-b", "main"]);
    remote.git_in(&local, &["add", "servers"]);
    remote.git_in(&local, &["commit", "-q", "-m", "MCP servers"]);
    remote.git_in(&local, &["remote", "add", "origin", &remote.url()]);
    remote.git_in(&local, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    let commit = remote
        .git_in(&local, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    (remote, commit)
}

/// `name → (status, harnesses, definition_hash)` from `resolve --json`.
fn desired(env: &TestEnv) -> BTreeMap<String, (Value, Value, Value)> {
    let value = ok(env, &["resolve"]);
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|server| {
            (
                server["name"].as_str().unwrap().to_owned(),
                (
                    server["status"].clone(),
                    server["harnesses"].clone(),
                    server["definition_hash"].clone(),
                ),
            )
        })
        .collect()
}

/// The user-scope `mcpServers` of the fake Claude Code configuration.
fn claude_servers(env: &TestEnv) -> Value {
    let text = fs::read_to_string(env.claude_config()).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["mcpServers"].clone()
}

fn codex_config(env: &TestEnv) -> String {
    fs::read_to_string(env.codex_config()).unwrap()
}

#[test]
fn a_second_machine_converges_with_one_setup_run_and_one_sync() {
    let one = machine();
    let (remote, commit) = first_machine(&one);
    let two = machine();
    let url = remote.url();

    // The documented sequence, exactly.
    let added = ok(
        &two,
        &["source", "add-git", &url, "--trusted", "--commit", &commit],
    );
    let source = &added["data"]["source"];
    let id = source["id"].as_str().unwrap().to_owned();
    assert_eq!(id, "personal");
    assert_eq!(source["commit"], commit.as_str());
    assert_eq!(source["trusted"], true);
    let approved = ok(&two, &["approve", "--all-from", &id, "--expect", &commit]);
    assert_eq!(approved["data"]["blocked"], serde_json::json!([]));
    assert_eq!(approved["data"]["approved"].as_array().unwrap().len(), 2);
    let selected = ok(&two, &["select", "--all-from", &id]);
    assert_eq!(
        selected["data"]["selected"],
        serde_json::json!(["github", "linear"])
    );
    ok(&two, &["sync"]);

    // The same desired servers: name, harnesses and definition hash.
    let desired_one = desired(&one);
    assert_eq!(desired_one.len(), 2);
    for (status, harnesses, hash) in desired_one.values() {
        assert_eq!(status, "resolved");
        assert_eq!(harnesses, &serde_json::json!(["all"]));
        assert!(hash.is_string());
    }
    assert_eq!(desired_one, desired(&two));
    // The same rendered entries in both isolated harness homes.
    assert!(codex_config(&one).contains("[mcp_servers.github]"));
    assert_eq!(codex_config(&one), codex_config(&two));
    let claude = claude_servers(&one);
    assert!(claude["github"].is_object() && claude["linear"].is_object());
    assert_eq!(claude, claude_servers(&two));
    // A second sync on machine 2 has nothing left to do.
    let value = ok(&two, &["sync", "--dry-run"]);
    for harness in value["data"]["harnesses"].as_array().unwrap() {
        for entry in harness["entries"].as_array().unwrap() {
            assert_eq!(entry["operation"], "unchanged", "{entry}");
        }
    }
}

#[test]
fn without_commit_the_second_machine_pins_the_head_of_the_ref() {
    let one = machine();
    let (remote, commit) = first_machine(&one);
    let two = machine();
    let added = ok(&two, &["source", "add-git", &remote.url(), "--trusted"]);
    assert_eq!(added["data"]["source"]["commit"], commit.as_str());
    assert_eq!(added["data"]["source"]["ref"], "main");
}

/// Writes a hand-made Codex entry `github` on machine 2.
fn hand_written_github(env: &TestEnv) {
    fs::write(
        env.codex_config(),
        "[mcp_servers.github]\ncommand = \"github-mcp\"\n",
    )
    .unwrap();
}

fn adopt_refusals(env: &TestEnv, entry: &str) -> (i32, Vec<String>) {
    let (code, value, _) = env.json(&["adopt", entry]);
    let data = &value["data"];
    let mut reasons: Vec<String> = data["refusals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|refusal| refusal["reason"].as_str().unwrap().to_owned())
        .collect();
    for server in data["servers"].as_array().unwrap() {
        reasons.extend(
            server["refusals"]
                .as_array()
                .unwrap()
                .iter()
                .map(|refusal| refusal["reason"].as_str().unwrap().to_owned()),
        );
    }
    (code, reasons)
}

#[test]
fn adopt_on_the_second_machine_is_refused_while_the_git_source_wins() {
    let one = machine();
    let (remote, commit) = first_machine(&one);
    let two = machine();
    hand_written_github(&two);
    ok(
        &two,
        &[
            "source",
            "add-git",
            &remote.url(),
            "--trusted",
            "--commit",
            &commit,
        ],
    );
    ok(
        &two,
        &["approve", "--all-from", "personal", "--expect", &commit],
    );
    let store = two.default_store();
    let before = common::snapshot(&store);
    let (code, reasons) = adopt_refusals(&two, "codex:user:github");
    assert_eq!(code, 1);
    assert_eq!(reasons, ["not-winning"]);
    assert_eq!(common::snapshot(&store), before);
    assert!(!store.join("local/servers/github.toml").exists());
}
