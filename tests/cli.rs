//! Command-level tests of the compiled binary in an isolated environment.

mod common;

use std::fs;
use std::path::Path;

use common::{TestEnv, snapshot, stdio};
use predicates::prelude::*;
use serde_json::Value;

/// A token that must never appear in any output.
const CANARY: &str = "ghp_CANARYtoken0123456789";

fn codes(value: &Value) -> Vec<String> {
    value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap().to_owned())
        .collect()
}

fn server<'v>(value: &'v Value, name: &str) -> &'v Value {
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == name)
        .unwrap_or_else(|| panic!("no server {name} in {value}"))
}

// --- Store location -------------------------------------------------------

#[test]
fn store_flag_beats_env_which_beats_home() {
    let env = TestEnv::new();
    let flag = env.root().join("flag-store");
    let from_env = env.root().join("env-store");
    let (code, value, _) = {
        let output = env
            .cmd()
            .env("D_MCP_STORE", &from_env)
            .args(["--json", "init", "--store"])
            .arg(&flag)
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            (),
        )
    };
    assert_eq!(code, 0);
    assert_eq!(value["data"]["store"], flag.to_str().unwrap());
    assert!(!from_env.exists());

    let output = env
        .cmd()
        .env("D_MCP_STORE", &from_env)
        .args(["--json", "init"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["store"], from_env.to_str().unwrap());
    assert!(!env.default_store().exists());

    let (code, value, _) = env.json(&["init"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["data"]["store"],
        env.default_store().to_str().unwrap()
    );
}

#[test]
fn empty_d_mcp_store_counts_as_unset() {
    let env = TestEnv::new();
    let output = env
        .cmd()
        .env("D_MCP_STORE", "")
        .args(["--json", "init"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["data"]["store"],
        env.default_store().to_str().unwrap()
    );
}

#[test]
fn relative_and_tilde_store_paths_are_resolved() {
    let env = TestEnv::new();
    let (code, value, _) = env.json(&["init", "--store", "rel/store"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["data"]["store"],
        env.work().join("rel/store").to_str().unwrap()
    );
    let output = env
        .cmd()
        .env("D_MCP_STORE", "~/tilde-store")
        .args(["--json", "init"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["data"]["store"],
        env.home().join("tilde-store").to_str().unwrap()
    );
}

#[test]
fn unset_or_empty_home_is_an_environment_error() {
    let env = TestEnv::new();
    env.cmd()
        .env_remove("HOME")
        .arg("init")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("[home-unset]"));
    let output = env
        .cmd()
        .env("HOME", "")
        .args(["--json", "resolve"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "home-unset");
    // An explicit store does not need HOME.
    env.cmd()
        .env_remove("HOME")
        .arg("init")
        .arg("--store")
        .arg(env.root().join("explicit"))
        .assert()
        .success();
}

// --- init -----------------------------------------------------------------

#[test]
fn init_is_idempotent_and_sets_permissions() {
    let env = TestEnv::new();
    let (code, value, _) = env.json(&["init"]);
    assert_eq!(code, 0);
    let store = env.default_store();
    let created: Vec<&str> = value["data"]["created"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    let expected: Vec<String> = ["", "/.lock", "/config.toml", "/local", "/local/servers"]
        .iter()
        .map(|suffix| format!("{}{suffix}", store.display()))
        .collect();
    assert_eq!(created, expected);
    assert_eq!(
        fs::read_to_string(store.join("config.toml")).unwrap(),
        "schema_version = 1\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        for dir in [
            store.clone(),
            store.join("local"),
            store.join("local/servers"),
        ] {
            assert_eq!(mode(&dir), 0o700, "{}", dir.display());
        }
        for file in [store.join("config.toml"), store.join(".lock")] {
            assert_eq!(mode(&file), 0o600, "{}", file.display());
        }
    }
    let before = snapshot(&store);
    let (code, value, _) = env.json(&["init"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["created"], serde_json::json!([]));
    assert_eq!(snapshot(&store), before);
    env.cmd()
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("already initialized"));
}

#[test]
fn init_completes_a_partial_store() {
    let env = TestEnv::new();
    let store = env.default_store();
    fs::create_dir_all(store.join("local")).unwrap();
    let (code, value, _) = env.json(&["init"]);
    assert_eq!(code, 0);
    let created = value["data"]["created"].as_array().unwrap();
    assert_eq!(created.len(), 3, "{value}");
    assert!(store.join("local/servers").is_dir());
    assert!(store.join("config.toml").is_file());
}

#[cfg(unix)]
#[test]
fn init_follows_a_symlinked_store_and_warns_about_loose_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    // A dotfile-managed store: `~/.d-mcp` links to a directory elsewhere.
    let real = env.root().join("dotfiles/d-mcp");
    fs::create_dir_all(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink(&real, env.default_store()).unwrap();
    let (code, value, stderr) = env.json(&["init"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["created"].as_array().unwrap().len(), 4);
    assert!(
        stderr.contains("warning: the store directory") && stderr.contains("mode 0750"),
        "{stderr}"
    );
    assert!(real.join("config.toml").is_file());
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o750
    );
    // Every other command uses the same store through the link.
    let (code, value, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["sources"][0]["id"], "local");
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let (code, _, stderr) = env.json(&["init"]);
    assert_eq!(code, 0);
    assert!(stderr.is_empty(), "{stderr}");
}

#[test]
fn text_output_escapes_control_characters_in_file_names() {
    let env = TestEnv::new();
    env.init();
    let servers = env.default_store().join("local/servers");
    fs::write(servers.join("bad\u{1b}[2J.toml"), stdio("bad", "x")).unwrap();
    let output = env.cmd().arg("resolve").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains('\u{1b}'), "{stdout:?}");
    assert!(stdout.contains("servers/bad\\u{1b}[2J.toml"), "{stdout}");
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    assert_eq!(value["problems"][0]["file"], "servers/bad\u{1b}[2J.toml");
}

#[test]
fn init_refuses_a_foreign_directory() {
    let env = TestEnv::new();
    let foreign = env.root().join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("README"), "mine").unwrap();
    let (code, value, _) = env.json(&["init", "--store", foreign.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "foreign-directory");
    assert_eq!(value["ok"], false);
    assert_eq!(value["data"], Value::Null);
    let names: Vec<_> = fs::read_dir(&foreign).unwrap().collect();
    assert_eq!(names.len(), 1);

    let file = env.root().join("file");
    fs::write(&file, "x").unwrap();
    let (code, value, _) = env.json(&["init", "--store", file.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "store-path-invalid");
}

#[test]
fn uninitialized_store_fails_with_exit_1() {
    let env = TestEnv::new();
    fs::create_dir_all(env.default_store()).unwrap();
    for args in [
        vec!["resolve"],
        vec!["source", "list"],
        vec!["select", "github"],
        vec!["unselect", "github"],
        vec!["source", "remove", "team"],
        vec!["source", "add", ".", "--id", "team", "--priority", "1"],
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 1, "{args:?}");
        assert_eq!(value["error"]["code"], "store-not-initialized", "{args:?}");
    }
    // Mutating commands fail before locking: no `.lock` was created.
    assert!(!env.default_store().join(".lock").exists());
}

#[test]
fn corrupt_config_is_unsafe_and_never_rewritten() {
    let env = TestEnv::new();
    env.init();
    let config = env.default_store().join("config.toml");
    for (text, code) in [
        (
            format!("schema_version = 1\n[[sources]]\nid = \"{CANARY}\"\n"),
            "config-corrupt",
        ),
        ("schema_version = 2\n".to_owned(), "schema-unsupported"),
        (
            format!("schema_version = 1\ntoken = \"{CANARY}\n"),
            "config-corrupt",
        ),
    ] {
        fs::write(&config, &text).unwrap();
        for args in [vec!["resolve"], vec!["select", "github"], vec!["init"]] {
            let output = env.cmd().arg("--json").args(&args).output().unwrap();
            assert_eq!(output.status.code(), Some(3), "{args:?} {text}");
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(!stdout.contains(CANARY) && !stderr.contains(CANARY));
            let value: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(value["error"]["code"], code);
            assert_eq!(fs::read_to_string(&config).unwrap(), text);
        }
    }
}

// --- Locking --------------------------------------------------------------

#[test]
fn held_lock_blocks_mutations_but_not_reads() {
    let env = TestEnv::new();
    env.init();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(env.default_store().join(".lock"))
        .unwrap();
    lock.lock().unwrap();
    let (code, value, _) = env.json(&["select", "github"]);
    assert_eq!(code, 3);
    assert_eq!(value["error"]["code"], "store-locked");
    let (code, _, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    let (code, _, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);
    let (code, _, _) = env.json(&["--dry-run", "select", "github"]);
    assert_eq!(code, 0);
    lock.unlock().unwrap();
    let (code, _, _) = env.json(&["select", "github"]);
    assert_eq!(code, 0);
}

// --- Sources --------------------------------------------------------------

#[test]
fn local_source_is_implicit_with_priority_zero() {
    let env = TestEnv::new();
    env.init();
    let (code, value, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);
    let sources = value["data"]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["id"], "local");
    assert_eq!(sources[0]["priority"], 0);
    assert_eq!(sources[0]["builtin"], true);
    assert_eq!(
        sources[0]["path"],
        env.default_store().join("local").to_str().unwrap()
    );
    let (code, value, _) = env.json(&["source", "remove", "local"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "source-id-reserved");
    assert!(
        !fs::read_to_string(env.default_store().join("config.toml"))
            .unwrap()
            .contains("local")
    );
}

#[test]
fn source_add_rejections() {
    let env = TestEnv::new();
    env.init();
    let team = env.source_dir("team");
    let team = team.to_str().unwrap();
    let file = env.root().join("plain-file");
    fs::write(&file, "x").unwrap();
    let in_store = env.default_store().join("local");
    let ok = env.json(&["source", "add", team, "--id", "team", "--priority", "5"]);
    assert_eq!(ok.0, 0);
    let other = env.source_dir("other");
    let other = other.to_str().unwrap();
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![other, "--id", "local", "--priority", "1"],
            "source-id-reserved",
        ),
        (
            vec![other, "--id", "Bad__Id", "--priority", "1"],
            "invalid-name",
        ),
        (
            vec![other, "--id", "other", "--priority", "0"],
            "priority-invalid",
        ),
        (
            vec!["/does/not/exist", "--id", "other", "--priority", "1"],
            "source-path-not-found",
        ),
        (
            vec![file.to_str().unwrap(), "--id", "other", "--priority", "1"],
            "source-path-not-directory",
        ),
        (
            vec![
                in_store.to_str().unwrap(),
                "--id",
                "other",
                "--priority",
                "1",
            ],
            "source-path-in-store",
        ),
        (
            vec![other, "--id", "team", "--priority", "1"],
            "source-id-taken",
        ),
        (
            vec![other, "--id", "other", "--priority", "5"],
            "priority-taken",
        ),
        (
            vec![team, "--id", "other", "--priority", "1"],
            "source-path-registered",
        ),
    ];
    for (args, code) in cases {
        let mut full = vec!["source", "add"];
        full.extend(args);
        let (exit, value, _) = env.json(&full);
        assert_eq!(exit, 1, "{full:?}");
        assert_eq!(value["error"]["code"], code, "{full:?}");
    }
    // Usage errors from clap: exit 2, no JSON on stdout.
    for priority in ["-1", "high"] {
        env.cmd()
            .args([
                "--json",
                "source",
                "add",
                other,
                "--id",
                "other",
                "--priority",
                priority,
            ])
            .assert()
            .code(2)
            .stdout(predicate::str::is_empty());
    }
    env.cmd()
        .args(["--json", "source", "add", other, "--id", "other"])
        .assert()
        .code(2);
}

#[test]
fn relative_source_paths_are_stored_canonically() {
    let env = TestEnv::new();
    env.init();
    let team = env.source_dir("team");
    fs::create_dir_all(env.work().join("nested")).unwrap();
    let (code, value, stderr) = env.json(&[
        "source",
        "add",
        "nested/../../team/./",
        "--id",
        "team",
        "--priority",
        "3",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let canonical = fs::canonicalize(&team).unwrap();
    assert_eq!(value["data"]["source"]["path"], canonical.to_str().unwrap());
    let config = fs::read_to_string(env.default_store().join("config.toml")).unwrap();
    assert!(config.contains(&format!("path = \"{}\"", canonical.display())));
    #[cfg(unix)]
    {
        let link = env.root().join("team-link");
        std::os::unix::fs::symlink(&team, &link).unwrap();
        let (code, value, _) = env.json(&[
            "source",
            "add",
            link.to_str().unwrap(),
            "--id",
            "alias",
            "--priority",
            "4",
        ]);
        assert_eq!(code, 1);
        assert_eq!(value["error"]["code"], "source-path-registered");
    }
}

#[test]
fn source_without_servers_dir_is_added_with_a_warning_and_unavailable() {
    let env = TestEnv::new();
    env.init();
    let bare = env.root().join("bare");
    fs::create_dir_all(&bare).unwrap();
    let (code, value, stderr) = env.json(&[
        "source",
        "add",
        bare.to_str().unwrap(),
        "--id",
        "bare",
        "--priority",
        "2",
    ]);
    assert_eq!(code, 0);
    assert!(stderr.contains("unavailable"));
    assert_eq!(value["data"]["source"]["available"], false);
}

#[test]
fn source_remove_reports_selections_no_longer_offered() {
    let env = TestEnv::new();
    env.init();
    let team = env.source_dir("team");
    fs::write(team.join("servers/github.toml"), stdio("github", "npx")).unwrap();
    fs::write(team.join("servers/shared.toml"), stdio("shared", "npx")).unwrap();
    env.write_local("shared", &stdio("shared", "local"));
    env.json(&[
        "source",
        "add",
        team.to_str().unwrap(),
        "--id",
        "team",
        "--priority",
        "5",
    ]);
    env.json(&["select", "github"]);
    env.json(&["select", "shared"]);
    let config = env.default_store().join("config.toml");
    let before = fs::read(&config).unwrap();
    let (code, value, stderr) = env.json(&["--dry-run", "source", "remove", "team"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["data"]["no_longer_offered"],
        serde_json::json!(["github"])
    );
    assert_eq!(value["data"]["dry_run"], true);
    assert!(stderr.contains("`github` is no longer offered"));
    assert_eq!(fs::read(&config).unwrap(), before);
    let (code, value, _) = env.json(&["source", "remove", "team"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["data"]["no_longer_offered"],
        serde_json::json!(["github"])
    );
    assert_eq!(value["data"]["source"]["id"], "team");
    let (_, value, _) = env.json(&["resolve"]);
    assert_eq!(server(&value, "github")["status"], "not-offered");
    assert_eq!(server(&value, "shared")["source"], "local");
    let (code, value, _) = env.json(&["source", "remove", "team"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "source-not-found");
}

#[test]
fn ignored_entries_are_reported_as_notices() {
    let env = TestEnv::new();
    env.init();
    let servers = env.default_store().join("local/servers");
    fs::write(servers.join(".hidden.toml"), stdio("hidden", "x")).unwrap();
    fs::write(servers.join("README.md"), "docs").unwrap();
    fs::create_dir(servers.join("drafts")).unwrap();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    let rules: Vec<(&str, &str)> = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            assert_eq!(p["code"], "notice");
            assert_eq!(p["source"], "local");
            (p["file"].as_str().unwrap(), p["rule"].as_str().unwrap())
        })
        .collect();
    assert_eq!(
        rules,
        [
            ("servers/.hidden.toml", "hidden-file"),
            ("servers/README.md", "non-toml-file"),
            ("servers/drafts", "subdirectory"),
        ]
    );
}

// --- Selections -----------------------------------------------------------

#[test]
fn select_defaults_to_all_replaces_and_collapses_duplicates() {
    let env = TestEnv::new();
    env.init();
    env.write_local("github", &stdio("github", "npx"));
    let (code, value, stderr) = env.json(&["select", "github"]);
    assert_eq!(code, 0);
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(value["data"]["harnesses"], serde_json::json!(["all"]));
    assert_eq!(value["data"]["offered"], true);
    assert_eq!(value["data"]["replaced"], false);
    let config = env.default_store().join("config.toml");
    assert!(
        fs::read_to_string(&config)
            .unwrap()
            .contains("server = \"github\"\nharnesses = [\"all\"]")
    );
    let (_, value, _) = env.json(&[
        "select",
        "github",
        "--harness",
        "codex",
        "--harness",
        "claude",
        "--harness",
        "codex",
    ]);
    assert_eq!(
        value["data"]["harnesses"],
        serde_json::json!(["claude", "codex"])
    );
    assert_eq!(value["data"]["replaced"], true);
    let text = fs::read_to_string(&config).unwrap();
    assert_eq!(text.matches("[[selections]]").count(), 1);
    assert!(text.contains("harnesses = [\"claude\", \"codex\"]"));
    let (_, value, _) = env.json(&["select", "github", "--harness", "claude"]);
    assert_eq!(value["data"]["harnesses"], serde_json::json!(["claude"]));
    let (_, value, _) = env.json(&["resolve"]);
    assert_eq!(
        server(&value, "github")["harnesses"],
        serde_json::json!(["claude"])
    );
    env.cmd()
        .args(["select", "github", "--harness", "gemini"])
        .assert()
        .code(2);
}

#[test]
fn selecting_an_unoffered_name_warns() {
    let env = TestEnv::new();
    env.init();
    let (code, value, stderr) = env.json(&["select", "ghost"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["offered"], false);
    assert!(stderr.starts_with("warning: no source offers `ghost`"));
    let (code, value, _) = env.json(&["select", "Not-Valid"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "invalid-name");
}

#[test]
fn unselect_removes_the_selection() {
    let env = TestEnv::new();
    env.init();
    env.json(&["select", "github"]);
    let (code, value, _) = env.json(&["unselect", "github"]);
    assert_eq!(code, 0);
    assert_eq!(
        value["data"],
        serde_json::json!({"server": "github", "dry_run": false})
    );
    assert_eq!(
        fs::read_to_string(env.default_store().join("config.toml")).unwrap(),
        "schema_version = 1\n"
    );
    let (code, value, _) = env.json(&["unselect", "github"]);
    assert_eq!(code, 1);
    assert_eq!(value["error"]["code"], "not-selected");
}

// --- Dry runs -------------------------------------------------------------

#[test]
fn dry_run_on_read_only_commands_warns_and_keeps_json() {
    let env = TestEnv::new();
    env.init();
    for args in [
        vec!["--dry-run", "resolve"],
        vec!["--dry-run", "source", "list"],
    ] {
        let (code, value, stderr) = env.json(&args);
        assert_eq!(code, 0);
        assert_eq!(value["ok"], true);
        assert!(stderr.contains("warning: --dry-run has no effect on read-only commands"));
    }
}

#[test]
fn dry_run_on_every_mutating_command_changes_nothing() {
    let env = TestEnv::new();
    env.init();
    env.write_local("github", &stdio("github", "npx"));
    let team = env.source_dir("team");
    let team_arg = team.to_str().unwrap().to_owned();
    env.json(&[
        "source",
        "add",
        &team_arg,
        "--id",
        "team",
        "--priority",
        "5",
    ]);
    env.json(&["select", "github"]);
    let other = env.source_dir("other");
    let store = env.default_store();
    let before = snapshot(&store);
    let commands: Vec<Vec<&str>> = vec![
        vec!["init"],
        vec![
            "source",
            "add",
            other.to_str().unwrap(),
            "--id",
            "other",
            "--priority",
            "7",
        ],
        vec!["source", "remove", "team"],
        vec!["select", "linear", "--harness", "codex"],
        vec!["select", "github", "--harness", "claude"],
        vec!["unselect", "github"],
    ];
    for args in &commands {
        let mut full = vec!["--dry-run"];
        full.extend(args.iter().copied());
        let (code, value, _) = env.json(&full);
        assert_eq!(code, 0, "{full:?}: {value}");
        assert_eq!(value["data"]["dry_run"], true, "{full:?}");
        assert_eq!(snapshot(&store), before, "{full:?} changed the store");
    }
}

#[test]
fn dry_run_creates_nothing_on_a_missing_store() {
    let env = TestEnv::new();
    let store = env.default_store();
    let (code, value, _) = env.json(&["--dry-run", "init"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["created"].as_array().unwrap().len(), 5);
    assert!(fs::symlink_metadata(&store).is_err());
    for args in [
        vec!["--dry-run", "select", "github"],
        vec!["--dry-run", "unselect", "github"],
        vec!["--dry-run", "source", "remove", "team"],
        vec![
            "--dry-run",
            "source",
            "add",
            ".",
            "--id",
            "team",
            "--priority",
            "1",
        ],
    ] {
        let (code, _, _) = env.json(&args);
        assert_eq!(code, 1, "{args:?}");
        assert!(fs::symlink_metadata(&store).is_err(), "{args:?}");
    }
    env.cmd()
        .args(["--dry-run", "init"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would create"));
    assert!(fs::symlink_metadata(&store).is_err());
}

// --- Resolution through the CLI -------------------------------------------

#[test]
fn resolve_covers_every_status() {
    let env = TestEnv::new();
    env.init();
    let team = env.source_dir("team");
    env.write_local("github", &stdio("github", "low"));
    fs::write(team.join("servers/github.toml"), stdio("github", "high")).unwrap();
    env.write_local("broken", &stdio("broken", "ok"));
    fs::write(
        team.join("servers/broken.toml"),
        "name = \"broken\"\n[transport\n",
    )
    .unwrap();
    env.json(&[
        "source",
        "add",
        team.to_str().unwrap(),
        "--id",
        "team",
        "--priority",
        "5",
    ]);
    for name in ["github", "broken", "ghost"] {
        env.json(&["select", name]);
    }
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    let github = server(&value, "github");
    assert_eq!(github["status"], "resolved");
    assert_eq!(github["source"], "team");
    assert_eq!(github["transport"], "stdio");
    assert_eq!(github["definition_hash"].as_str().unwrap().len(), 64);
    let broken = server(&value, "broken");
    assert_eq!(broken["status"], "hold");
    assert_eq!(broken["reason"], "invalid-winner");
    assert_eq!(broken["source"], "team");
    assert_eq!(server(&value, "ghost")["status"], "not-offered");
    let codes = codes(&value);
    for code in [
        "invalid-definition",
        "invalid-winner",
        "not-offered",
        "shadowed",
    ] {
        assert!(
            codes.contains(&code.to_owned()),
            "{code} missing in {codes:?}"
        );
    }

    // The higher-priority source becomes unavailable: affected servers are held.
    fs::remove_dir_all(team.join("servers")).unwrap();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    for name in ["github", "broken", "ghost"] {
        let held = server(&value, name);
        assert_eq!(held["status"], "hold", "{name}");
        assert_eq!(held["reason"], "source-unavailable", "{name}");
        assert_eq!(held["source"], "team");
    }
    let (_, value, _) = env.json(&["source", "list"]);
    assert_eq!(value["data"]["sources"][0]["available"], false);
    env.cmd()
        .arg("resolve")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "github\thold (source-unavailable)\tsource=team",
        ));
}

// --- Canaries -------------------------------------------------------------

#[test]
fn canary_tokens_never_reach_any_output() {
    let env = TestEnv::new();
    env.init();
    let cases = [
        (
            "malformed",
            format!("name = \"malformed\"\ntoken = \"{CANARY}\n"),
        ),
        (
            "literal",
            format!(
                "name = \"literal\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
                 [env]\nGH = {{ value = \"{CANARY}\" }}\n"
            ),
        ),
        (
            "argument",
            format!(
                "name = \"argument\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
                 args = [\"--github={CANARY}\"]\n"
            ),
        ),
        (
            "key",
            format!(
                "name = \"key\"\n{CANARY} = 1\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n"
            ),
        ),
    ];
    for (name, text) in &cases {
        env.write_local(name, text);
        env.json(&["select", name]);
    }
    for args in [
        vec!["resolve"],
        vec!["--json", "resolve"],
        vec!["source", "list"],
    ] {
        let output = env.cmd().args(&args).output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stdout.contains(CANARY), "{args:?} stdout: {stdout}");
        assert!(!stderr.contains(CANARY), "{args:?} stderr: {stderr}");
        assert!(!stdout.contains("CANARY"));
    }
    let (_, value, _) = env.json(&["resolve"]);
    for (name, _) in &cases {
        assert_eq!(server(&value, name)["reason"], "invalid-winner", "{name}");
    }
    let rules: Vec<&str> = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["code"] == "invalid-definition")
        .map(|p| p["rule"].as_str().unwrap())
        .collect();
    assert_eq!(
        rules,
        [
            "secret-prefix",
            "unknown-key",
            "secret-prefix",
            "toml-syntax"
        ]
    );
}

#[test]
fn json_flag_alone_is_a_usage_error() {
    let env = TestEnv::new();
    env.cmd()
        .arg("--json")
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty());
}

// --- Acceptance-criteria gap tests ------------------------------------------

#[test]
fn held_lock_blocks_every_mutating_command_and_changes_nothing() {
    let env = TestEnv::new();
    env.init();
    let team = env.source_dir("team");
    env.json(&[
        "source",
        "add",
        team.to_str().unwrap(),
        "--id",
        "team",
        "--priority",
        "3",
    ]);
    env.json(&["select", "github"]);
    let other = env.source_dir("other");
    let store = env.default_store();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(store.join(".lock"))
        .unwrap();
    lock.lock().unwrap();
    let before = snapshot(&store);
    let commands: Vec<Vec<&str>> = vec![
        vec![
            "source",
            "add",
            other.to_str().unwrap(),
            "--id",
            "other",
            "--priority",
            "9",
        ],
        vec!["source", "remove", "team"],
        vec!["select", "linear"],
        vec!["unselect", "github"],
    ];
    std::thread::scope(|scope| {
        for args in &commands {
            let env = &env;
            scope.spawn(move || {
                let (code, value, _) = env.json(args);
                assert_eq!(code, 3, "{args:?}");
                assert_eq!(value["error"]["code"], "store-locked", "{args:?}");
            });
        }
    });
    assert_eq!(snapshot(&store), before);
    // Dry runs and read-only commands take no lock.
    for args in [
        vec!["--dry-run", "unselect", "github"],
        vec!["--dry-run", "source", "remove", "team"],
        vec!["--dry-run", "init"],
        vec!["source", "list"],
        vec!["resolve"],
    ] {
        let (code, _, _) = env.json(&args);
        assert_eq!(code, 0, "{args:?}");
    }
    assert_eq!(snapshot(&store), before);
    lock.unlock().unwrap();
}

#[cfg(unix)]
#[test]
fn rewritten_config_keeps_mode_0600_and_leaves_no_temp_files() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    env.init();
    let team = env.source_dir("team");
    env.json(&[
        "source",
        "add",
        team.to_str().unwrap(),
        "--id",
        "team",
        "--priority",
        "3",
    ]);
    env.json(&["select", "github"]);
    env.json(&["unselect", "github"]);
    let store = env.default_store();
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&store.join("config.toml")), 0o600);
    assert_eq!(mode(&store.join(".lock")), 0o600);
    assert_eq!(mode(&store), 0o700);
    let mut names: Vec<String> = fs::read_dir(&store)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [".lock", "config.toml", "local"]);
}

#[test]
fn store_path_flag_beats_env_for_every_command() {
    let env = TestEnv::new();
    let flag = env.root().join("flag-store");
    let bogus = env.root().join("env-store");
    env.cmd()
        .args(["init", "--store"])
        .arg(&flag)
        .assert()
        .success();
    for args in [
        vec!["resolve"],
        vec!["source", "list"],
        vec!["select", "x"],
        vec!["unselect", "x"],
    ] {
        let output = env
            .cmd()
            .env("D_MCP_STORE", &bogus)
            .arg("--json")
            .args(&args)
            .arg("--store")
            .arg(&flag)
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(
            value["error"]["code"], "store-not-initialized",
            "{args:?} ignored --store"
        );
    }
    assert!(!bogus.exists());
}
