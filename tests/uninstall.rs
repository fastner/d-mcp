//! `d-mcp uninstall`: the read-only preview, the removal of entries d-mcp
//! created through a scoped sync, the release of adopted and taken-over
//! entries, drifted entries left with exit 1, and `--purge` (known store
//! entries only, the Git clones under `sources/` included, blocked by
//! location changes, a missing `claude`, a symlinked store root without
//! `--follow-store-link`, and a `local/` Git repository without
//! `--include-local-repo`), against the fake `claude` and Codex config
//! files in an isolated environment.

mod common;

use std::fs;
use std::path::Path;

use common::{TestEnv, snapshot, stdio};
use d_mcp::error::Error;
use d_mcp::store::StorePaths;
use d_mcp::sync::{SyncHooks, SyncOptions, sync};
use serde_json::{Value, json};

/// An initialized store with the fake `claude` and the fake `codex` on
/// `PATH` and an empty Codex file.
fn setup() -> TestEnv {
    let env = TestEnv::new();
    let claude = env.fake_claude_dir();
    let codex = env.fake_codex_dir();
    let env = env.with_path(&[claude, codex]);
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

fn ok(env: &TestEnv, args: &[&str]) -> Value {
    let (code, value, stderr) = run(env, &[], args);
    assert_eq!(code, 0, "{args:?}: {value:#}\n{stderr}");
    value
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

fn claude_servers(env: &TestEnv) -> Value {
    let text = fs::read_to_string(env.claude_config()).unwrap_or_else(|_| "{}".to_owned());
    serde_json::from_str::<Value>(&text).unwrap()["mcpServers"].clone()
}

fn state(env: &TestEnv) -> String {
    fs::read_to_string(env.default_store().join("state.toml")).unwrap_or_default()
}

fn config(env: &TestEnv) -> String {
    fs::read_to_string(env.default_store().join("config.toml")).unwrap()
}

/// Snapshots of the harness files and the store.
fn everything(env: &TestEnv) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    let mut all = snapshot(&env.default_store());
    all.extend(snapshot(env.claude_config().parent().unwrap()));
    all.extend(snapshot(&env.codex_home()));
    all.retain(|(path, _)| {
        // The fake's log records every call, the preview's too.
        !path.ends_with("fake-claude.log")
    });
    all
}

#[test]
fn owned_entries_are_removed_after_a_read_only_preview() {
    let env = setup();
    env.write_local("github", &stdio("github", "npx"));
    env.write_local("tools", &stdio("tools", "x"));
    ok(&env, &["select", "github", "--harness", "codex"]);
    ok(&env, &["select", "tools"]);
    ok(&env, &["sync"]);
    assert!(claude_servers(&env)["tools"].is_object());

    let before = everything(&env);
    let preview = ok(&env, &["uninstall"]);
    assert_eq!(preview["data"]["apply"], false);
    assert_eq!(preview["data"]["dry_run"], true);
    for (harness, name) in [("codex", "github"), ("codex", "tools"), ("claude", "tools")] {
        let item = entry(&preview, harness, name);
        assert_eq!(item["action"], "remove", "{item}");
        assert_eq!(item["outcome"], "planned");
    }
    let dry = ok(&env, &["--dry-run", "uninstall", "--apply"]);
    assert_eq!(dry["data"]["harnesses"], preview["data"]["harnesses"]);
    assert_eq!(dry["data"]["dry_run"], true);
    assert_eq!(everything(&env), before, "the preview wrote something");
    // Uninstall never runs `codex`.
    assert!(!env.codex_home().join("fake-codex.log").exists());

    let applied = ok(&env, &["uninstall", "--apply"]);
    assert_eq!(applied["data"]["dry_run"], false);
    for (harness, name) in [("codex", "github"), ("codex", "tools"), ("claude", "tools")] {
        assert_eq!(entry(&applied, harness, name)["outcome"], "applied");
    }
    let sync = &applied["data"]["sync"];
    assert_eq!(sync["harnesses"].as_array().unwrap().len(), 2, "{sync:#}");
    assert!(
        !fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("github")
    );
    assert_eq!(claude_servers(&env), json!({}));
    assert!(!state(&env).contains("[[records]]"), "{}", state(&env));
    assert!(!config(&env).contains("[[selections]]"), "{}", config(&env));
    // Nothing is left to uninstall.
    let again = ok(&env, &["uninstall", "--apply"]);
    assert!(
        again["data"]["harnesses"][0]["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (code, out, _) = env.text(&["uninstall"]);
    assert_eq!(code, 0);
    assert!(out.contains("(no managed entries)"), "{out}");
    assert!(out.contains("Preview: nothing was written"), "{out}");
}

#[test]
fn adopted_and_taken_over_entries_stay_in_the_harness_unowned() {
    let env = setup();
    fs::write(
        env.claude_config(),
        json!({ "mcpServers": { "notes": { "command": "notes", "args": [], "env": { "NOTES_TOKEN": "lit" } } } })
            .to_string(),
    )
    .unwrap();
    fs::write(
        env.codex_config(),
        "[mcp_servers.time]\ncommand = \"time\"\n",
    )
    .unwrap();
    let vars = [("NOTES_TOKEN", "lit")];
    let (code, preview, _) = run(&env, &vars, &["adopt", "claude:user:notes"]);
    assert_eq!(code, 0, "{preview:#}");
    let hash = preview["data"]["servers"][0]["entries"][0]["display_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let expect = format!("--expect=claude:user:notes={hash}");
    let (code, value, _) = run(
        &env,
        &vars,
        &["adopt", "claude:user:notes", "--apply", &expect],
    );
    assert_eq!(code, 0, "{value:#}");
    env.write_local("time", &stdio("time", "time"));
    ok(&env, &["select", "time", "--harness", "codex"]);
    ok(&env, &["sync", "--take-over", "time"]);
    let adopted = claude_servers(&env)["notes"].clone();
    assert_eq!(adopted["env"]["NOTES_TOKEN"], "${NOTES_TOKEN:-}");

    let value = ok(&env, &["uninstall", "--apply"]);
    let notes = entry(&value, "claude", "notes");
    assert_eq!(notes["action"], "release");
    assert_eq!(notes["outcome"], "applied");
    assert_eq!(notes["adopted_from"], "claude:user");
    assert_eq!(
        notes["replaced_fields"],
        json!([{ "field": "env.NOTES_TOKEN", "var": "NOTES_TOKEN" }])
    );
    assert!(
        notes["notice"]
            .as_str()
            .unwrap()
            .contains("NOTES_TOKEN (env.NOTES_TOKEN)")
    );
    let time = entry(&value, "codex", "time");
    assert_eq!(time["action"], "release");
    // Nothing was written: both entries are still there, unowned.
    assert_eq!(claude_servers(&env)["notes"], adopted);
    assert!(
        fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("[mcp_servers.time]")
    );
    assert!(!state(&env).contains("[[records]]"));
    assert!(!config(&env).contains("[[selections]]"));
    // The definition stays in `local/` (only `--purge` deletes it).
    assert!(
        env.default_store()
            .join("local/servers/notes.toml")
            .exists()
    );
    assert!(!value["data"]["sync"].is_object());
}

#[test]
fn drifted_entries_are_left_with_exit_1_and_keep_their_records() {
    let env = setup();
    env.write_local("wiki", &stdio("wiki", "w"));
    env.write_local("github", &stdio("github", "npx"));
    ok(&env, &["select", "wiki", "--harness", "codex"]);
    ok(&env, &["select", "github", "--harness", "codex"]);
    ok(&env, &["sync"]);
    let text = fs::read_to_string(env.codex_config()).unwrap();
    fs::write(
        env.codex_config(),
        text.replace("command = \"w\"", "command = \"edited\""),
    )
    .unwrap();

    let (code, value, _) = run(&env, &[], &["uninstall", "--apply"]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "uninstall-incomplete");
    let wiki = entry(&value, "codex", "wiki");
    assert_eq!(wiki["action"], "drifted");
    assert_eq!(wiki["outcome"], "left");
    assert!(
        wiki["notice"]
            .as_str()
            .unwrap()
            .contains("--take-over wiki")
    );
    assert_eq!(entry(&value, "codex", "github")["outcome"], "applied");
    let codex = fs::read_to_string(env.codex_config()).unwrap();
    assert!(codex.contains("command = \"edited\"") && !codex.contains("[mcp_servers.github]"));
    assert!(state(&env).contains("name = \"wiki\""));
    assert!(!state(&env).contains("name = \"github\""));
    // The drifted entry keeps its selection.
    assert!(config(&env).contains("server = \"wiki\""));
    assert!(!config(&env).contains("server = \"github\""));

    // `--purge` drops its record with the store; the entry is still left.
    let (code, value, _) = run(&env, &[], &["uninstall", "--apply", "--purge"]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(!env.default_store().exists());
    assert!(
        fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("edited")
    );
}

#[test]
fn pending_records_are_settled_first() {
    let env = setup();
    env.write_local("github", &stdio("github", "npx"));
    ok(&env, &["select", "github", "--harness", "codex"]);
    let crash = || -> d_mcp::error::Result<()> { Err(Error::HomeUnset) };
    let options = SyncOptions {
        hooks: SyncHooks {
            before_promote: Some(&crash),
            ..SyncHooks::default()
        },
        harnesses: [d_mcp::config::Harness::Codex].into_iter().collect(),
        ..SyncOptions::default()
    };
    let paths = StorePaths::new(env.default_store());
    assert!(sync(&paths, &env.environment(), &options, &mut Vec::new()).is_err());
    assert!(state(&env).contains("status = \"pending\""));
    let value = ok(&env, &["uninstall", "--apply"]);
    let codex = &value["data"]["harnesses"][0];
    assert!(
        codex["notices"]
            .to_string()
            .contains("an interrupted write of `github` was completed"),
        "{codex:#}"
    );
    assert_eq!(entry(&value, "codex", "github")["outcome"], "applied");
    assert!(
        !fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("github")
    );
}

#[test]
fn purge_deletes_only_known_store_entries() {
    let env = setup();
    let team = env.source_dir("team");
    fs::write(team.join("servers/github.toml"), stdio("github", "npx")).unwrap();
    ok(
        &env,
        &[
            "source",
            "add",
            team.to_str().unwrap(),
            "--id",
            "team",
            "--priority",
            "5",
        ],
    );
    ok(&env, &["select", "github", "--harness", "codex"]);
    ok(&env, &["sync"]);
    let store = env.default_store();
    // A foreign file in the root, and a link inside `local/` to a file
    // outside the store.
    fs::write(store.join("notes.txt"), "mine").unwrap();
    let outside = env.root().join("outside.toml");
    fs::write(&outside, "keep").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, store.join("local/servers/linked.toml")).unwrap();

    let preview = ok(&env, &["uninstall", "--purge"]);
    let purge = &preview["data"]["purge"];
    assert_eq!(purge["requested"], true);
    assert_eq!(purge["deleted"], false);
    let listed: Vec<&str> = purge["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    let name = |path: &str| {
        Path::new(path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    };
    let names: Vec<String> = listed.iter().map(|path| name(path)).collect();
    assert_eq!(
        names,
        ["config.toml", "local", "state.toml", ".hash-key", ".lock"]
    );
    assert!(store.join("config.toml").exists());

    let value = ok(&env, &["uninstall", "--apply", "--purge"]);
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert_eq!(entry(&value, "codex", "github")["outcome"], "applied");
    for known in ["config.toml", "local", "state.toml", ".hash-key", ".lock"] {
        assert!(!store.join(known).exists(), "{known} survived");
    }
    // The root keeps its foreign file, so it stays; nothing outside the
    // store is touched.
    assert_eq!(fs::read_to_string(store.join("notes.txt")).unwrap(), "mine");
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    assert!(team.join("servers/github.toml").exists());
    assert!(
        !fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("github")
    );
}

#[test]
fn purge_is_blocked_by_location_changes() {
    let env = setup();
    env.write_local("github", &stdio("github", "npx"));
    ok(&env, &["select", "github", "--harness", "codex"]);
    ok(&env, &["sync"]);
    let other = env.root().join("codex-other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("config.toml"), "").unwrap();
    let output = env
        .cmd()
        .env("CODEX_HOME", &other)
        .args(["--json", "uninstall", "--apply", "--purge"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "purge-blocked");
    assert_eq!(
        value["data"]["purge"]["blocked"],
        json!([{ "reason": "location-changed", "harness": "codex" }])
    );
    let github = entry(&value, "codex", "github");
    assert_eq!(github["action"], "location-changed");
    assert_eq!(github["outcome"], "left");
    assert!(env.default_store().join("config.toml").exists());
    assert!(state(&env).contains("name = \"github\""));
}

#[test]
fn a_missing_claude_blocks_removals_and_purge_with_exit_4() {
    let env = setup();
    env.write_local("tools", &stdio("tools", "x"));
    ok(&env, &["select", "tools", "--harness", "claude"]);
    ok(&env, &["sync"]);
    let output = env
        .cmd()
        .env("PATH", env.root().join("empty-bin"))
        .args(["--json", "uninstall", "--apply", "--purge"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "harness-cli-not-found");
    let claude = value["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["harness"] == "claude")
        .unwrap();
    assert_eq!(claude["status"], "failed");
    assert_eq!(entry(&value, "claude", "tools")["outcome"], "left");
    assert!(
        value["data"]["purge"]["blocked"]
            .to_string()
            .contains("harness-cli-not-found")
    );
    assert!(claude_servers(&env)["tools"].is_object());
    assert!(env.default_store().join("config.toml").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_store_root_is_purged_only_with_follow_store_link() {
    let env = setup();
    let real = env.root().join("real-store");
    let link = env.root().join("link-store");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let store = link.to_str().unwrap();
    ok(&env, &["--store", store, "init"]);
    let (code, value, stderr) = run(
        &env,
        &[],
        &["--store", store, "uninstall", "--apply", "--purge"],
    );
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(value["error"]["code"], "purge-blocked");
    assert_eq!(
        value["data"]["purge"]["blocked"],
        json!([{ "reason": "store-root-symlink", "harness": null }])
    );
    assert_eq!(value["data"]["purge"]["store_root_symlink"], true);
    assert!(stderr.contains("only with --follow-store-link"), "{stderr}");
    assert!(real.join("config.toml").exists());

    let (code, value, stderr) = run(
        &env,
        &[],
        &[
            "--store",
            store,
            "uninstall",
            "--apply",
            "--purge",
            "--follow-store-link",
        ],
    );
    assert_eq!(code, 0, "{value:#}");
    assert!(
        stderr.contains("deletes the store files inside its target"),
        "{stderr}"
    );
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(!real.join("config.toml").exists());
    // The link and its target directory are left.
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(real.is_dir());
    // `--follow-store-link` needs `--purge`.
    let (code, _, _) = env.text(&["uninstall", "--follow-store-link"]);
    assert_eq!(code, 2);
}

/// An error after planning (here: the store directory cannot be written,
/// so releasing the records fails) is reported next to the plan: `data`
/// is present, the entries are still `planned`, and the purge did not run.
#[cfg(unix)]
#[test]
fn an_error_during_the_apply_keeps_the_plan_as_data() {
    use std::os::unix::fs::PermissionsExt;
    let env = setup();
    fs::write(
        env.codex_config(),
        "[mcp_servers.time]\ncommand = \"time\"\n",
    )
    .unwrap();
    env.write_local("time", &stdio("time", "time"));
    ok(&env, &["select", "time", "--harness", "codex"]);
    ok(&env, &["sync", "--take-over", "time"]);
    let store = env.default_store();
    let mode = |mode: u32| fs::set_permissions(&store, fs::Permissions::from_mode(mode)).unwrap();
    mode(0o555);
    if fs::write(store.join("probe"), "").is_ok() {
        // Running as root: permissions do not apply.
        fs::remove_file(store.join("probe")).unwrap();
        mode(0o755);
        return;
    }
    let (code, value, stderr) = run(&env, &[], &["uninstall", "--apply", "--purge"]);
    mode(0o755);
    assert_eq!(code, 4, "{value:#}\n{stderr}");
    assert_eq!(value["error"]["code"], "io-error");
    let time = entry(&value, "codex", "time");
    assert_eq!(time["action"], "release");
    assert_eq!(time["outcome"], "planned");
    assert_eq!(value["data"]["purge"]["deleted"], false);
    assert!(
        value["data"]["notices"]
            .to_string()
            .contains("the apply stopped at an error"),
        "{value:#}"
    );
    assert!(state(&env).contains("name = \"time\""));
    assert!(store.join("config.toml").exists());
}

/// An error in a later step (here: the scoped sync of the second harness,
/// Codex, after the first one, Claude Code, removed its entry) reports what
/// already happened: the release and the first removal are `applied`, the
/// stopped removal is `failed`, the first harness's `sync` document is
/// kept, and the purge did not run.
#[test]
fn an_error_in_a_later_step_keeps_the_outcomes_of_earlier_ones() {
    let env = setup();
    fs::write(
        env.codex_config(),
        "[mcp_servers.time]\ncommand = \"time\"\n",
    )
    .unwrap();
    env.write_local("time", &stdio("time", "time"));
    env.write_local("tools", &stdio("tools", "x"));
    env.write_local("github", &stdio("github", "npx"));
    ok(&env, &["select", "time", "--harness", "codex"]);
    ok(&env, &["select", "tools", "--harness", "claude"]);
    ok(&env, &["select", "github", "--harness", "codex"]);
    ok(&env, &["sync", "--take-over", "time"]);
    assert!(claude_servers(&env)["tools"].is_object());
    // After `claude mcp remove tools`, `config.toml` is no longer TOML, so
    // the Codex sync stops before it changes anything.
    env.claude_mode(&["clobber-after remove tools home/.d-mcp/config.toml"]);
    let (code, value, stderr) = run(&env, &[], &["uninstall", "--apply", "--purge"]);
    assert_eq!(code, 3, "{value:#}\n{stderr}");
    assert_eq!(value["error"]["code"], "config-corrupt", "{value:#}");
    let outcome = |harness: &str, name: &str| entry(&value, harness, name)["outcome"].clone();
    assert_eq!(entry(&value, "codex", "time")["action"], "release");
    assert_eq!(outcome("codex", "time"), "applied");
    assert_eq!(outcome("claude", "tools"), "applied");
    assert_eq!(outcome("codex", "github"), "failed");
    let sync = &value["data"]["sync"]["harnesses"];
    assert_eq!(sync.as_array().unwrap().len(), 1, "{sync:#}");
    assert_eq!(sync[0]["harness"], "claude");
    assert_eq!(value["data"]["purge"]["deleted"], false);
    let notices = value["data"]["notices"].to_string();
    assert!(
        notices.contains("entries `failed` may be partly done"),
        "{notices}"
    );
    assert_eq!(claude_servers(&env), json!({}));
    assert!(
        fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("[mcp_servers.github]")
    );
    let state = state(&env);
    assert!(!state.contains("name = \"tools\""), "{state}");
    assert!(!state.contains("name = \"time\""), "{state}");
    assert!(state.contains("name = \"github\""), "{state}");
}

/// A store with a Git source, the clone and snapshot under `sources/`.
fn with_git_source() -> (TestEnv, common::Remote) {
    let env = TestEnv::new().with_git();
    env.init();
    let remote = env.remote("personal");
    remote.write("servers/github.toml", stdio("github", "npx"));
    remote.commit("first");
    ok(&env, &["source", "add-git", &remote.url()]);
    assert!(env.default_store().join("sources/personal/repo").is_dir());
    (env, remote)
}

fn entry_names(value: &Value) -> Vec<String> {
    value["data"]["purge"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| {
            Path::new(path.as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// `--purge` deletes `sources/` with the Git clones and snapshots, and
/// never touches the remote or a registered directory source.
#[test]
fn purge_deletes_the_git_clones_under_sources() {
    let (env, remote) = with_git_source();
    let team = env.source_dir("team");
    fs::write(team.join("servers/wiki.toml"), stdio("wiki", "w")).unwrap();
    ok(
        &env,
        &[
            "source",
            "add",
            team.to_str().unwrap(),
            "--id",
            "team",
            "--priority",
            "5",
        ],
    );
    let store = env.default_store();
    let preview = ok(&env, &["uninstall", "--purge"]);
    assert_eq!(
        entry_names(&preview),
        [
            "config.toml",
            "local",
            "state.toml",
            ".hash-key",
            "lock.toml",
            "sources",
            ".lock"
        ]
        .iter()
        .filter(|name| store.join(name).exists())
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>()
    );
    assert!(entry_names(&preview).contains(&"sources".to_owned()));
    assert_eq!(preview["data"]["purge"]["local_is_git"], false);
    let remote_before = snapshot(&remote.bare);
    let value = ok(&env, &["uninstall", "--apply", "--purge"]);
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(!store.exists(), "the empty store root is removed");
    assert_eq!(snapshot(&remote.bare), remote_before);
    assert!(team.join("servers/wiki.toml").exists());
}

/// A `local/` that is a Git repository blocks `--purge` (`local-is-git`)
/// until `--include-local-repo` is given.
#[test]
fn a_local_git_repository_blocks_purge_without_include_local_repo() {
    let (env, remote) = with_git_source();
    let store = env.default_store();
    let local = store.join("local");
    remote.git_in(&local, &["init", "-q", "-b", "main"]);
    assert!(local.join(".git").is_dir());
    let before = snapshot(&store);
    for args in [
        &["uninstall", "--purge"][..],
        &["uninstall", "--apply", "--purge"],
    ] {
        let (code, value, stderr) = run(&env, &[], args);
        assert_eq!(code, 1, "{args:?}: {value:#}");
        assert_eq!(value["error"]["code"], "purge-blocked");
        assert_eq!(
            value["data"]["purge"]["blocked"],
            json!([{ "reason": "local-is-git", "harness": null }])
        );
        assert_eq!(value["data"]["purge"]["local_is_git"], true);
        assert_eq!(value["data"]["purge"]["deleted"], false);
        assert!(stderr.contains("--include-local-repo"), "{stderr}");
        assert!(stderr.contains("unpushed work"), "{stderr}");
    }
    assert_eq!(snapshot(&store), before);
    // The text form names the reason.
    let output = env
        .cmd()
        .args(["uninstall", "--apply", "--purge"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Purge blocked: local-is-git"));
    // `--include-local-repo` needs `--purge`.
    let output = env
        .cmd()
        .args(["uninstall", "--apply", "--include-local-repo"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(snapshot(&store), before);

    let (code, value, stderr) = run(
        &env,
        &[],
        &["uninstall", "--apply", "--purge", "--include-local-repo"],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(value["data"]["purge"]["include_local_repo"], true);
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(stderr.contains("deletes the Git repository"), "{stderr}");
    assert!(!store.exists());
}

/// A symlinked `local` is only unlinked by `--purge`: even when its target
/// is a Git repository it does not block the purge, the warning says that
/// the target stays, and the target is left as it is.
#[cfg(unix)]
#[test]
fn a_symlinked_local_git_repository_is_only_unlinked() {
    let (env, remote) = with_git_source();
    let store = env.default_store();
    let outside = env.root().join("outside-local");
    fs::create_dir_all(outside.join("servers")).unwrap();
    remote.git_in(&outside, &["init", "-q", "-b", "main"]);
    fs::write(outside.join("servers/wiki.toml"), stdio("wiki", "w")).unwrap();
    fs::remove_dir_all(store.join("local")).unwrap();
    std::os::unix::fs::symlink(&outside, store.join("local")).unwrap();
    let before = snapshot(&outside);

    let (code, value, stderr) = run(&env, &[], &["uninstall", "--purge"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(value["data"]["purge"]["local_is_git"], false);
    assert_eq!(value["data"]["purge"]["blocked"], json!([]));
    assert!(
        stderr.contains("is a symlink: --purge would only unlink it"),
        "{stderr}"
    );
    assert!(stderr.contains(&outside.display().to_string()), "{stderr}");
    assert!(!stderr.contains("unpushed work"), "{stderr}");

    let (code, value, stderr) = run(&env, &[], &["uninstall", "--apply", "--purge"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(stderr.contains("--purge only unlinks it"), "{stderr}");
    assert!(!store.exists(), "the empty store root is removed");
    assert_eq!(snapshot(&outside), before);
    assert!(outside.join(".git").is_dir());
}

/// `--purge` unlinks a symlink under `sources/`, and a `sources` that is a
/// symlink itself, without following either.
#[cfg(unix)]
#[test]
fn purge_never_follows_symlinks_under_or_at_sources() {
    // A symlink inside `sources/`.
    let (env, _remote) = with_git_source();
    let store = env.default_store();
    let outside = env.root().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, store.join("sources/linked")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("keep.txt"),
        store.join("sources/personal/snapshot/linked.txt"),
    )
    .unwrap();
    let before = snapshot(&outside);
    let value = ok(&env, &["uninstall", "--apply", "--purge"]);
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(!store.exists());
    assert_eq!(snapshot(&outside), before);

    // `sources` itself is a symlink.
    let (env, _remote) = with_git_source();
    let store = env.default_store();
    let outside = env.root().join("outside-sources");
    fs::rename(store.join("sources"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, store.join("sources")).unwrap();
    let before = snapshot(&outside);
    assert!(!before.is_empty());
    let preview = ok(&env, &["uninstall", "--purge"]);
    assert!(entry_names(&preview).contains(&"sources".to_owned()));
    let value = ok(&env, &["uninstall", "--apply", "--purge"]);
    assert_eq!(value["data"]["purge"]["deleted"], true);
    assert!(fs::symlink_metadata(store.join("sources")).is_err());
    assert_eq!(snapshot(&outside), before);
    assert!(outside.join("personal/repo/.git").is_dir());
}

/// A corrupt `lock.toml` or `approvals.toml` stops `uninstall` (preview
/// and apply, with or without `--purge`) before anything is written: the
/// scoped removals resolve through both files.
#[test]
fn corrupt_lock_or_approvals_stop_uninstall_before_any_write() {
    for (file, code) in [
        ("lock.toml", "lock-corrupt"),
        ("approvals.toml", "approvals-corrupt"),
    ] {
        let env = setup();
        env.write_local("github", &stdio("github", "npx"));
        ok(&env, &["select", "github", "--harness", "codex"]);
        ok(&env, &["sync"]);
        assert!(
            fs::read_to_string(env.codex_config())
                .unwrap()
                .contains("github")
        );
        fs::write(
            env.default_store().join(file),
            "schema_version = 1\nx = 1\n",
        )
        .unwrap();
        let before = everything(&env);
        for args in [
            &["uninstall"][..],
            &["uninstall", "--apply"],
            &["uninstall", "--apply", "--purge"],
        ] {
            let (exit, value, _) = run(&env, &[], args);
            assert_eq!(exit, 3, "{file} {args:?}: {value:#}");
            assert_eq!(value["error"]["code"], code, "{file} {args:?}");
            assert_eq!(value["data"], Value::Null, "{file} {args:?}");
            assert_eq!(everything(&env), before, "{file} {args:?} wrote");
        }
    }
}
