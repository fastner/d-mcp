//! Hardening of the approval commands and the gate: invalid names never
//! reach `approvals.toml`, a revoke removes what was revoked in every state
//! of the source, revoked markers are bound to the source URL for the
//! orphan report, an approval under a new URL starts from scratch, and the
//! new audit rules (`execution-env`, `unparsed-launcher`) block a Git-source
//! definition until its risk is accepted. Every remote is a local bare
//! repository; no test uses the network.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Remote, TestEnv, snapshot, stdio};
use serde_json::Value;

fn pinned(name: &str, version: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"{name}-mcp@{version}\"]\n"
    )
}

fn unpinned(name: &str, package: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"uvx\"\nargs = [\"{package}\"]\n"
    )
}

/// A store with Codex present and a remote `personal` offering `github`
/// (clean) and `fetch` (unpinned), registered as a Git source.
fn setup() -> (TestEnv, Remote) {
    let env = TestEnv::new().with_git();
    env.init();
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    let remote = env.remote("personal");
    remote.write("servers/github.toml", pinned("github", "1.0.0"));
    remote.write("servers/fetch.toml", unpinned("fetch", "fetch-mcp"));
    remote.commit("first");
    let (code, value, err) = env.json(&["source", "add-git", &remote.url(), "--id", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    (env, remote)
}

fn store(env: &TestEnv) -> PathBuf {
    env.default_store()
}

fn error_code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or_default()
}

fn resolved(env: &TestEnv, name: &str) -> Value {
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0, "{value}");
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == name)
        .unwrap_or_else(|| panic!("no server {name} in {value}"))
        .clone()
}

fn hash_of(env: &TestEnv, name: &str) -> String {
    resolved(env, name)["definition_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn problems<'v>(value: &'v Value, code: &str) -> Vec<&'v Value> {
    value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|problem| problem["code"] == code)
        .collect()
}

fn select_codex(env: &TestEnv, name: &str) {
    env.cmd()
        .args(["select", name, "--harness", "codex"])
        .assert()
        .success();
}

fn sync(env: &TestEnv) -> Value {
    let (code, value, err) = env.json(&["sync", "--harness", "codex"]);
    assert_eq!(code, 0, "{value:#} {err}");
    value
}

fn sync_operation(value: &Value, name: &str) -> String {
    value["data"]["harnesses"][0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no entry {name} in {value:#}"))["operation"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn approve(env: &TestEnv, name: &str) {
    let hash = hash_of(env, name);
    let (code, value, err) = env.json(&["approve", name, "--expect", &hash]);
    assert_eq!(code, 0, "{value} {err}");
}

fn accept(env: &TestEnv, name: &str) {
    let hash = hash_of(env, name);
    let (code, value, err) = env.json(&[
        "accept-risk",
        name,
        "--reason",
        "reviewed upstream",
        "--expect",
        &hash,
    ]);
    assert_eq!(code, 0, "{value} {err}");
}

fn codex_config(env: &TestEnv) -> String {
    fs::read_to_string(env.codex_config()).unwrap()
}

/// `github` approved, selected for Codex and synced.
fn installed() -> (TestEnv, Remote) {
    let (env, remote) = setup();
    select_codex(&env, "github");
    approve(&env, "github");
    sync(&env);
    assert!(codex_config(&env).contains("[mcp_servers.github]"));
    (env, remote)
}

#[test]
fn invalid_names_are_refused_before_anything_is_written() {
    let (env, _remote) = setup();
    let before = snapshot(&store(&env));
    let commands: [&[&str]; 9] = [
        &["revoke", "Bad Name", "--source", "personal"],
        &["revoke", "github", "--source", "Bad Source"],
        &["approve", "Bad Name", "--expect", "x"],
        &[
            "approve",
            "github",
            "--source",
            "Bad Source",
            "--expect",
            "x",
        ],
        &["approve", "--all-from", "Bad Source", "--expect", "x"],
        &["accept-risk", "Bad Name", "--reason", "r", "--expect", "x"],
        &["select", "--all-from", "Bad Source"],
        &["source", "trust", "Bad Source", "--on"],
        &["--dry-run", "revoke", "../x", "--source", "personal"],
    ];
    for args in commands {
        let (code, value, err) = env.json(args);
        assert_eq!(
            (code, error_code(&value)),
            (1, "invalid-name"),
            "{args:?} {err}"
        );
        assert_eq!(snapshot(&store(&env)), before, "{args:?} changed the store");
    }
    // Every command still works afterwards.
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0, "{value}");
    let (code, value, _) = env.json(&["doctor"]);
    assert_eq!(code, 0, "{value}");
    assert!(
        value["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|check| check["severity"] != "error" || check["check"] != "approvals"),
        "{value:#}"
    );
}

#[test]
fn unknown_names_are_refused_or_revoked_ahead() {
    let (env, _remote) = setup();
    // Without `--source` nothing offers it: refused, nothing written.
    let before = snapshot(&store(&env));
    for args in [
        vec!["revoke", "nobody"],
        vec!["approve", "nobody", "--expect", "x"],
        vec!["accept-risk", "nobody", "--reason", "r", "--expect", "x"],
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(
            (code, error_code(&value)),
            (1, "approval-not-needed"),
            "{args:?}"
        );
    }
    let (code, value, _) = env.json(&["revoke", "github", "--source", "nosuch"]);
    assert_eq!((code, error_code(&value)), (1, "source-not-found"));
    assert_eq!(snapshot(&store(&env)), before);
    // With `--source`, a valid name the source does not offer (yet) is
    // revoked ahead; the file stays valid.
    let (code, value, _) = env.json(&["revoke", "nobody", "--source", "personal"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["had_approval"], false);
    let approvals = fs::read_to_string(store(&env).join("approvals.toml")).unwrap();
    assert!(approvals.contains("server = \"nobody\""), "{approvals}");
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0, "{value}");
    assert!(
        problems(&value, "orphaned-approval").is_empty(),
        "{value:#}"
    );
}

#[test]
fn a_revoked_server_is_removed_when_its_definition_turns_invalid() {
    let (env, remote) = installed();
    let (code, value, _) = env.json(&["revoke", "github"]);
    assert_eq!(code, 0, "{value}");
    // Before the next sync, upstream breaks the definition.
    remote.write("servers/github.toml", "name = \"github\"\n[transport\n");
    remote.commit("break github");
    let (code, value, err) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "revoked", "{server}");
    assert_eq!(server["source"], "personal");
    assert_eq!(server["file"], "servers/github.toml");
    let value = sync(&env);
    assert_eq!(sync_operation(&value, "github"), "remove");
    assert!(!codex_config(&env).contains("github"));
}

#[test]
fn a_revoked_server_is_removed_when_it_is_deleted_upstream() {
    let (env, remote) = installed();
    let (code, value, _) = env.json(&["revoke", "github"]);
    assert_eq!(code, 0, "{value}");
    remote.remove("servers/github.toml");
    remote.commit("drop github");
    let (code, value, err) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "revoked", "{server}");
    assert_eq!(server["source"], "personal");
    let value = sync(&env);
    assert_eq!(sync_operation(&value, "github"), "remove");
    assert!(!codex_config(&env).contains("github"));
}

#[test]
fn a_revoked_server_is_removed_when_its_source_is_unavailable() {
    let (env, _remote) = installed();
    let (code, value, _) = env.json(&["revoke", "github"]);
    assert_eq!(code, 0, "{value}");
    // A missing snapshot makes the source unavailable.
    fs::remove_dir_all(store(&env).join("sources/personal/snapshot")).unwrap();
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "revoked", "{server}");
    let value = sync(&env);
    assert_eq!(sync_operation(&value, "github"), "remove");
    assert!(!codex_config(&env).contains("github"));
}

#[test]
fn a_revoked_server_is_removed_after_its_source_is_removed() {
    let (env, _remote) = installed();
    let (code, value, _) = env.json(&["revoke", "github"]);
    assert_eq!(code, 0, "{value}");
    // Before the next sync, the source goes away; its marker stays.
    let (code, value, err) = env.json(&["source", "remove", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "revoked", "{server}");
    assert_eq!(server["source"], "personal");
    let (_, value, _) = env.json(&["resolve"]);
    assert!(problems(&value, "not-offered").is_empty(), "{value:#}");
    assert_eq!(problems(&value, "orphaned-approval").len(), 1, "{value:#}");
    let value = sync(&env);
    assert_eq!(sync_operation(&value, "github"), "remove");
    assert!(!codex_config(&env).contains("github"));
    // A directory definition of the name resolves again: the marker only
    // revokes what nothing offers.
    env.write_local("github", &stdio("github", "local-github"));
    assert_eq!(resolved(&env, "github")["status"], "resolved");
}

#[test]
fn revoked_markers_are_bound_to_the_url_for_the_orphan_report() {
    let (env, remote) = setup();
    select_codex(&env, "github");
    let (code, value, _) = env.json(&["revoke", "github", "--source", "personal"]);
    assert_eq!(code, 0, "{value}");
    let approvals = fs::read_to_string(store(&env).join("approvals.toml")).unwrap();
    assert!(
        approvals.contains(&format!("url = \"{}\"", remote.url())),
        "{approvals}"
    );
    let (_, value, _) = env.json(&["doctor"]);
    let approvals_checks: Vec<&Value> = value["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["check"] == "approvals")
        .collect();
    assert_eq!(approvals_checks.len(), 1);
    assert_eq!(approvals_checks[0]["severity"], "ok");
    assert!(
        approvals_checks[0]["message"]
            .as_str()
            .unwrap()
            .contains("revoked marker")
    );

    // The same id for another repository: the marker still blocks (a
    // revoke is never undone by re-adding a source), and it is reported.
    let other = env.remote("elsewhere");
    other.write("servers/github.toml", pinned("github", "2.0.0"));
    other.commit("other");
    let (code, value, _) = env.json(&["source", "remove", "personal"]);
    assert_eq!(code, 0, "{value}");
    let (code, value, err) = env.json(&["source", "add-git", &other.url(), "--id", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(resolved(&env, "github")["status"], "revoked");
    let (_, value, _) = env.json(&["resolve"]);
    let orphaned = problems(&value, "orphaned-approval");
    assert_eq!(orphaned.len(), 1, "{value:#}");
    assert_eq!(orphaned[0]["server"], "github");
    assert!(
        orphaned[0]["message"]
            .as_str()
            .unwrap()
            .contains("revoked marker")
    );
    let (_, value, _) = env.json(&["doctor"]);
    let check = value["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["code"] == "orphaned-approval")
        .unwrap_or_else(|| panic!("{value:#}"));
    assert_eq!(check["severity"], "warn");
    assert!(check["hint"].as_str().unwrap().contains("still blocks"));
    // Approving under the new URL clears the marker; nothing is orphaned.
    approve(&env, "github");
    assert_eq!(resolved(&env, "github")["status"], "resolved");
    let (_, value, _) = env.json(&["resolve"]);
    assert!(
        problems(&value, "orphaned-approval").is_empty(),
        "{value:#}"
    );
}

#[test]
fn approving_under_a_new_url_drops_the_risks_of_the_old_one() {
    let (env, _remote) = setup();
    select_codex(&env, "fetch");
    accept(&env, "fetch");
    approve(&env, "fetch");
    // The same id for another repository, with a changed definition.
    let other = env.remote("elsewhere");
    other.write("servers/fetch.toml", unpinned("fetch", "fetch-mcp-next"));
    other.commit("other");
    let (code, value, _) = env.json(&["source", "remove", "personal"]);
    assert_eq!(code, 0, "{value}");
    let (code, value, err) = env.json(&["source", "add-git", &other.url(), "--id", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    let (_, value, _) = env.json(&["resolve"]);
    assert_eq!(problems(&value, "orphaned-approval").len(), 1, "{value:#}");
    accept(&env, "fetch");
    approve(&env, "fetch");
    // The old approval was replaced and the old risk dropped: nothing
    // orphaned stays behind.
    let (_, value, _) = env.json(&["resolve"]);
    assert!(
        problems(&value, "orphaned-approval").is_empty(),
        "{value:#}"
    );
    let approvals = fs::read_to_string(store(&env).join("approvals.toml")).unwrap();
    assert_eq!(
        approvals.matches("[[accepted_risks]]").count(),
        1,
        "{approvals}"
    );
}

#[test]
fn the_approve_dry_run_prints_the_source_to_apply_with() {
    let (env, _remote) = setup();
    select_codex(&env, "github");
    let hash = hash_of(&env, "github");
    let (code, stdout, stderr) = env.text(&["--dry-run", "approve", "github"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains(&format!("apply with --source personal --expect {hash}")),
        "{stdout}"
    );
    // The printed command applies.
    let (code, value, _) = env.json(&[
        "approve", "github", "--source", "personal", "--expect", &hash,
    ]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["newly_approved"], true);
}

#[test]
fn execution_env_and_unparsed_launchers_block_until_accepted() {
    let (env, remote) = setup();
    let canary = "CANARY-require-hook";
    remote.write(
        "servers/hooked.toml",
        format!(
            "name = \"hooked\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"hooked-mcp@1.0.0\"]\n[env]\nNODE_OPTIONS = {{ value = \"--require {canary}\" }}\n"
        ),
    );
    remote.write(
        "servers/published.toml",
        "name = \"published\"\n[transport]\ntype = \"stdio\"\ncommand = \"docker\"\nargs = [\"run\", \"-dp\", \"127.0.0.1:3000:3000\", \"evil\"]\n",
    );
    remote.commit("hooks");
    let (code, value, err) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    assert!(!value.to_string().contains(canary));
    select_codex(&env, "hooked");
    select_codex(&env, "published");
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    assert!(!value.to_string().contains(canary), "{value:#}");
    let hooked = resolved(&env, "hooked");
    assert_eq!(hooked["findings"][0]["rule"], "execution-env");
    assert_eq!(hooked["findings"][0]["launcher"], "[env]");
    assert_eq!(
        hooked["findings"][0]["args"],
        serde_json::json!(["NODE_OPTIONS"])
    );
    let published = resolved(&env, "published");
    assert_eq!(published["findings"][0]["rule"], "unparsed-launcher");
    assert_eq!(published["findings"][0]["args"], serde_json::json!(["-dp"]));
    for name in ["hooked", "published"] {
        let hash = hash_of(&env, name);
        let (code, value, _) = env.json(&["approve", name, "--expect", &hash]);
        assert_eq!(
            (code, error_code(&value)),
            (1, "approval-blocked"),
            "{name}"
        );
        let (code, stdout, stderr) = env.text(&["--dry-run", "approve", name]);
        assert_eq!(code, 1);
        assert!(!stdout.contains(canary) && !stderr.contains(canary));
        accept(&env, name);
        approve(&env, name);
        assert_eq!(resolved(&env, name)["status"], "resolved", "{name}");
    }
}
