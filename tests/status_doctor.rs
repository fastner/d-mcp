//! `d-mcp status` and `d-mcp doctor`: the desired, recorded and actual
//! state, the pending operation, missing variables with the GUI caveat,
//! the login state, and every doctor check, against the fake `claude`, the
//! fake `codex` and Codex config files in an isolated environment.
//!
//! The version probe and the login-state queries only ever reach the fakes:
//! `PATH` holds nothing else (see `tests/common/mod.rs`).

mod common;

use std::fs;

use common::{CANARY, TestEnv, stdio};
use d_mcp::error::Error;
use d_mcp::store::StorePaths;
use d_mcp::sync::{SyncHooks, SyncOptions, sync};
use serde_json::{Value, json};

const GITHUB: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n\
                      [env]\nDMCP_TEST_GITHUB_TOKEN = { forward = true }\nDMCP_TEST_GITHUB_EMPTY = { forward = true }\n";
const STATIC: &str = include_str!("fixtures/definitions/valid/bearer-env.toml");
const OAUTH: &str =
    "name = \"oauth1\"\n[transport]\ntype = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n";

/// An initialized store with both fakes on `PATH` and an empty Codex file.
fn with_fakes() -> TestEnv {
    let env = TestEnv::new();
    let claude = env.fake_claude_dir();
    let codex = env.fake_codex_dir();
    let env = env.with_path(&[claude, codex]);
    env.init();
    fs::write(env.codex_config(), "").unwrap();
    env
}

/// An initialized store without any harness CLI and an empty Codex file.
fn without_fakes() -> TestEnv {
    let env = TestEnv::new();
    env.init();
    fs::write(env.codex_config(), "").unwrap();
    env
}

fn run(env: &TestEnv, vars: &[(&str, &str)], args: &[&str]) -> (i32, Value, String) {
    let mut cmd = env.cmd();
    for (name, value) in vars {
        cmd.env(name, value);
    }
    let output = cmd.arg("--json").args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout}"));
    (
        output.status.code().unwrap(),
        value,
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn sync_ok(env: &TestEnv) {
    let (code, value, stderr) = run(env, &[], &["sync"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
}

fn entry<'a>(value: &'a Value, harness: &str, name: &str) -> &'a Value {
    value["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["harness"] == harness)
        .and_then(|item| {
            item["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
        })
        .unwrap_or_else(|| panic!("no {harness} entry {name}: {value:#}"))
}

fn harness<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["harness"] == name)
        .unwrap_or_else(|| panic!("no harness {name}: {value:#}"))
}

/// The checks of a doctor document with this id.
fn checks<'a>(value: &'a Value, id: &str) -> Vec<&'a Value> {
    value["data"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["check"] == id)
        .collect()
}

/// The one check with this id (and harness, if given).
fn check<'a>(value: &'a Value, id: &str, harness: Option<&str>) -> &'a Value {
    let found: Vec<&Value> = checks(value, id)
        .into_iter()
        .filter(|check| harness.is_none_or(|harness| check["harness"] == harness))
        .collect();
    assert_eq!(found.len(), 1, "{id}: {value:#}");
    found[0]
}

// ---------------------------------------------------------------------------
// status

#[test]
fn status_shows_every_state_the_next_operation_and_missing_variables() {
    let env = without_fakes();
    env.write_local("github", GITHUB);
    env.write_local("gone", &stdio("gone", "x"));
    select(&env, &["github", "--harness", "codex"]);
    select(&env, &["gone", "--harness", "codex"]);
    sync_ok(&env);
    // `gone` stays recorded but is no longer selected; `ghost` is selected
    // and offered nowhere; `fresh` is selected and not written yet.
    select(&env, &["ghost", "--harness", "codex"]);
    env.write_local("fresh", &stdio("fresh", "y"));
    select(&env, &["fresh", "--harness", "codex"]);
    let (code, _, _) = run(&env, &[], &["unselect", "gone"]);
    assert_eq!(code, 0);

    let vars = [("DMCP_TEST_GITHUB_EMPTY", "")];
    let (code, value, stderr) = run(&env, &vars, &["status", "--harness", "codex"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(value["data"]["harnesses"].as_array().unwrap().len(), 1);
    let github = entry(&value, "codex", "github");
    assert_eq!(github["desired"], "resolved");
    assert_eq!(github["source"], "local");
    assert_eq!(github["recorded"], "owned");
    assert_eq!(github["actual"], "present");
    assert_eq!(github["operation"], "unchanged");
    assert!(github["hash"].is_string() && github["display_hash"].is_null());
    // A set but empty variable counts as missing.
    assert_eq!(
        github["missing_variables"],
        json!(["DMCP_TEST_GITHUB_EMPTY", "DMCP_TEST_GITHUB_TOKEN"])
    );
    assert_eq!(github["login"], Value::Null);
    let gone = entry(&value, "codex", "gone");
    assert_eq!(gone["desired"], "not-selected");
    assert_eq!(gone["operation"], "remove");
    let ghost = entry(&value, "codex", "ghost");
    assert_eq!(ghost["desired"], "held");
    assert_eq!(ghost["reason"], "not-offered");
    assert_eq!(ghost["recorded"], "none");
    assert_eq!(ghost["actual"], "absent");
    assert_eq!(ghost["operation"], "held");
    let fresh = entry(&value, "codex", "fresh");
    assert_eq!(fresh["operation"], "add");
    assert_eq!(fresh["actual"], "absent");
    assert_eq!(
        value["data"]["missing_variables"],
        json!(["DMCP_TEST_GITHUB_EMPTY", "DMCP_TEST_GITHUB_TOKEN"])
    );
    let notices = value["data"]["notices"].to_string();
    assert!(
        notices.contains("a harness launched from a GUI"),
        "{notices}"
    );

    // With the variables set, nothing is missing; the caveat stays.
    let vars = [
        ("DMCP_TEST_GITHUB_EMPTY", "x"),
        ("DMCP_TEST_GITHUB_TOKEN", "y"),
    ];
    let (_, value, _) = run(&env, &vars, &["status", "github"]);
    assert_eq!(value["data"]["missing_variables"], json!([]));
    assert_eq!(
        value["data"]["harnesses"][0]["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(value["data"]["notices"].to_string().contains("GUI"));
    // An unknown name is a notice; text mode prints the caveat as well.
    let (_, value, _) = run(&env, &[], &["status", "nothing"]);
    assert!(
        value["data"]["notices"]
            .to_string()
            .contains("`nothing` is neither selected nor managed")
    );
    let (code, out, _) = env.text(&["status"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(
            "github\tdesired=resolved\trecorded=owned\tactual=present\tnext sync: unchanged"
        ),
        "{out}"
    );
    assert!(
        out.contains("missing: DMCP_TEST_GITHUB_EMPTY, DMCP_TEST_GITHUB_TOKEN"),
        "{out}"
    );
    assert!(
        out.contains("note: variables are checked in this shell only"),
        "{out}"
    );
}

#[test]
fn status_reports_harness_errors_as_data_and_exits_0() {
    let env = without_fakes();
    env.write_local("github", GITHUB);
    select(&env, &["github", "--harness", "codex"]);
    fs::write(
        env.codex_config(),
        "[mcp_servers.x\ncommand = \"CANARY_BROKEN\"\n",
    )
    .unwrap();
    let (code, value, stderr) = run(&env, &[], &["status"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    let codex = harness(&value, "codex");
    assert_eq!(codex["status"], "failed");
    assert_eq!(codex["error"]["code"], "harness-config-invalid");
    assert_eq!(entry(&value, "codex", "github")["actual"], "unknown");
    assert_eq!(entry(&value, "codex", "github")["operation"], Value::Null);
    assert_eq!(harness(&value, "claude")["status"], "not-present");
    assert!(!value.to_string().contains(CANARY) && !stderr.contains(CANARY));
}

#[test]
fn status_shows_pending_writes_and_location_changes() {
    let env = without_fakes();
    env.write_local("github", GITHUB);
    select(&env, &["github", "--harness", "codex"]);
    let crash = || -> d_mcp::error::Result<()> { Err(Error::HomeUnset) };
    let options = SyncOptions {
        hooks: SyncHooks {
            before_promote: Some(&crash),
            ..SyncHooks::default()
        },
        ..SyncOptions::default()
    };
    let paths = StorePaths::new(env.default_store());
    assert!(sync(&paths, &env.environment(), &options, &mut Vec::new()).is_err());
    let (_, value, _) = run(&env, &[], &["status"]);
    let github = entry(&value, "codex", "github");
    assert_eq!(github["recorded"], "pending");
    // The read-only plan settles it in memory, as a dry-run sync does.
    assert_eq!(github["operation"], "unchanged");
    // Another CODEX_HOME: the record is for another file.
    let other = env.root().join("codex-other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("config.toml"), "").unwrap();
    let output = env
        .cmd()
        .env("CODEX_HOME", &other)
        .args(["--json", "status"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(harness(&value, "codex")["status"], "location-changed");
    assert_eq!(
        entry(&value, "codex", "github")["operation"],
        "location-changed"
    );
}

#[test]
fn status_auth_reads_the_login_state_through_the_fakes() {
    let env = with_fakes();
    env.write_local("oauth1", OAUTH);
    env.write_local("linear", &OAUTH.replace("oauth1", "linear"));
    env.write_local("tools", &stdio("tools", "x"));
    select(&env, &["oauth1", "--harness", "codex"]);
    select(&env, &["linear", "--harness", "claude"]);
    select(&env, &["tools", "--harness", "claude"]);
    sync_ok(&env);
    env.claude_mode(&["login linear connected"]);
    let (code, value, stderr) = run(&env, &[], &["status", "--auth"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(value["data"]["auth"], true);
    assert!(stderr.contains("connects to that server"), "{stderr}");
    assert!(
        value["data"]["notices"]
            .to_string()
            .contains("claude mcp get")
    );
    // The fake `codex mcp list --json` reports `oauth1` as not logged in.
    assert_eq!(entry(&value, "codex", "oauth1")["login"], "needs-login");
    assert_eq!(entry(&value, "claude", "linear")["login"], "logged-in");
    assert_eq!(entry(&value, "claude", "tools")["login"], "not-applicable");
    assert!(!value.to_string().contains(CANARY) && !stderr.contains(CANARY));
    // Without `claude` on PATH the Claude Code login state is unknown.
    let mut bare = env.cmd();
    bare.env("PATH", env.root().join("bin"));
    let output = bare.args(["--json", "status", "--auth"]).output().unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entry(&value, "claude", "linear")["login"], "unknown");
}

// ---------------------------------------------------------------------------
// doctor

#[test]
fn doctor_is_ok_with_verified_harnesses_and_a_clean_store() {
    let env = with_fakes();
    let (code, value, stderr) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(value["ok"], true);
    for id in ["store", "state", "hash-key"] {
        assert_eq!(check(&value, id, None)["severity"], "ok", "{id}");
    }
    let claude = check(&value, "harness-version", Some("claude"));
    assert_eq!(claude["severity"], "ok");
    assert!(
        claude["message"]
            .as_str()
            .unwrap()
            .contains("2.1.310 (Claude Code)")
    );
    assert_eq!(
        check(&value, "harness-version", Some("codex"))["severity"],
        "ok"
    );
    for id in [
        "harness-config",
        "drift",
        "location-changed",
        "pending",
        "recurring-vanish",
        "shadowing",
        "static-token",
        "name-length",
        "credential-names",
        "managed-policy",
    ] {
        assert_eq!(check(&value, id, None)["severity"], "ok", "{id}");
    }
    assert!(checks(&value, "login").is_empty());
    assert_eq!(value["data"]["summary"]["error"], 0);
    // The probe runs from a neutral temporary directory, without the
    // autoupdater, with the isolated HOME and CLAUDE_CONFIG_DIR.
    let probe = env
        .claude_calls()
        .into_iter()
        .find(|call| call["args"] == json!(["--version"]))
        .expect("a --version call");
    assert_eq!(probe["autoupdater"], "1");
    assert_eq!(probe["home"], env.home().to_str().unwrap());
    assert_eq!(probe["config_dir"], env.claude_dir().to_str().unwrap());
    assert_ne!(probe["cwd"], env.work().to_str().unwrap());
    let cwd = fs::read_to_string(env.codex_home().join("fake-codex-cwd.log")).unwrap();
    assert!(!cwd.contains(env.work().to_str().unwrap()), "{cwd}");
}

#[test]
fn doctor_warns_about_unverified_unrecognized_failed_and_missing_versions() {
    let env = with_fakes();
    fs::write(env.codex_home().join("fake-codex-mode"), "version-mismatch").unwrap();
    env.claude_mode(&["version 2.1.312 (Claude Code)"]);
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0, "{value:#}");
    let codex = check(&value, "harness-version", Some("codex"));
    assert_eq!(codex["severity"], "warn");
    assert!(
        codex["message"]
            .as_str()
            .unwrap()
            .contains("codex-cli 0.100.0")
    );
    assert!(
        codex["message"]
            .as_str()
            .unwrap()
            .contains("codex-cli 0.165.0")
    );
    let claude = check(&value, "harness-version", Some("claude"));
    assert_eq!(claude["severity"], "warn");
    assert!(
        claude["message"]
            .as_str()
            .unwrap()
            .contains("2.1.312 (Claude Code)")
    );

    // Output that is not a version is never shown; a failure names the
    // exit status only.
    fs::write(env.codex_home().join("fake-codex-mode"), "version-garbage").unwrap();
    env.claude_mode(&["version fail"]);
    let (code, value, stderr) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0);
    let codex = check(&value, "harness-version", Some("codex"));
    assert_eq!(codex["severity"], "warn");
    assert!(
        codex["message"]
            .as_str()
            .unwrap()
            .contains("no recognizable version")
    );
    let claude = check(&value, "harness-version", Some("claude"));
    assert!(
        claude["message"]
            .as_str()
            .unwrap()
            .contains("failed (exit status 1)")
    );
    assert!(!value.to_string().contains(CANARY) && !stderr.contains(CANARY));

    // A missing binary is a warning when the harness is configured, and ok
    // when nothing of it exists.
    let env = without_fakes();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0);
    let codex = check(&value, "harness-version", Some("codex"));
    assert_eq!(codex["severity"], "warn");
    assert!(codex["message"].as_str().unwrap().contains("not on PATH"));
    assert_eq!(
        check(&value, "harness-version", Some("claude"))["severity"],
        "ok"
    );
}

#[test]
fn doctor_reports_store_problems_as_errors_with_exit_1() {
    // Not initialized.
    let env = TestEnv::new();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "doctor-failed");
    let store = check(&value, "store", None);
    assert_eq!(store["severity"], "error");
    assert_eq!(store["code"], "store-not-initialized");
    assert_eq!(value["data"]["summary"]["error"], 1);
    let (code, out, err) = env.text(&["doctor"]);
    assert_eq!(code, 1);
    assert!(out.starts_with("error\tstore\t"), "{out}");
    assert!(err.contains("[doctor-failed]"), "{err}");

    // A corrupt config.toml and state.toml, a missing and a corrupt key.
    let env = without_fakes();
    fs::remove_file(env.default_store().join(".hash-key")).unwrap();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0);
    assert_eq!(check(&value, "hash-key", None)["severity"], "warn");
    fs::write(env.default_store().join(".hash-key"), "nonsense").unwrap();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 1);
    assert_eq!(check(&value, "hash-key", None)["code"], "hash-key-corrupt");
    fs::write(
        env.default_store().join("state.toml"),
        "schema_version = 1\nx = 1\n",
    )
    .unwrap();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 1);
    assert_eq!(check(&value, "state", None)["code"], "state-corrupt");
    // The checks that need the store are skipped.
    assert!(checks(&value, "drift").is_empty());
    fs::write(
        env.default_store().join("config.toml"),
        "schema_version = 7\n",
    )
    .unwrap();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 1);
    assert_eq!(check(&value, "store", None)["code"], "schema-unsupported");
}

#[test]
fn doctor_finds_drift_location_changes_pending_writes_and_broken_files() {
    let env = without_fakes();
    env.write_local("github", GITHUB);
    env.write_local("wiki", &stdio("wiki", "w"));
    select(&env, &["github", "--harness", "codex"]);
    select(&env, &["wiki", "--harness", "codex"]);
    sync_ok(&env);
    let text = fs::read_to_string(env.codex_config()).unwrap();
    fs::write(
        env.codex_config(),
        text.replace("command = \"w\"", "command = \"edited\""),
    )
    .unwrap();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0, "{value:#}");
    let drift = check(&value, "drift", None);
    assert_eq!(drift["severity"], "warn");
    assert_eq!(drift["server"], "wiki");
    assert!(drift["hint"].as_str().unwrap().contains("--take-over wiki"));

    // A location change.
    let other = env.root().join("codex-other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("config.toml"), "").unwrap();
    let output = env
        .cmd()
        .env("CODEX_HOME", &other)
        .args(["--json", "doctor"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let moved = checks(&value, "location-changed");
    assert_eq!(moved.len(), 2, "{value:#}");
    assert!(moved.iter().all(|check| check["severity"] == "warn"));

    // A pending write.
    let env = without_fakes();
    env.write_local("github", GITHUB);
    select(&env, &["github", "--harness", "codex"]);
    let crash = || -> d_mcp::error::Result<()> { Err(Error::HomeUnset) };
    let options = SyncOptions {
        hooks: SyncHooks {
            before_promote: Some(&crash),
            ..SyncHooks::default()
        },
        ..SyncOptions::default()
    };
    let paths = StorePaths::new(env.default_store());
    assert!(sync(&paths, &env.environment(), &options, &mut Vec::new()).is_err());
    let (_, value, _) = run(&env, &[], &["doctor"]);
    let pending = check(&value, "pending", None);
    assert_eq!(pending["severity"], "warn");
    assert_eq!(pending["server"], "github");

    // A user-scope file that cannot be read is an error.
    fs::write(env.codex_config(), "[mcp_servers\n").unwrap();
    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 1, "{value:#}");
    let broken = check(&value, "harness-config", Some("codex"));
    assert_eq!(broken["severity"], "error");
    assert_eq!(broken["code"], "harness-config-invalid");
}

#[test]
fn doctor_warns_about_recurring_vanishes_from_the_second_one() {
    let env = without_fakes();
    env.write_local("github", GITHUB);
    select(&env, &["github", "--harness", "codex"]);
    sync_ok(&env);
    for round in 1..=2 {
        fs::write(env.codex_config(), "").unwrap();
        sync_ok(&env);
        let (_, value, _) = run(&env, &[], &["doctor"]);
        let vanish = check(&value, "recurring-vanish", None);
        if round == 1 {
            assert_eq!(vanish["severity"], "ok", "{value:#}");
        } else {
            assert_eq!(vanish["severity"], "warn", "{value:#}");
            assert_eq!(vanish["server"], "github");
            assert!(vanish["message"].as_str().unwrap().contains("2 times"));
        }
    }
    let (_, value, _) = run(&env, &[], &["status"]);
    assert_eq!(entry(&value, "codex", "github")["vanish_count"], 2);
}

#[test]
fn doctor_warns_about_shadowing_static_tokens_and_long_names() {
    let env = with_fakes();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    env.write_local("tools", &stdio("tools", "x"));
    env.write_local("bearer-env", STATIC);
    select(&env, &["tools", "--harness", "claude"]);
    select(&env, &["bearer-env", "--harness", "codex"]);
    sync_ok(&env);
    // A local-scope entry of the same name shadows the managed one, and a
    // Codex entry has a name over the limit.
    let mut config: Value =
        serde_json::from_str(&fs::read_to_string(env.claude_config()).unwrap()).unwrap();
    config["projects"] =
        json!({ "/some/project": { "mcpServers": { "tools": { "command": "y" } } } });
    fs::write(env.claude_config(), config.to_string()).unwrap();
    let long = "a".repeat(40);
    let codex = fs::read_to_string(env.codex_config()).unwrap();
    fs::write(
        env.codex_config(),
        format!("{codex}\n[mcp_servers.{long}]\ncommand = \"z\"\n"),
    )
    .unwrap();
    // An entry adopted from the project scope is shadowed by its origin.
    fs::write(
        env.work().join(".mcp.json"),
        r#"{"mcpServers": {"review": {"type": "http", "url": "https://review.example.test/mcp",
            "headers": {"Authorization": "Bearer review-literal-4711"}}}}"#,
    )
    .unwrap();
    let vars = [("REVIEW_TOKEN", "review-literal-4711")];
    let (code, preview, _) = run(
        &env,
        &vars,
        &["adopt", "claude:project:review", "--to-user-scope"],
    );
    assert_eq!(code, 0, "{preview:#}");
    let hash = preview["data"]["servers"][0]["entries"][0]["display_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let expect = format!("--expect=claude:project:review={hash}");
    let (code, value, _) = run(
        &env,
        &vars,
        &[
            "adopt",
            "claude:project:review",
            "--to-user-scope",
            "--apply",
            &expect,
        ],
    );
    assert_eq!(code, 0, "{value:#}");

    let (code, value, _) = run(&env, &[], &["doctor"]);
    assert_eq!(code, 0, "{value:#}");
    let shadowing = checks(&value, "shadowing");
    assert_eq!(shadowing.len(), 2, "{value:#}");
    let local = shadowing
        .iter()
        .find(|check| check["server"] == "tools")
        .unwrap();
    assert!(
        local["message"]
            .as_str()
            .unwrap()
            .contains("local-scope entry in /some/project")
    );
    let adopted = shadowing
        .iter()
        .find(|check| check["server"] == "review")
        .unwrap();
    assert!(
        adopted["message"].as_str().unwrap().contains(
            "the project-scope entry it was adopted from, which still holds its literals"
        ),
        "{adopted}"
    );
    let token = check(&value, "static-token", None);
    assert_eq!(token["severity"], "warn");
    assert_eq!(token["server"], "bearer-env");
    assert!(token["message"].as_str().unwrap().contains("EXAMPLE_TOKEN"));
    let length = check(&value, "name-length", None);
    assert_eq!(length["severity"], "warn");
    assert_eq!(length["server"], long.as_str());
    assert!(!value.to_string().contains("review-literal-4711"));
}

#[test]
fn doctor_auth_reports_the_login_state_of_managed_entries() {
    let env = with_fakes();
    env.write_local("linear", &OAUTH.replace("oauth1", "linear"));
    env.write_local("tools", &stdio("tools", "x"));
    select(&env, &["linear", "--harness", "claude"]);
    select(&env, &["tools"]);
    sync_ok(&env);
    env.claude_mode(&["login linear needs-authentication"]);
    let (code, value, stderr) = run(&env, &[], &["doctor", "--auth"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(value["data"]["auth"], true);
    assert!(stderr.contains("connects to that server"), "{stderr}");
    let login = checks(&value, "login");
    let linear = login
        .iter()
        .find(|check| check["server"] == "linear")
        .unwrap();
    assert_eq!(linear["severity"], "warn");
    assert_eq!(linear["hint"], "run `claude mcp login linear`");
    let tools: Vec<&&Value> = login
        .iter()
        .filter(|check| check["server"] == "tools")
        .collect();
    assert_eq!(tools.len(), 2);
    // The fake `codex` does not list `tools`, so its state is unknown.
    assert!(
        tools
            .iter()
            .any(|check| check["harness"] == "claude" && check["severity"] == "ok")
    );
    assert!(
        tools
            .iter()
            .any(|check| check["harness"] == "codex" && check["severity"] == "warn")
    );
    assert!(!value.to_string().contains(CANARY) && !stderr.contains(CANARY));
    // Without managed entries there is one `ok`.
    let env = with_fakes();
    let (_, value, _) = run(&env, &[], &["doctor", "--auth"]);
    assert_eq!(check(&value, "login", None)["severity"], "ok");
}

fn select(env: &TestEnv, args: &[&str]) {
    let mut all = vec!["select"];
    all.extend_from_slice(args);
    let (code, value, stderr) = run(env, &[], &all);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
}
