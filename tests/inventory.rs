//! `d-mcp inventory` on the combined `real-setup` fixture: classes,
//! `shadowed_in`, local and project scopes, duplicates and conflicts, hash
//! rules, per-scope errors, and that the command is read-only.

mod common;

use std::fs;
use std::path::Path;

use common::{CANARY, RealSetup, TestEnv, snapshot};
use serde_json::Value;

/// The fixture with the fake `claude` and the fake `codex` on `PATH`,
/// after `fetch` was synced into both harnesses and the identical Codex
/// `time` entry was taken over.
fn synced() -> (TestEnv, RealSetup) {
    let env = TestEnv::new();
    let claude = env.fake_claude_dir();
    let codex = env.fake_codex_dir();
    let env = env.with_path(&[claude, codex]);
    let setup = env.install_real_setup();
    for args in [
        &["select", "fetch"][..],
        &["select", "time", "--harness", "codex"],
    ] {
        let (code, _, stderr) = env.text(args);
        assert_eq!(code, 0, "{stderr}");
    }
    let (code, value, _) = env.json(&["sync", "--take-over", "time"]);
    assert_eq!(code, 0, "{value:#}");
    (env, setup)
}

fn inventory(env: &TestEnv, setup: &RealSetup) -> Value {
    let (code, value, stderr) = env.json_in(&setup.cwd, &["inventory"]);
    assert_eq!(code, 0, "{value:#}");
    // Neither sync nor inventory ever runs `codex`.
    assert!(!env.codex_home().join("fake-codex.log").exists());
    assert!(!stderr.contains(CANARY), "{stderr}");
    assert!(!value.to_string().contains(CANARY), "{value:#}");
    value["data"].clone()
}

/// The first scope of a harness with this kind and, if given, this path
/// (Codex project layers share the repository root; the root layer comes
/// first).
fn scope<'a>(data: &'a Value, harness: &str, scope: &str, path: Option<&Path>) -> &'a Value {
    data["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["harness"] == harness)
        .flat_map(|item| item["scopes"].as_array().unwrap())
        .find(|item| {
            item["scope"] == scope && path.is_none_or(|path| item["path"] == path.to_str().unwrap())
        })
        .unwrap_or_else(|| panic!("no {harness} {scope} {path:?} scope in {data:#}"))
}

fn entry<'a>(scope: &'a Value, name: &str) -> &'a Value {
    scope["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no entry {name} in {scope:#}"))
}

fn gap_reasons(entry: &Value) -> Vec<&str> {
    entry["import"]["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gap| gap["reason"].as_str().unwrap())
        .collect()
}

fn same_name<'a>(data: &'a Value, name: &str) -> &'a Value {
    data["same_name"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["name"] == name)
        .unwrap_or_else(|| panic!("no same-name group {name}"))
}

fn is_hex64(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Every `(harness, scope, path, name, class)` of the inventory, in order.
fn classes(data: &Value) -> Vec<(String, String, String, String, String)> {
    let text = |value: &Value| value.as_str().unwrap_or("-").to_owned();
    let mut all = Vec::new();
    for harness in data["harnesses"].as_array().unwrap() {
        for scope in harness["scopes"].as_array().unwrap() {
            for entry in scope["entries"].as_array().unwrap() {
                all.push((
                    text(&harness["harness"]),
                    text(&scope["scope"]),
                    text(&scope["path"]),
                    text(&entry["name"]),
                    text(&entry["class"]),
                ));
            }
        }
    }
    all.sort();
    all
}

#[test]
fn every_fixture_entry_is_classified() {
    let (env, setup) = synced();
    let data = inventory(&env, &setup);
    assert_eq!(data["repository"], setup.repo.to_str().unwrap());
    assert_eq!(data["display_hashes"], true);
    // The complete set: no entry is missing, none is extra.
    let (repo, cwd) = (
        setup.repo.to_str().unwrap().to_owned(),
        setup.cwd.to_str().unwrap().to_owned(),
    );
    let gone = "/nonexistent-d-mcp-fixture/project".to_owned();
    let mut expected: Vec<(String, String, String, String, String)> = [
        ("claude", "local", cwd.as_str(), "notes", "local"),
        ("claude", "local", gone.as_str(), "scratch", "local"),
        ("claude", "project", repo.as_str(), "build", "unmanaged"),
        ("claude", "project", repo.as_str(), "deploy", "unmanaged"),
        ("claude", "project", repo.as_str(), "notes", "unmanaged"),
        ("claude", "project", repo.as_str(), "review", "unmanaged"),
        ("claude", "user", "-", "api", "unmanaged"),
        ("claude", "user", "-", "docs", "unmanaged"),
        ("claude", "user", "-", "fetch", "managed"),
        ("claude", "user", "-", "github", "unmanaged"),
        ("claude", "user", "-", "linear", "unmanaged"),
        ("claude", "user", "-", "multi", "unmanaged"),
        ("claude", "user", "-", "notes", "unmanaged"),
        ("claude", "user", "-", "tasks", "unmanaged"),
        ("codex", "project", repo.as_str(), "build", "unmanaged"),
        ("codex", "project", repo.as_str(), "deploy", "unmanaged"),
        ("codex", "project", repo.as_str(), "github", "unmanaged"),
        ("codex", "user", "-", "fetch", "managed"),
        ("codex", "user", "-", "github", "unmanaged"),
        ("codex", "user", "-", "helper", "unmanaged"),
        ("codex", "user", "-", "inline", "unmanaged"),
        ("codex", "user", "-", "linear", "unmanaged"),
        ("codex", "user", "-", "search", "unmanaged"),
        ("codex", "user", "-", "tasks", "unmanaged"),
        ("codex", "user", "-", "time", "managed"),
    ]
    .iter()
    .map(|(harness, scope, path, name, class)| {
        (
            (*harness).to_owned(),
            (*scope).to_owned(),
            (*path).to_owned(),
            (*name).to_owned(),
            (*class).to_owned(),
        )
    })
    .collect();
    expected.sort();
    assert_eq!(classes(&data), expected, "{data:#}");

    let codex = scope(&data, "codex", "user", None);
    assert_eq!(codex["error"], Value::Null);
    let fetch = entry(codex, "fetch");
    assert_eq!(fetch["class"], "managed");
    // Written by d-mcp: the plain owned hash only.
    assert!(is_hex64(&fetch["hash"]));
    assert_eq!(fetch["display_hash"], Value::Null);
    let time = entry(codex, "time");
    assert_eq!(time["class"], "managed");
    // Taken over: the foreign entry's hash, shown keyed only.
    assert_eq!(time["hash"], Value::Null);
    assert!(is_hex64(&time["display_hash"]));
    let github = entry(codex, "github");
    assert_eq!(github["class"], "unmanaged");
    assert!(is_hex64(&github["display_hash"]));
    assert_eq!(
        github["shadowed_in"],
        serde_json::json!([{"scope": "project", "path": setup.repo.to_str().unwrap()}])
    );
    assert_eq!(github["import"]["ok"], true);
    assert_eq!(
        github["import"]["replaced"][0]["field"],
        "env.GITHUB_API_URL"
    );
    assert_eq!(github["view"]["transport"], "stdio");
    assert_eq!(
        github["view"]["env"],
        serde_json::json!(["GITHUB_API_URL", "GITHUB_TOKEN"])
    );
    let linear = entry(codex, "linear");
    assert_eq!(linear["class"], "unmanaged");
    assert_eq!(linear["view"]["oauth_client_id"], true);
    assert_eq!(linear["view"]["auth"], "oauth");
    let search = entry(codex, "search");
    assert_eq!(search["import"]["replaced"][0]["kind"], "header");
    assert_eq!(search["import"]["replaced"][0]["var"], "SEARCH_X_API_KEY");
    assert_eq!(
        search["view"]["url"],
        serde_json::json!({"scheme": "https", "host": "search.example.test", "port": null,
                           "hidden": ["path", "query"]})
    );
    assert_eq!(gap_reasons(entry(codex, "helper")), ["excluded-key"]);
    assert_eq!(gap_reasons(entry(codex, "inline")), ["unsupported-style"]);

    let layer = scope(&data, "codex", "project", Some(&setup.repo));
    assert_eq!(layer["trust"], "trusted");
    assert_eq!(entry(layer, "github")["class"], "unmanaged");
    let broken = data["harnesses"][0]["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scope| scope["file"] == setup.cwd.join(".codex/config.toml").to_str().unwrap())
        .unwrap();
    assert_eq!(broken["error"]["code"], "harness-config-invalid");
    assert!(broken["entries"].as_array().unwrap().is_empty());

    let claude = scope(&data, "claude", "user", None);
    assert_eq!(entry(claude, "fetch")["class"], "managed");
    assert!(is_hex64(&entry(claude, "fetch")["hash"]));
    for name in ["github", "linear", "docs", "multi", "notes"] {
        assert_eq!(entry(claude, name)["class"], "unmanaged", "{name}");
    }
    assert_eq!(gap_reasons(entry(claude, "docs")), ["sse-transport"]);
    assert_eq!(gap_reasons(entry(claude, "multi")), ["multiple-headers"]);
    assert_eq!(entry(claude, "linear")["view"]["oauth_client_id"], true);
    assert_eq!(
        entry(claude, "notes")["shadowed_in"],
        serde_json::json!([
            {"scope": "local", "path": setup.cwd.to_str().unwrap()},
            {"scope": "project", "path": setup.repo.to_str().unwrap()},
        ])
    );

    let local = scope(&data, "claude", "local", Some(&setup.cwd));
    assert_eq!(local["orphaned"], false);
    let notes = entry(local, "notes");
    assert_eq!(notes["class"], "local");
    // Names only: never hashed, viewed or imported.
    assert_eq!(notes["display_hash"], Value::Null);
    assert_eq!(notes["view"], Value::Null);
    assert_eq!(notes["import"], Value::Null);
    let gone = scope(
        &data,
        "claude",
        "local",
        Some(Path::new("/nonexistent-d-mcp-fixture/project")),
    );
    assert_eq!(gone["orphaned"], true);
    assert_eq!(entry(gone, "scratch")["class"], "local");

    let project = scope(&data, "claude", "project", None);
    assert_eq!(project["trust"], "trusted");
    assert_eq!(entry(project, "review")["approval"], "approved");
    assert_eq!(entry(project, "notes")["approval"], "pending");
    assert_eq!(
        entry(project, "review")["view"]["variables"],
        serde_json::json!(["REVIEW_TOKEN"])
    );

    assert_eq!(same_name(&data, "fetch")["verdict"], "duplicate");
    // Equal cores and equal replaced literals across the harnesses.
    assert_eq!(same_name(&data, "tasks")["verdict"], "duplicate");
    for name in ["github", "linear", "notes"] {
        assert_eq!(same_name(&data, name)["verdict"], "conflict", "{name}");
    }
    let places: Vec<String> = same_name(&data, "github")["places"]
        .as_array()
        .unwrap()
        .iter()
        .map(|place| {
            format!(
                "{}:{}",
                place["harness"].as_str().unwrap(),
                place["scope"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(places, ["codex:user", "codex:project", "claude:user"]);
}

#[test]
fn the_text_form_lists_scopes_and_redacts_content() {
    let (env, setup) = synced();
    let (code, stdout, stderr) = env.text_in(&setup.cwd, &["inventory"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains(CANARY), "{stdout}");
    assert!(!stderr.contains(CANARY), "{stderr}");
    assert!(stdout.starts_with(&format!("Repository: {}\n", setup.repo.display())));
    assert!(stdout.contains("\n  fetch\tmanaged\thash="), "{stdout}");
    assert!(stdout.contains("(trust: trusted)"), "{stdout}");
    assert!(
        stdout.contains("(orphaned: the project no longer exists)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("  error [harness-config-invalid]:"),
        "{stdout}"
    );
    assert!(
        stdout.contains("not importable: type (sse-transport)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Same name in several scopes:\n"),
        "{stdout}"
    );
    assert!(stdout.contains("  github\tconflict\tcodex:user, codex:project, claude:user"));
    assert!(stdout.contains(&format!("shadowed by local {}", setup.cwd.display())));
    let (code, only, _) = env.text_in(&setup.cwd, &["inventory", "--harness", "codex"]);
    assert_eq!(code, 0);
    assert!(!only.contains("claude "), "{only}");
}

#[test]
fn drift_vanish_and_location_changes_are_classes() {
    let (env, setup) = synced();
    // Codex: the managed entry is edited by hand.
    let config = fs::read_to_string(env.codex_config()).unwrap();
    fs::write(
        env.codex_config(),
        config.replace(
            "args = [\"mcp-server-fetch\"]",
            "args = [\"mcp-server-fetch\", \"--edited\"]",
        ),
    )
    .unwrap();
    // Claude Code: a session rewrote the file without the managed entry.
    let mut claude: Value =
        serde_json::from_str(&fs::read_to_string(env.claude_config()).unwrap()).unwrap();
    claude["mcpServers"]
        .as_object_mut()
        .unwrap()
        .remove("fetch");
    fs::write(env.claude_config(), claude.to_string()).unwrap();
    let data = inventory(&env, &setup);
    let fetch = entry(scope(&data, "codex", "user", None), "fetch");
    assert_eq!(fetch["class"], "drifted");
    // The recorded hash stays plain; the changed content is keyed.
    assert!(is_hex64(&fetch["hash"]));
    assert!(is_hex64(&fetch["display_hash"]));
    assert_ne!(fetch["hash"], fetch["display_hash"]);
    let vanished = entry(scope(&data, "claude", "user", None), "fetch");
    assert_eq!(vanished["class"], "vanished");
    assert_eq!(vanished["view"], Value::Null);

    // Another CODEX_HOME: the records point at the old file.
    let other = env.root().join("other-codex");
    fs::create_dir_all(&other).unwrap();
    let output = env
        .cmd_in(&setup.cwd)
        .env("CODEX_HOME", &other)
        .args(["--json", "inventory", "--harness", "codex"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let user = scope(&value["data"], "codex", "user", None);
    assert_eq!(entry(user, "fetch")["class"], "location-changed");
    assert_eq!(entry(user, "time")["class"], "location-changed");
}

#[test]
fn an_interrupted_write_is_pending() {
    use d_mcp::adapter::{Scope, WriteKind};
    use d_mcp::config::Harness;
    use d_mcp::state::{self, RecordKey, RecordState};
    use d_mcp::store::{StoreLock, StorePaths};
    let (env, setup) = synced();
    let paths = StorePaths::new(env.default_store());
    let lock = StoreLock::acquire(&paths).unwrap();
    let mut state = state::load(&paths).unwrap();
    let key = RecordKey {
        harness: Harness::Codex,
        scope: Scope::User,
        name: "fetch".to_owned(),
    };
    let record = state.records.get_mut(&key).unwrap();
    let previous = record.hashes().cloned();
    record.state = RecordState::Pending {
        op: WriteKind::Update,
        expected: previous.clone(),
        previous,
    };
    state::save(&paths, &lock, &state).unwrap();
    drop(lock);
    let data = inventory(&env, &setup);
    let fetch = entry(scope(&data, "codex", "user", None), "fetch");
    assert_eq!(fetch["class"], "pending");
    assert_eq!(fetch["hash"], Value::Null);
    assert_eq!(fetch["display_hash"], Value::Null);
}

#[test]
fn inventory_is_read_only_and_never_creates_the_key() {
    let (env, setup) = synced();
    fs::remove_file(env.default_store().join(".hash-key")).unwrap();
    let _ = fs::remove_file(env.root().join("fake-claude.log"));
    let before = (
        snapshot(&env.default_store()),
        snapshot(env.claude_config().parent().unwrap()),
        snapshot(&env.codex_home()),
        snapshot(&setup.repo),
    );
    let data = inventory(&env, &setup);
    let after = (
        snapshot(&env.default_store()),
        snapshot(env.claude_config().parent().unwrap()),
        snapshot(&env.codex_home()),
        snapshot(&setup.repo),
    );
    assert_eq!(before, after);
    // No harness CLI ran.
    assert!(!env.root().join("fake-claude.log").exists());
    assert!(!env.codex_home().join("fake-codex.log").exists());
    // Without a key: no display hashes, plain owned hashes stay.
    assert_eq!(data["display_hashes"], false);
    let codex = scope(&data, "codex", "user", None);
    assert_eq!(entry(codex, "github")["display_hash"], Value::Null);
    assert_eq!(entry(codex, "time")["display_hash"], Value::Null);
    assert!(is_hex64(&entry(codex, "fetch")["hash"]));
    assert!(
        data["notices"][0]
            .as_str()
            .unwrap()
            .contains("run `d-mcp init`")
    );
    // A mutating command creates it again; display hashes come back.
    let (code, _, _) = env.text(&["select", "time", "--harness", "codex"]);
    assert_eq!(code, 0);
    assert!(env.default_store().join(".hash-key").is_file());
    let data = inventory(&env, &setup);
    assert!(is_hex64(
        &entry(scope(&data, "codex", "user", None), "github")["display_hash"]
    ));
}

#[test]
fn display_hashes_are_stable_and_keyed_per_store() {
    let (env, setup) = synced();
    let first = inventory(&env, &setup);
    let again = inventory(&env, &setup);
    let hash =
        |data: &Value| entry(scope(data, "codex", "user", None), "github")["display_hash"].clone();
    assert_eq!(hash(&first), hash(&again));
    // A new key gives other display hashes for the same content.
    fs::remove_file(env.default_store().join(".hash-key")).unwrap();
    env.init();
    let rekeyed = inventory(&env, &setup);
    assert_ne!(hash(&first), hash(&rekeyed));
}

#[test]
fn an_unparsable_file_does_not_abort_the_inventory() {
    let env = TestEnv::new();
    let setup = env.install_real_setup();
    fs::write(
        env.claude_config(),
        "{\"mcpServers\": {\"a\": CANARY_BROKEN}}",
    )
    .unwrap();
    let data = inventory(&env, &setup);
    let claude = scope(&data, "claude", "user", None);
    assert_eq!(claude["error"]["code"], "harness-config-invalid");
    assert!(
        claude["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("line 1, column")
    );
    // The project scope is still read, with an unknown trust.
    let project = scope(&data, "claude", "project", None);
    assert_eq!(project["trust"], "unknown-trust");
    assert_eq!(entry(project, "review")["approval"], "unknown");
    // Codex is unaffected.
    let codex = scope(&data, "codex", "user", None);
    assert_eq!(codex["error"], Value::Null);
    assert_eq!(entry(codex, "github")["class"], "unmanaged");
}

#[test]
fn an_uninitialized_store_lists_everything_as_unmanaged() {
    let env = TestEnv::new();
    let setup = env.install_real_setup();
    fs::remove_dir_all(env.default_store()).unwrap();
    let data = inventory(&env, &setup);
    assert_eq!(data["initialized"], false);
    assert_eq!(data["display_hashes"], false);
    assert!(!env.default_store().exists());
    let codex = scope(&data, "codex", "user", None);
    for entry in codex["entries"].as_array().unwrap() {
        assert_eq!(entry["class"], "unmanaged", "{entry}");
        assert_eq!(entry["display_hash"], Value::Null);
    }
    assert!(
        data["notices"][0]
            .as_str()
            .unwrap()
            .contains("not initialized")
    );
}

#[test]
fn an_untrusted_codex_project_does_not_shadow() {
    let env = TestEnv::new();
    let setup = env.install_real_setup();
    let config = fs::read_to_string(env.codex_config()).unwrap();
    fs::write(
        env.codex_config(),
        config.replace("trust_level = \"trusted\"", "trust_level = \"untrusted\""),
    )
    .unwrap();
    let data = inventory(&env, &setup);
    let layer = scope(&data, "codex", "project", Some(&setup.repo));
    assert_eq!(layer["trust"], "untrusted");
    let github = entry(scope(&data, "codex", "user", None), "github");
    assert_eq!(github["shadowed_in"], serde_json::json!([]));
}

#[test]
fn no_read_only_command_creates_the_key() {
    let (env, setup) = synced();
    let key = env.default_store().join(".hash-key");
    fs::remove_file(&key).unwrap();
    // `adopt` is refused: the literals' variables are not set.
    for (args, expected) in [
        (&["inventory"][..], 0),
        (&["status"], 0),
        (&["doctor"], 0),
        (&["status", "--auth"], 0),
        (&["adopt", "claude:user:github"], 1),
        (&["adopt", "codex:user:github"], 1),
        (&["uninstall"], 0),
        (&["resolve"], 0),
        (&["sync", "--dry-run"], 0),
        (&["--dry-run", "unmanage", "fetch"], 0),
    ] {
        let (code, value, _) = env.json_in(&setup.cwd, args);
        assert_eq!(code, expected, "{args:?}: {value:#}");
        assert!(!key.exists(), "{args:?} created the hash key");
    }
    // Doctor warns and hints at `d-mcp init`.
    let (code, value, _) = env.json_in(&setup.cwd, &["doctor"]);
    assert_eq!(code, 0, "{value:#}");
    let checks = value["data"]["checks"].as_array().unwrap();
    let hash_key = checks.iter().find(|c| c["check"] == "hash-key").unwrap();
    assert_eq!(hash_key["severity"], "warn", "{value:#}");
    assert!(hash_key.to_string().contains("d-mcp init"), "{hash_key}");
    assert!(!key.exists());
}
