//! Approval and audit: the resolve-time gate, `approve`, `accept-risk`,
//! `revoke`, `source trust`, `select --all-from`, and how `resolve`,
//! `status`, `inventory` and `sync` agree on held and revoked servers.
//! Every remote is a local bare repository; no test uses the network.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Remote, TestEnv, snapshot, stdio};
use serde_json::Value;

/// A clean definition: `npx` with an exact version.
fn pinned(name: &str, version: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"{name}-mcp@{version}\"]\n"
    )
}

/// A definition with an audit finding: `uvx` without a version.
fn unpinned(name: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"uvx\"\nargs = [\"{name}-mcp\"]\n"
    )
}

/// A store with Codex present, and a remote `personal` offering `github`
/// (clean) and `fetch` (unpinned), registered as an untrusted Git source.
fn setup() -> (TestEnv, Remote) {
    let env = TestEnv::new().with_git();
    env.init();
    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    let remote = env.remote("personal");
    remote.write("servers/github.toml", pinned("github", "1.0.0"));
    remote.write("servers/fetch.toml", unpinned("fetch"));
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

fn server<'v>(value: &'v Value, name: &str) -> &'v Value {
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == name)
        .unwrap_or_else(|| panic!("no server {name} in {value}"))
}

fn resolved(env: &TestEnv, name: &str) -> Value {
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0, "{value}");
    server(&value, name).clone()
}

/// `(status, reason)` of a server in `resolve`.
fn state(env: &TestEnv, name: &str) -> (String, Value) {
    let server = resolved(env, name);
    (
        server["status"].as_str().unwrap().to_owned(),
        server["reason"].clone(),
    )
}

fn hash_of(env: &TestEnv, name: &str) -> String {
    resolved(env, name)["definition_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn select_codex(env: &TestEnv, name: &str) {
    env.cmd()
        .args(["select", name, "--harness", "codex"])
        .assert()
        .success();
}

fn sync_entry<'v>(value: &'v Value, name: &str) -> &'v Value {
    value["data"]["harnesses"][0]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no entry {name} in {value:#}"))
}

fn sync(env: &TestEnv, args: &[&str]) -> Value {
    let mut all = vec!["sync", "--harness", "codex"];
    all.extend_from_slice(args);
    let (code, value, err) = env.json(&all);
    assert_eq!(code, 0, "{value:#} {err}");
    value
}

fn approve(env: &TestEnv, name: &str) {
    let hash = hash_of(env, name);
    let (code, value, err) = env.json(&["approve", name, "--expect", &hash]);
    assert_eq!(code, 0, "{value} {err}");
}

fn codex_config(env: &TestEnv) -> String {
    fs::read_to_string(env.codex_config()).unwrap()
}

/// Runs a command and asserts that the store stays byte-identical.
fn assert_store_unchanged(env: &TestEnv, args: &[&str], code: i32) -> Value {
    let before = snapshot(&store(env));
    let (actual, value, err) = env.json(args);
    assert_eq!(actual, code, "{args:?}: {value} {err}");
    assert_eq!(snapshot(&store(env)), before, "{args:?} changed the store");
    value
}

#[test]
fn pending_until_approved_then_updated() {
    let (env, remote) = setup();
    select_codex(&env, "github");
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "hold");
    assert_eq!(server["reason"], "pending-approval");
    assert_eq!(server["source"], "personal");
    assert_eq!(server["findings"], serde_json::json!([]));
    let hash = server["definition_hash"].as_str().unwrap().to_owned();
    // Nothing is planned or written for a pending server d-mcp does not own.
    let value = sync(&env, &[]);
    assert_eq!(
        value["data"]["harnesses"][0]["entries"],
        serde_json::json!([])
    );
    assert_eq!(codex_config(&env), "");

    // `--expect` is required, and must match.
    let (code, value, _) = env.json(&["approve", "github"]);
    assert_eq!((code, error_code(&value)), (1, "expect-required"));
    let (code, value, _) = env.json(&["approve", "github", "--expect", &"0".repeat(64)]);
    assert_eq!((code, error_code(&value)), (3, "approval-changed"));
    let preview = assert_store_unchanged(&env, &["--dry-run", "approve", "github"], 0);
    assert_eq!(preview["data"]["definition_hash"], hash.as_str());
    assert_eq!(preview["data"]["view"]["command"], "npx");
    assert_eq!(
        preview["data"]["view"]["args"],
        serde_json::json!(["-y", "github-mcp@1.0.0"])
    );
    assert_eq!(preview["data"]["newly_approved"], true);
    assert_eq!(preview["data"]["approved"], false);
    let (code, value, _) = env.json(&["approve", "github", "--expect", &hash]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["approved"], true);
    assert_eq!(state(&env, "github"), ("resolved".to_owned(), Value::Null));
    // Approving again is a no-op.
    let (code, value, _) = env.json(&["approve", "github", "--expect", &hash]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["newly_approved"], false);
    let value = sync(&env, &[]);
    assert_eq!(sync_entry(&value, "github")["operation"], "add");
    let synced = codex_config(&env);
    assert!(synced.contains("github-mcp@1.0.0"), "{synced}");

    // Upstream changes: the refresh moves the pin, but approves nothing.
    remote.write("servers/github.toml", pinned("github", "1.1.0"));
    remote.commit("bump");
    let preview = assert_store_unchanged(&env, &["--dry-run", "source", "refresh", "personal"], 0);
    let changes = preview["data"]["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{preview}");
    assert_eq!(changes[0]["server"], "github");
    assert_eq!(changes[0]["change"], "changed");
    assert_eq!(changes[0]["previous_hash"], hash.as_str());
    assert_eq!(changes[0]["approval"], "pending-approval");
    assert_eq!(
        changes[0]["view"]["args"],
        serde_json::json!(["-y", "github-mcp@1.1.0"])
    );
    let (code, value, err) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["commit"], remote.head().as_str());
    assert_eq!(value["data"]["rewrite"], false);
    assert_eq!(value["data"]["recloned"], false);
    assert_eq!(
        state(&env, "github"),
        ("hold".to_owned(), "pending-approval".into())
    );
    let new_hash = hash_of(&env, "github");
    assert_ne!(new_hash, hash);
    // Sync holds the existing entry byte-identical until approval.
    let value = sync(&env, &[]);
    assert_eq!(sync_entry(&value, "github")["operation"], "held");
    assert_eq!(sync_entry(&value, "github")["reason"], "pending-approval");
    assert_eq!(codex_config(&env), synced);
    // A refresh between preview and approve: the old hash is stale.
    let (code, value, _) = env.json(&["approve", "github", "--expect", &hash]);
    assert_eq!((code, error_code(&value)), (3, "approval-changed"));
    approve(&env, "github");
    let value = sync(&env, &[]);
    assert_eq!(sync_entry(&value, "github")["operation"], "update");
    assert!(codex_config(&env).contains("github-mcp@1.1.0"));

    // A refresh without upstream change writes nothing new.
    let (code, value, _) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["changes"], serde_json::json!([]));
    let (code, text, _) = env.text(&["source", "refresh", "personal"]);
    assert_eq!(code, 0);
    assert!(text.contains("is up to date"), "{text}");
}

#[test]
fn revoke_removes_without_fall_through() {
    let (env, _remote) = setup();
    // A lower-priority directory definition of the same name.
    env.write_local("github", &stdio("github", "local-github"));
    select_codex(&env, "github");
    approve(&env, "github");
    sync(&env, &[]);
    assert!(codex_config(&env).contains("[mcp_servers.github]"));

    let preview = assert_store_unchanged(&env, &["--dry-run", "revoke", "github"], 0);
    assert_eq!(preview["data"]["had_approval"], true);
    assert_eq!(preview["data"]["source"], "personal");
    let (code, value, _) = env.json(&["revoke", "github"]);
    assert_eq!(code, 0, "{value}");
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "revoked");
    assert_eq!(server["source"], "personal");
    assert!(server["definition_hash"].is_string());
    let value = sync(&env, &["--dry-run"]);
    assert_eq!(sync_entry(&value, "github")["operation"], "remove");
    let value = sync(&env, &[]);
    assert_eq!(sync_entry(&value, "github")["operation"], "remove");
    let config = codex_config(&env);
    assert!(!config.contains("github"), "{config}");
    // The local definition does not take its place.
    let value = sync(&env, &[]);
    assert!(
        value["data"]["harnesses"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["name"] != "github")
    );
    assert!(!codex_config(&env).contains("local-github"));
    // Revoking again changes nothing; a later approval clears the marker.
    let (code, value, _) = env.json(&["revoke", "github", "--source", "personal"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["had_approval"], false);
    approve(&env, "github");
    assert_eq!(state(&env, "github"), ("resolved".to_owned(), Value::Null));
    sync(&env, &[]);
    assert!(codex_config(&env).contains("github-mcp@1.0.0"));
    let approvals = fs::read_to_string(store(&env).join("approvals.toml")).unwrap();
    assert!(!approvals.contains("[[revoked]]"), "{approvals}");
}

#[test]
fn risk_acceptance_is_bound_to_the_findings() {
    let (env, _remote) = setup();
    select_codex(&env, "fetch");
    let server = resolved(&env, "fetch");
    assert_eq!(server["reason"], "pending-approval");
    assert_eq!(server["findings"][0]["rule"], "unpinned-python-package");
    assert_eq!(server["findings"][0]["launcher"], "uvx");
    assert_eq!(
        server["findings"][0]["args"],
        serde_json::json!(["fetch-mcp"])
    );
    let hash = server["definition_hash"].as_str().unwrap().to_owned();
    let (_, value, _) = env.json(&["resolve"]);
    assert!(
        value["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|problem| problem["code"] == "audit-finding" && problem["server"] == "fetch")
    );

    // Findings block the approval until they are accepted.
    let value = assert_store_unchanged(&env, &["approve", "fetch", "--expect", &hash], 1);
    assert_eq!(error_code(&value), "approval-blocked");
    assert_eq!(value["data"]["blocked"], "risk-not-accepted");
    let (code, value, _) = env.json(&["accept-risk", "fetch", "--reason", "upstream is ours"]);
    assert_eq!((code, error_code(&value)), (1, "expect-required"));
    let (code, value, _) = env.json(&[
        "accept-risk",
        "fetch",
        "--reason",
        "upstream is ours",
        "--expect",
        &"1".repeat(64),
    ]);
    assert_eq!((code, error_code(&value)), (3, "approval-changed"));
    let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    let long = "x".repeat(501);
    for reason in [secret, long.as_str(), "", "two\nlines"] {
        let (code, stdout, stderr) = env.text(&[
            "accept-risk",
            "fetch",
            "--reason",
            reason,
            "--expect",
            &hash,
        ]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stderr.contains("[reason-invalid]"), "{stderr}");
        if !reason.is_empty() {
            assert!(!stdout.contains(reason) && !stderr.contains(reason));
        }
    }
    assert_store_unchanged(
        &env,
        &[
            "--dry-run",
            "accept-risk",
            "fetch",
            "--reason",
            "upstream is ours",
        ],
        0,
    );
    let (code, value, _) = env.json(&[
        "accept-risk",
        "fetch",
        "--reason",
        "upstream is ours",
        "--expect",
        &hash,
    ]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["approved"], false);
    // Acceptance does not approve.
    assert_eq!(
        state(&env, "fetch"),
        ("hold".to_owned(), "pending-approval".into())
    );
    let approvals = fs::read_to_string(store(&env).join("approvals.toml")).unwrap();
    assert!(approvals.contains("reason = \"upstream is ours\""));
    approve(&env, "fetch");
    assert_eq!(state(&env, "fetch"), ("resolved".to_owned(), Value::Null));
    sync(&env, &[]);
    let synced = codex_config(&env);
    assert!(synced.contains("[mcp_servers.fetch]"));

    // New audit rules change the findings hash: the acceptance stops
    // matching, and the entry is held unchanged.
    let path = store(&env).join("approvals.toml");
    let text = fs::read_to_string(&path).unwrap();
    let start = text.find("findings_hash = \"").unwrap() + "findings_hash = \"".len();
    let changed = format!(
        "{}{}{}",
        &text[..start],
        "e".repeat(64),
        &text[start + 64..]
    );
    fs::write(&path, changed).unwrap();
    assert_eq!(
        state(&env, "fetch"),
        ("hold".to_owned(), "risk-not-accepted".into())
    );
    let value = sync(&env, &[]);
    assert_eq!(sync_entry(&value, "fetch")["reason"], "risk-not-accepted");
    assert_eq!(codex_config(&env), synced);
    // Re-accepting the current findings restores it.
    let (code, _, _) = env.json(&[
        "accept-risk",
        "fetch",
        "--reason",
        "re-checked",
        "--expect",
        &hash,
    ]);
    assert_eq!(code, 0);
    assert_eq!(state(&env, "fetch"), ("resolved".to_owned(), Value::Null));
    // A server without findings has nothing to accept.
    let (code, value, _) = env.json(&["accept-risk", "github", "--reason", "x", "--expect", &hash]);
    assert_eq!((code, error_code(&value)), (1, "approval-not-needed"));
}

#[test]
fn trust_and_all_from() {
    let (env, remote) = setup();
    remote.write("servers/broken.toml", "name = ");
    remote.commit("broken");
    env.cmd()
        .args(["source", "refresh", "personal"])
        .assert()
        .success();
    let pin = remote.head();
    // Untrusted: refused.
    let (code, value, _) = env.json(&["approve", "--all-from", "personal", "--expect", &pin]);
    assert_eq!((code, error_code(&value)), (1, "approval-blocked"));
    let (code, _, err) = env.text(&["approve", "--all-from", "personal", "--expect", &pin]);
    assert_eq!(code, 1);
    assert!(err.contains("not trusted"), "{err}");
    let value = assert_store_unchanged(
        &env,
        &["--dry-run", "source", "trust", "personal", "--on"],
        0,
    );
    assert_eq!(value["data"]["changed"], true);
    let (code, value, _) = env.json(&["source", "trust", "personal", "--on"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["source"]["trusted"], true);
    let (code, value, _) = env.json(&["source", "trust", "personal", "--on"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["changed"], false);

    let (code, value, _) = env.json(&["approve", "--all-from", "personal"]);
    assert_eq!((code, error_code(&value)), (1, "expect-required"));
    let (code, value, _) = env.json(&[
        "approve",
        "--all-from",
        "personal",
        "--expect",
        &"0".repeat(40),
    ]);
    assert_eq!((code, error_code(&value)), (3, "approval-changed"));
    let preview =
        assert_store_unchanged(&env, &["--dry-run", "approve", "--all-from", "personal"], 1);
    assert_eq!(error_code(&preview), "approval-blocked");
    let (code, value, _) = env.json(&["approve", "--all-from", "personal", "--expect", &pin]);
    assert_eq!((code, error_code(&value)), (1, "approval-blocked"));
    let approved: Vec<&str> = value["data"]["approved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["server"].as_str().unwrap())
        .collect();
    assert_eq!(approved, ["github"]);
    let blocked: Vec<(&str, &str)> = value["data"]["blocked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["server"].as_str().unwrap(),
                item["reason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        blocked,
        [
            ("broken", "invalid-definition"),
            ("fetch", "risk-not-accepted")
        ]
    );
    let (_, text, _) = env.text(&["--dry-run", "approve", "--all-from", "personal"]);
    assert!(
        text.contains(&format!("approved github: {}", hash_of_offered(&env))),
        "{text}"
    );

    // `select --all-from` selects every offered server not yet selected.
    select_codex(&env, "fetch");
    assert_store_unchanged(&env, &["--dry-run", "select", "--all-from", "personal"], 0);
    let (code, value, _) = env.json(&["select", "--all-from", "personal"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["data"]["selected"],
        serde_json::json!(["broken", "github"])
    );
    assert_eq!(
        value["data"]["already_selected"],
        serde_json::json!(["fetch"])
    );
    let config = fs::read_to_string(store(&env).join("config.toml")).unwrap();
    // The existing codex-only selection is kept.
    assert!(config.contains("harnesses = [\"codex\"]"), "{config}");
    let (code, value, _) = env.json(&["select", "--all-from", "personal"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["selected"], serde_json::json!([]));
    assert_eq!(state(&env, "github"), ("resolved".to_owned(), Value::Null));
    let (code, value, _) = env.json(&["select", "--all-from", "nope"]);
    assert_eq!((code, error_code(&value)), (1, "source-not-found"));

    let (code, value, _) = env.json(&["source", "trust", "personal", "--off"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["source"]["trusted"], false);
    let (code, value, _) = env.json(&["source", "trust", "nope", "--on"]);
    assert_eq!((code, error_code(&value)), (1, "source-not-found"));
}

fn hash_of_offered(env: &TestEnv) -> String {
    let (_, value, _) = env.json(&["--dry-run", "approve", "github"]);
    value["data"]["definition_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn directory_sources_need_no_approval_and_findings_only_warn() {
    let env = TestEnv::new();
    env.init();
    env.write_local("local-fetch", &unpinned("local-fetch"));
    env.cmd().args(["select", "local-fetch"]).assert().success();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    let server = server(&value, "local-fetch");
    assert_eq!(server["status"], "resolved");
    assert_eq!(server["findings"], Value::Null);
    let problem = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|problem| problem["code"] == "audit-finding")
        .expect("an audit warning");
    assert_eq!(problem["source"], "local");
    assert!(
        problem["message"]
            .as_str()
            .unwrap()
            .contains("unpinned-python-package")
    );
    for args in [
        &["approve", "local-fetch", "--expect", "x"][..],
        &["approve", "local-fetch", "--source", "local"],
        &["revoke", "local-fetch"],
        &["accept-risk", "local-fetch", "--reason", "ok"],
        &["approve", "nobody", "--expect", "x"],
        &["approve", "--all-from", "local", "--expect", "x"],
    ] {
        let (code, value, _) = env.json(args);
        assert_eq!(
            (code, error_code(&value)),
            (1, "approval-not-needed"),
            "{args:?}"
        );
    }
    let (code, value, _) = env.json(&["approve", "x", "--source", "nope", "--expect", "x"]);
    assert_eq!((code, error_code(&value)), (1, "source-not-found"));
    let (code, value, _) = env.json(&["approve", "--all-from", "nope", "--expect", "x"]);
    assert_eq!((code, error_code(&value)), (1, "source-not-found"));
    // Trust and refresh have no effect on a directory source.
    let team = env.source_dir("team");
    env.cmd()
        .args([
            "source",
            "add",
            team.to_str().unwrap(),
            "--id",
            "team",
            "--priority",
            "3",
        ])
        .assert()
        .success();
    let (code, value, err) = env.json(&["source", "trust", "team", "--on"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["changed"], false);
    assert!(err.contains("no effect"), "{err}");
    let (code, value, err) = env.json(&["source", "refresh", "team"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["commit"], Value::Null);
    assert!(err.contains("nothing to refresh"), "{err}");
    let (code, value, _) = env.json(&["source", "trust", "local", "--on"]);
    assert_eq!((code, error_code(&value)), (1, "source-id-reserved"));
}

#[test]
fn rollback_and_formatting_changes_need_no_new_approval() {
    let (env, remote) = setup();
    select_codex(&env, "github");
    approve(&env, "github");
    let first = hash_of(&env, "github");
    // A formatting-only change keeps the hash: nothing is listed.
    remote.write(
        "servers/github.toml",
        format!("# comment\n\n{}", pinned("github", "1.0.0")),
    );
    remote.commit("format");
    let (code, value, _) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["changes"], serde_json::json!([]));
    assert_eq!(hash_of(&env, "github"), first);
    assert_eq!(state(&env, "github"), ("resolved".to_owned(), Value::Null));
    // Version two, approved; then a rollback commit to version one.
    remote.write("servers/github.toml", pinned("github", "2.0.0"));
    remote.commit("two");
    env.cmd()
        .args(["source", "refresh", "personal"])
        .assert()
        .success();
    approve(&env, "github");
    remote.write("servers/github.toml", pinned("github", "1.0.0"));
    remote.commit("rollback");
    let (code, value, _) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["changes"][0]["approval"], "approved");
    assert_eq!(state(&env, "github"), ("resolved".to_owned(), Value::Null));
    assert_eq!(hash_of(&env, "github"), first);
}

#[test]
fn resolve_status_inventory_and_sync_agree() {
    let (env, remote) = setup();
    remote.write("servers/linear.toml", pinned("linear", "1.0.0"));
    remote.write("servers/notion.toml", pinned("notion", "1.0.0"));
    remote.commit("linear");
    env.cmd()
        .args(["source", "refresh", "personal"])
        .assert()
        .success();
    for name in ["github", "fetch", "linear", "notion"] {
        select_codex(&env, name);
    }
    // All three approved and synced (`fetch` after accepting its risk).
    let fetch = hash_of(&env, "fetch");
    env.cmd()
        .args([
            "accept-risk",
            "fetch",
            "--reason",
            "ours",
            "--expect",
            &fetch,
        ])
        .assert()
        .success();
    for name in ["github", "fetch", "linear"] {
        approve(&env, name);
    }
    sync(&env, &[]);
    // github: pending after an upstream change; fetch: risk not accepted;
    // linear: revoked.
    remote.write("servers/github.toml", pinned("github", "3.0.0"));
    remote.commit("bump");
    env.cmd()
        .args(["source", "refresh", "personal"])
        .assert()
        .success();
    let path = store(&env).join("approvals.toml");
    let text = fs::read_to_string(&path).unwrap();
    let start = text.find("findings_hash = \"").unwrap() + "findings_hash = \"".len();
    fs::write(
        &path,
        format!(
            "{}{}{}",
            &text[..start],
            "e".repeat(64),
            &text[start + 64..]
        ),
    )
    .unwrap();
    env.cmd().args(["revoke", "linear"]).assert().success();
    // notion: selected and revoked before it was ever approved or synced,
    // so it has no record and no entry.
    env.cmd().args(["revoke", "notion"]).assert().success();

    let expected = [
        ("github", "hold", Some("pending-approval"), "held"),
        ("fetch", "hold", Some("risk-not-accepted"), "held"),
        ("linear", "revoked", None, "remove"),
    ];
    let (_, resolve, _) = env.json(&["resolve"]);
    let (code, status, _) = env.json(&["status"]);
    assert_eq!(code, 0);
    let (code, inventory, _) = env.json(&["inventory"]);
    assert_eq!(code, 0);
    let dry = sync(&env, &["--dry-run"]);
    let status_entry = |name: &str| {
        status["data"]["harnesses"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap()
            .clone()
    };
    let inventory_entry = |name: &str| {
        inventory["data"]["harnesses"][0]["scopes"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap()
            .clone()
    };
    for (name, status_value, reason, operation) in expected {
        let resolved = server(&resolve, name);
        assert_eq!(resolved["status"], status_value, "{name}");
        assert_eq!(resolved["reason"].as_str(), reason, "{name}");
        assert!(resolved["definition_hash"].is_string(), "{name}");
        let entry = status_entry(name);
        let desired = if status_value == "hold" {
            "held"
        } else {
            "revoked"
        };
        assert_eq!(entry["desired"], desired, "{name}");
        assert_eq!(entry["reason"].as_str(), reason, "{name}");
        assert_eq!(entry["source"], "personal", "{name}");
        assert_eq!(entry["operation"], operation, "{name}");
        let planned = sync_entry(&dry, name);
        assert_eq!(planned["operation"], operation, "{name}");
        assert_eq!(planned["reason"].as_str(), reason, "{name}");
        // The inventory classifies the owned, unchanged entries by content.
        assert_eq!(inventory_entry(name)["class"], "managed", "{name}");
    }
    // A selected, revoked server without a record is still listed by
    // status, with nothing for the next sync to do.
    assert_eq!(server(&resolve, "notion")["status"], "revoked");
    let notion = status_entry("notion");
    assert_eq!(notion["desired"], "revoked", "{notion}");
    assert_eq!(notion["recorded"], "none", "{notion}");
    assert_eq!(notion["source"], "personal", "{notion}");
    assert_eq!(notion["operation"], Value::Null, "{notion}");
    assert!(
        dry["data"]["harnesses"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["name"] != "notion"),
        "{dry}"
    );
}

#[test]
fn a_definition_removed_upstream_is_listed_and_the_owned_entry_is_kept() {
    let (env, remote) = setup();
    select_codex(&env, "github");
    approve(&env, "github");
    sync(&env, &[]);
    let synced = codex_config(&env);
    assert!(synced.contains("[mcp_servers.github]"), "{synced}");

    remote.remove("servers/github.toml");
    remote.commit("drop github");
    let preview = assert_store_unchanged(&env, &["--dry-run", "source", "refresh", "personal"], 0);
    let changes = preview["data"]["changes"].as_array().unwrap();
    let removed: Vec<&Value> = changes
        .iter()
        .filter(|change| change["server"] == "github")
        .collect();
    assert_eq!(removed.len(), 1, "{preview}");
    assert_eq!(removed[0]["change"], "removed");
    let (code, value, err) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0, "{value} {err}");

    // Selected but not offered: held, and the owned entry stays unchanged.
    let server = resolved(&env, "github");
    assert_eq!(server["status"], "not-offered", "{server}");
    let value = sync(&env, &[]);
    assert_eq!(sync_entry(&value, "github")["operation"], "held");
    assert_eq!(sync_entry(&value, "github")["reason"], "not-offered");
    assert_eq!(codex_config(&env), synced);
}
