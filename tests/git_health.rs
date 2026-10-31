//! Git sources in the read-only health views: the doctor checks `lock`,
//! `approvals` (orphaned approvals), `git` and `pending-approval`; the
//! `orphaned-approval` problem of `resolve` and `sync` with its URL binding;
//! and how `status` and `inventory` show held and revoked servers in text
//! form. Every remote is a local bare repository; no test uses the network.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Remote, TestEnv, snapshot};
use serde_json::Value;

/// A clean definition: `npx` with an exact version.
fn pinned(name: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"{name}-mcp@1.0.0\"]\n"
    )
}

/// A definition with an audit finding: `uvx` without a version.
fn unpinned(name: &str) -> String {
    format!(
        "name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"uvx\"\nargs = [\"{name}-mcp\"]\n"
    )
}

/// A store with Codex present and the Git source `personal` offering
/// `github` (clean) and `fetch` (unpinned), both selected for Codex.
fn setup() -> (TestEnv, Remote) {
    let env = TestEnv::new().with_git();
    env.init();
    fs::write(env.codex_config(), "").unwrap();
    let remote = env.remote("personal");
    remote.write("servers/github.toml", pinned("github"));
    remote.write("servers/fetch.toml", unpinned("fetch"));
    remote.commit("first");
    ok(
        &env,
        &["source", "add-git", &remote.url(), "--id", "personal"],
    );
    for name in ["github", "fetch"] {
        ok(&env, &["select", name, "--harness", "codex"]);
    }
    (env, remote)
}

fn store(env: &TestEnv) -> PathBuf {
    env.default_store()
}

fn ok(env: &TestEnv, args: &[&str]) -> Value {
    let (code, value, err) = env.json(args);
    assert_eq!(code, 0, "{args:?}: {value:#}\n{err}");
    value
}

fn hash_of(env: &TestEnv, name: &str) -> String {
    let value = ok(env, &["resolve"]);
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == name)
        .and_then(|server| server["definition_hash"].as_str())
        .unwrap_or_else(|| panic!("no hash for {name}: {value:#}"))
        .to_owned()
}

fn approve(env: &TestEnv, name: &str) {
    let hash = hash_of(env, name);
    ok(env, &["approve", name, "--expect", &hash]);
}

fn accept_and_approve_fetch(env: &TestEnv) {
    let hash = hash_of(env, "fetch");
    ok(
        env,
        &[
            "accept-risk",
            "fetch",
            "--reason",
            "ours",
            "--expect",
            &hash,
        ],
    );
    approve(env, "fetch");
}

/// The doctor checks of one kind.
fn checks<'v>(value: &'v Value, id: &str) -> Vec<&'v Value> {
    value["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["check"] == id)
        .collect()
}

/// The one check of a kind.
fn check<'v>(value: &'v Value, id: &str) -> &'v Value {
    let found = checks(value, id);
    assert_eq!(found.len(), 1, "{id}: {value:#}");
    found[0]
}

fn doctor(env: &TestEnv) -> (i32, Value) {
    let (code, value, _) = env.json(&["doctor"]);
    (code, value)
}

/// `(code, server, source)` of every problem of a `resolve`.
fn problems(env: &TestEnv) -> Vec<(String, Value, Value)> {
    let value = ok(env, &["resolve"]);
    value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| {
            (
                problem["code"].as_str().unwrap().to_owned(),
                problem["server"].clone(),
                problem["source"].clone(),
            )
        })
        .collect()
}

fn orphaned(env: &TestEnv) -> Vec<String> {
    problems(env)
        .into_iter()
        .filter(|(code, _, _)| code == "orphaned-approval")
        .map(|(_, server, source)| {
            format!("{}/{}", source.as_str().unwrap(), server.as_str().unwrap())
        })
        .collect()
}

#[test]
fn doctor_reports_git_lock_approvals_and_pending_approval() {
    let (env, _remote) = setup();
    let (code, value) = doctor(&env);
    assert_eq!(code, 0, "{value:#}");
    let lock = check(&value, "lock");
    assert_eq!(lock["severity"], "ok");
    assert!(lock["message"].as_str().unwrap().contains("1 pin"));
    assert_eq!(check(&value, "approvals")["severity"], "ok");
    let git = check(&value, "git");
    assert_eq!(git["severity"], "ok");
    assert!(git["message"].as_str().unwrap().contains("`personal`"));
    // Both selected servers wait for approval; one `warn` each.
    let pending = checks(&value, "pending-approval");
    let servers: Vec<&str> = pending
        .iter()
        .map(|check| check["server"].as_str().unwrap())
        .collect();
    assert_eq!(servers, ["fetch", "github"]);
    for check in &pending {
        assert_eq!(check["severity"], "warn");
        assert!(
            check["hint"].as_str().unwrap().contains("--dry-run"),
            "{check}"
        );
    }

    // Approved and accepted: nothing waits.
    approve(&env, "github");
    accept_and_approve_fetch(&env);
    let (_, value) = doctor(&env);
    let pending = check(&value, "pending-approval");
    assert_eq!(pending["severity"], "ok");

    // An acceptance whose findings hash no longer matches: risk not accepted.
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
    let (code, value) = doctor(&env);
    assert_eq!(code, 0);
    let pending = check(&value, "pending-approval");
    assert_eq!(pending["severity"], "warn");
    assert_eq!(pending["server"], "fetch");
    assert!(
        pending["message"]
            .as_str()
            .unwrap()
            .contains("audit findings")
    );
    assert!(
        pending["hint"]
            .as_str()
            .unwrap()
            .contains("accept-risk fetch")
    );

    // A revoked server is the user's decision, not a pending approval.
    ok(&env, &["revoke", "fetch"]);
    let (_, value) = doctor(&env);
    assert_eq!(check(&value, "pending-approval")["severity"], "ok");
}

#[test]
fn doctor_warns_when_git_is_missing_and_never_runs_it() {
    let (env, _remote) = setup();
    let before = snapshot(&store(&env));
    let output = env
        .cmd()
        .env("PATH", env.root().join("empty-bin"))
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let git = check(&value, "git");
    assert_eq!(git["severity"], "warn");
    assert_eq!(git["code"], "git-not-found");
    assert!(git["hint"].as_str().unwrap().contains("source refresh"));
    // Read-only work goes on without git.
    let output = env
        .cmd()
        .env("PATH", env.root().join("empty-bin"))
        .args(["--json", "resolve"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(snapshot(&store(&env)), before);

    // Without a Git source, a missing git is fine.
    let plain = TestEnv::new();
    plain.init();
    let (code, value) = doctor(&plain);
    assert_eq!(code, 0, "{value:#}");
    let git = check(&value, "git");
    assert_eq!(git["severity"], "ok");
    assert!(git["message"].as_str().unwrap().contains("not needed"));
    assert_eq!(check(&value, "lock")["severity"], "ok");
    assert_eq!(check(&value, "approvals")["severity"], "ok");
    assert_eq!(check(&value, "pending-approval")["severity"], "ok");
}

#[test]
fn doctor_reports_corrupt_lock_and_approvals_as_errors() {
    let (env, _remote) = setup();
    approve(&env, "github");
    let lock = store(&env).join("lock.toml");
    let good = fs::read_to_string(&lock).unwrap();
    let corrupt = "schema_version = 1\n[[pins]]\nsource = \"personal\"\ncommit = \"main\"\n";
    fs::write(&lock, corrupt).unwrap();
    let (code, value) = doctor(&env);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(value["error"]["code"], "doctor-failed");
    let check_lock = check(&value, "lock");
    assert_eq!(check_lock["severity"], "error");
    assert_eq!(check_lock["code"], "lock-corrupt");
    assert!(check_lock["hint"].as_str().unwrap().contains("by hand"));
    // The checks that need the store are left out; discovery did not fail.
    for id in [
        "harness-config",
        "drift",
        "pending-approval",
        "static-token",
        "discovery",
    ] {
        assert!(checks(&value, id).is_empty(), "{id}: {value:#}");
    }
    assert_eq!(value["data"]["summary"]["error"], 1);
    assert_eq!(fs::read_to_string(&lock).unwrap(), corrupt);

    fs::write(&lock, good).unwrap();
    let approvals = store(&env).join("approvals.toml");
    fs::write(&approvals, "schema_version = 1\nbogus = 1\n").unwrap();
    let (code, value) = doctor(&env);
    assert_eq!(code, 1, "{value:#}");
    let check_approvals = check(&value, "approvals");
    assert_eq!(check_approvals["severity"], "error");
    assert_eq!(check_approvals["code"], "approvals-corrupt");
    assert_eq!(check(&value, "lock")["severity"], "ok");
    assert!(checks(&value, "drift").is_empty());
    assert_eq!(
        fs::read_to_string(&approvals).unwrap(),
        "schema_version = 1\nbogus = 1\n"
    );
}

#[test]
fn orphaned_approvals_are_reported_and_bound_to_the_url() {
    let (env, remote) = setup();
    approve(&env, "github");
    accept_and_approve_fetch(&env);
    assert!(orphaned(&env).is_empty());

    // Removing the source keeps its approvals; they are orphaned.
    ok(&env, &["source", "remove", "personal"]);
    assert_eq!(orphaned(&env), ["personal/fetch", "personal/github"]);
    let (code, value, _) = env.json(&["sync", "--harness", "codex", "--dry-run"]);
    assert_eq!(code, 0, "{value:#}");
    let codes: Vec<&str> = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"orphaned-approval"), "{codes:?}");
    let (code, value) = doctor(&env);
    assert_eq!(code, 0, "{value:#}");
    let found = checks(&value, "approvals");
    assert_eq!(found.len(), 2, "{value:#}");
    for check in found {
        assert_eq!(check["severity"], "warn");
        assert_eq!(check["code"], "orphaned-approval");
        assert!(check["message"].as_str().unwrap().contains("`personal`"));
    }
    let (_, text, _) = env.text(&["resolve"]);
    assert!(text.contains("orphaned"), "{text}");

    // Re-added with the same URL, the approvals apply again.
    ok(
        &env,
        &["source", "add-git", &remote.url(), "--id", "personal"],
    );
    assert!(orphaned(&env).is_empty());
    let value = ok(&env, &["resolve"]);
    for server in value["data"]["servers"].as_array().unwrap() {
        assert_eq!(server["status"], "resolved", "{server}");
    }

    // Re-added under the same id from another URL, they never do.
    ok(&env, &["source", "remove", "personal"]);
    let other = env.remote("mirror");
    other.write("servers/github.toml", pinned("github"));
    other.write("servers/fetch.toml", unpinned("fetch"));
    other.commit("same definitions");
    ok(
        &env,
        &["source", "add-git", &other.url(), "--id", "personal"],
    );
    assert_eq!(orphaned(&env), ["personal/fetch", "personal/github"]);
    let value = ok(&env, &["resolve"]);
    for server in value["data"]["servers"].as_array().unwrap() {
        assert_eq!(server["reason"], "pending-approval", "{server}");
    }
    // Approving under the new URL replaces the old record.
    approve(&env, "github");
    assert_eq!(orphaned(&env), ["personal/fetch"]);
}

#[test]
fn status_and_inventory_show_held_and_revoked_servers_in_text() {
    let (env, remote) = setup();
    approve(&env, "github");
    accept_and_approve_fetch(&env);
    ok(&env, &["sync", "--harness", "codex"]);
    remote.write(
        "servers/github.toml",
        pinned("github").replace("1.0.0", "2.0.0"),
    );
    remote.commit("bump");
    ok(&env, &["source", "refresh", "personal"]);
    ok(&env, &["revoke", "fetch"]);

    let (code, text, _) = env.text(&["status"]);
    assert_eq!(code, 0);
    let line = |name: &str| {
        text.lines()
            .find(|line| line.trim_start().starts_with(&format!("{name}\t")))
            .unwrap_or_else(|| panic!("no line for {name}:\n{text}"))
            .to_owned()
    };
    // The first five columns: name, desired state, record, actual entry
    // and the next sync's operation.
    let columns = |name: &str| -> Vec<String> {
        line(name)
            .trim_start()
            .split('\t')
            .take(5)
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        columns("github"),
        [
            "github",
            "desired=held (pending-approval)",
            "recorded=owned",
            "actual=present",
            "next sync: held"
        ]
    );
    assert_eq!(
        columns("fetch"),
        [
            "fetch",
            "desired=revoked",
            "recorded=owned",
            "actual=present",
            "next sync: remove"
        ]
    );

    // The inventory classifies by ownership and content: both stay managed.
    let (code, text, _) = env.text(&["inventory"]);
    assert_eq!(code, 0);
    for name in ["github", "fetch"] {
        let entry = text
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{name}\t")))
            .unwrap_or_else(|| panic!("no line for {name}:\n{text}"));
        assert!(entry.contains("\tmanaged"), "{entry}");
    }

    // The next sync keeps the held entry and removes the revoked one.
    let before = fs::read_to_string(env.codex_config()).unwrap();
    assert!(before.contains("[mcp_servers.fetch]"));
    ok(&env, &["sync", "--harness", "codex"]);
    let after = fs::read_to_string(env.codex_config()).unwrap();
    assert!(after.contains("github-mcp@1.0.0"), "{after}");
    assert!(!after.contains("[mcp_servers.fetch]"), "{after}");
}
