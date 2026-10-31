//! Git sources: `source add-git`, snapshots, removal, offline reads,
//! failure injection, input validation and hostile repositories. Every
//! remote is a local bare repository; no test uses the network.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Remote, TestEnv, snapshot, stdio, system_git};
use serde_json::Value;

/// A store with one remote offering `github` (stdio) and `linear` (http).
fn setup() -> (TestEnv, Remote) {
    let env = TestEnv::new().with_git();
    env.init();
    let remote = env.remote("personal");
    remote.write("servers/github.toml", stdio("github", "npx"));
    remote.write(
        "servers/linear.toml",
        "name = \"linear\"\n[transport]\ntype = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n",
    );
    remote.write("README.md", "definitions\n");
    remote.commit("first");
    (env, remote)
}

fn add_git(env: &TestEnv, args: &[&str]) -> (i32, Value, String) {
    let mut all = vec!["source", "add-git"];
    all.extend_from_slice(args);
    env.json(&all)
}

fn error_code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or_default()
}

fn store(env: &TestEnv) -> PathBuf {
    env.default_store()
}

/// The entries of `<store>/sources`, sorted; empty when it is missing.
fn sources_entries(env: &TestEnv) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(store(env).join("sources"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Replaces the `git` on d-mcp's `PATH` with a shell script.
#[cfg(unix)]
fn replace_git(env: &TestEnv, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let git = env.git_bin().join("git");
    fs::remove_file(&git).unwrap();
    fs::write(&git, script).unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `git` that records every call in `<root>/git-called` and fails.
#[cfg(unix)]
fn record_git_calls(env: &TestEnv) -> PathBuf {
    let marker = env.root().join("git-called");
    replace_git(
        env,
        &format!("#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n", marker.display()),
    );
    marker
}

/// A `git` that fails on the first call with `subcommand` and runs the
/// real git otherwise.
#[cfg(unix)]
fn fail_git_on(env: &TestEnv, subcommand: &str) {
    replace_git(
        env,
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = '{subcommand}' ]; then exit 1; fi\ndone\nexec '{}' \"$@\"\n",
            system_git().display()
        ),
    );
}

fn resolve_server<'v>(value: &'v Value, name: &str) -> &'v Value {
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == name)
        .unwrap_or_else(|| panic!("no server {name} in {value}"))
}

#[test]
fn add_git_clones_pins_and_snapshots() {
    let (env, remote) = setup();
    let head = remote.head();
    let (code, value, err) = add_git(&env, &[&remote.url(), "--id", "personal", "--trusted"]);
    assert_eq!(code, 0, "{value} {err}");
    let source = &value["data"]["source"];
    assert_eq!(source["kind"], "git");
    assert_eq!(source["id"], "personal");
    assert_eq!(source["commit"], head.as_str());
    assert_eq!(source["ref"], "main");
    assert_eq!(source["url"], remote.url().as_str());
    assert_eq!(source["trusted"], true);
    assert_eq!(source["available"], true);
    assert_eq!(source["priority"], 1);
    let location = store(&env).join("sources/personal");
    assert_eq!(source["path"], location.to_str().unwrap());
    assert_eq!(
        value["data"]["offered"],
        serde_json::json!(["github", "linear"])
    );

    // The clone has no checkout and keeps the pin reachable.
    assert!(location.join("repo/.git").is_dir());
    assert!(!location.join("repo/servers").exists());
    let pin = remote
        .command(&location.join("repo"))
        .args(["rev-parse", "refs/d-mcp/pin"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(pin.stdout).unwrap().trim(), head);
    // The snapshot holds the definitions and the stamp.
    assert_eq!(
        fs::read_to_string(location.join("snapshot/servers/github.toml")).unwrap(),
        stdio("github", "npx")
    );
    assert!(!location.join("snapshot/README.md").exists());
    assert_eq!(
        fs::read_to_string(location.join("snapshot/commit")).unwrap(),
        format!("{head}\n")
    );
    assert_eq!(
        fs::read_to_string(store(&env).join("lock.toml")).unwrap(),
        format!(
            "schema_version = 1\n\n[[pins]]\nsource = \"personal\"\ncommit = \"{head}\"\nref = \"refs/heads/main\"\n"
        )
    );
    let config = fs::read_to_string(store(&env).join("config.toml")).unwrap();
    assert!(config.contains("kind = \"git\""), "{config}");
    assert!(config.contains("trusted = true"), "{config}");
    assert!(!config.contains("path"), "{config}");
    assert_eq!(sources_entries(&env), ["personal"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store(&env).join("sources")), 0o700);
        assert_eq!(mode(&store(&env).join("lock.toml")), 0o600);
        assert_eq!(mode(&location.join("snapshot/servers/github.toml")), 0o600);
    }

    // Read-only commands see the snapshot; the shared loader resolves it.
    env.cmd().args(["select", "github"]).assert().success();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    assert_eq!(resolve_server(&value, "github")["source"], "personal");
    let (code, value, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);
    let listed = &value["data"]["sources"][0];
    assert_eq!(listed["id"], "personal");
    assert_eq!(listed["commit"], head.as_str());
    let local = &value["data"]["sources"][1];
    assert_eq!(local["url"], Value::Null);
    assert_eq!(local["trusted"], Value::Null);
    let (code, out, _) = env.text(&["source", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("personal\t1\tgit\tavailable\t"), "{out}");
}

#[test]
fn add_git_text_output_and_defaults() {
    let (env, remote) = setup();
    let (code, out, err) = env.text(&["source", "add-git", &remote.url()]);
    assert_eq!(code, 0, "{err}");
    // The id comes from the URL's last segment; untrusted by default.
    assert!(
        out.starts_with(&format!(
            "Added Git source `personal` (priority 1) at {}: {}",
            &remote.head()[..12],
            remote.url()
        )),
        "{out}"
    );
    assert!(out.contains("offers: github, linear"), "{out}");
    let config = fs::read_to_string(store(&env).join("config.toml")).unwrap();
    assert!(config.contains("trusted = false"), "{config}");
    // The default priority is one above the highest user source.
    let other = env.remote("other");
    other.write("servers/a.toml", stdio("a", "npx"));
    other.commit("a");
    let (code, value, _) = add_git(&env, &[&other.url()]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["source"]["priority"], 2);
    // Taken ids and priorities fail before anything is cloned.
    let (code, value, _) = add_git(&env, &[&other.url()]);
    assert_eq!((code, error_code(&value)), (1, "source-id-taken"));
    let (code, value, _) = add_git(&env, &[&other.url(), "--id", "x", "--priority", "2"]);
    assert_eq!((code, error_code(&value)), (1, "priority-taken"));
    let (code, value, _) = add_git(&env, &[&other.url(), "--id", "x", "--priority", "0"]);
    assert_eq!((code, error_code(&value)), (1, "priority-invalid"));
    let (code, value, _) = add_git(&env, &[&other.url(), "--id", "local"]);
    assert_eq!((code, error_code(&value)), (1, "source-id-reserved"));
    let (code, value, _) = add_git(&env, &[&other.url(), "--id", "Bad"]);
    assert_eq!((code, error_code(&value)), (1, "invalid-name"));
    assert_eq!(sources_entries(&env), ["other", "personal"]);
}

#[test]
fn refs_tags_and_commits() {
    let (env, remote) = setup();
    let first = remote.head();
    remote.git(&["tag", "v1"]);
    remote.git(&["push", "-q", "origin", "v1"]);
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let second = remote.commit("second");
    remote.git(&["checkout", "-q", "-b", "dev"]);
    remote.write("servers/dev.toml", stdio("dev", "npx"));
    let dev = remote.commit("dev");
    remote.git(&["checkout", "-q", "main"]);

    let (code, value, err) = add_git(&env, &[&remote.url(), "--id", "dev", "--ref", "dev"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["source"]["ref"], "dev");
    assert_eq!(value["data"]["source"]["commit"], dev.as_str());
    let (code, value, _) = add_git(&env, &[&remote.url(), "--id", "tagged", "--ref", "v1"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["source"]["commit"], first.as_str());
    assert_eq!(
        value["data"]["offered"],
        serde_json::json!(["github", "linear"])
    );
    let (code, value, _) = add_git(&env, &[&remote.url(), "--id", "head"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["source"]["commit"], second.as_str());
    // `--commit` pins an older commit reachable from the ref (any hex case).
    let (code, value, _) = add_git(
        &env,
        &[
            &remote.url(),
            "--id",
            "old",
            "--commit",
            &first.to_uppercase(),
        ],
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["source"]["commit"], first.as_str());
    assert_eq!(value["data"]["source"]["ref"], "main");

    let before = snapshot(&store(&env));
    for (commit, reference) in [(dev.as_str(), "main"), (&"0".repeat(40), "main")] {
        let (code, value, _) = add_git(
            &env,
            &[
                &remote.url(),
                "--id",
                "bad",
                "--ref",
                reference,
                "--commit",
                commit,
            ],
        );
        assert_eq!(
            (code, error_code(&value)),
            (1, "source-commit-unreachable"),
            "{value}"
        );
    }
    let (code, value, _) = add_git(&env, &[&remote.url(), "--id", "bad", "--commit", "main"]);
    assert_eq!((code, error_code(&value)), (1, "source-ref-invalid"));
    let (code, value, _) = add_git(&env, &[&remote.url(), "--id", "bad", "--ref", "a..b"]);
    assert_eq!((code, error_code(&value)), (1, "source-ref-invalid"));
    let (code, value, _) = add_git(&env, &[&remote.url(), "--id", "bad", "--ref=-x"]);
    assert_eq!((code, error_code(&value)), (1, "source-ref-invalid"));
    let (code, value, _) = add_git(&env, &[&remote.url(), "--id", "bad", "--ref", "nope"]);
    assert_eq!((code, error_code(&value)), (4, "git-ref-not-found"));
    // Nothing is half-registered, and no temporary clone is left behind.
    assert_eq!(snapshot(&store(&env)), before);
}

#[test]
fn dry_run_leaves_the_store_byte_identical() {
    let (env, remote) = setup();
    let tmp = env.root().join("dry-tmp");
    fs::create_dir(&tmp).unwrap();
    let before = snapshot(&store(&env));
    let output = env
        .cmd()
        .env("TMPDIR", &tmp)
        .args([
            "--json",
            "--dry-run",
            "source",
            "add-git",
            &remote.url(),
            "--trusted",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["dry_run"], true);
    assert_eq!(value["data"]["source"]["commit"], remote.head().as_str());
    assert_eq!(value["data"]["source"]["available"], true);
    assert_eq!(
        value["data"]["offered"],
        serde_json::json!(["github", "linear"])
    );
    assert_eq!(snapshot(&store(&env)), before);
    assert!(!store(&env).join("sources").exists());
    // The temporary clone under TMPDIR is gone.
    assert_eq!(fs::read_dir(&tmp).unwrap().count(), 0);
    let (code, out, _) = env.text(&["--dry-run", "source", "add-git", &remote.url()]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("Dry run: would have added Git source `personal`"),
        "{out}"
    );
    assert_eq!(snapshot(&store(&env)), before);
}

#[test]
fn remove_deletes_the_clone_and_the_pin() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url()]);
    env.cmd().args(["select", "github"]).assert().success();
    let before = snapshot(&store(&env));
    let (code, value, _) = env.json(&["--dry-run", "source", "remove", "personal"]);
    assert_eq!(code, 0);
    assert_eq!(value["data"]["source"]["kind"], "git");
    assert_eq!(
        value["data"]["no_longer_offered"],
        serde_json::json!(["github"])
    );
    assert_eq!(snapshot(&store(&env)), before);
    let (code, value, err) = env.json(&["source", "remove", "personal"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        value["data"]["no_longer_offered"],
        serde_json::json!(["github"])
    );
    assert!(err.contains("no longer offered"), "{err}");
    assert!(sources_entries(&env).is_empty());
    assert_eq!(
        fs::read_to_string(store(&env).join("lock.toml")).unwrap(),
        "schema_version = 1\n"
    );
    let config = fs::read_to_string(store(&env).join("config.toml")).unwrap();
    assert!(!config.contains("personal"), "{config}");
    // The id can be added again.
    let (code, _, _) = add_git(&env, &[&remote.url()]);
    assert_eq!(code, 0);
}

#[cfg(unix)]
#[test]
fn read_only_commands_start_no_git_and_work_offline() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    env.cmd().args(["select", "github"]).assert().success();
    // The remote is gone and any git call would be recorded.
    fs::rename(&remote.bare, env.root().join("moved.git")).unwrap();
    let marker = record_git_calls(&env);
    let before = snapshot(&store(&env));
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    assert_eq!(resolve_server(&value, "github")["source"], "personal");
    for args in [
        &["source", "list"][..],
        &["status"],
        &["inventory"],
        &["doctor"],
        &["sync", "--dry-run"],
    ] {
        let (code, value, err) = env.json(args);
        assert_eq!(code, 0, "{args:?}: {value} {err}");
        if args == ["doctor"] {
            // The Git checks pass offline, from the store alone.
            for check in value["data"]["checks"].as_array().unwrap() {
                if ["lock", "approvals", "git"].contains(&check["check"].as_str().unwrap()) {
                    assert_eq!(check["severity"], "ok", "{check}");
                }
            }
        }
    }
    assert!(
        !marker.exists(),
        "git was started: {}",
        fs::read_to_string(&marker).unwrap()
    );
    assert_eq!(snapshot(&store(&env)), before);
}

#[test]
fn a_snapshot_that_does_not_match_the_pin_is_unavailable() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url()]);
    env.cmd().args(["select", "github"]).assert().success();
    let hold = |env: &TestEnv| {
        let (code, value, _) = env.json(&["resolve"]);
        assert_eq!(code, 0);
        let server = resolve_server(&value, "github").clone();
        (
            server["status"].as_str().unwrap().to_owned(),
            server["reason"].clone(),
        )
    };
    let stamp = store(&env).join("sources/personal/snapshot/commit");
    let lock = store(&env).join("lock.toml");
    let good_stamp = fs::read_to_string(&stamp).unwrap();
    let good_lock = fs::read_to_string(&lock).unwrap();
    // Not approved yet: held by the gate, not by availability.
    assert_eq!(hold(&env), ("hold".to_owned(), "pending-approval".into()));

    fs::write(&stamp, format!("{}\n", "a".repeat(40))).unwrap();
    assert_eq!(hold(&env), ("hold".to_owned(), "source-unavailable".into()));
    fs::write(&stamp, &good_stamp).unwrap();
    fs::write(&lock, "schema_version = 1\n").unwrap();
    assert_eq!(hold(&env), ("hold".to_owned(), "source-unavailable".into()));
    let (_, value, _) = env.json(&["source", "list"]);
    assert_eq!(value["data"]["sources"][0]["available"], false);
    assert_eq!(value["data"]["sources"][0]["commit"], Value::Null);
    fs::write(&lock, &good_lock).unwrap();
    fs::remove_dir_all(store(&env).join("sources/personal/snapshot")).unwrap();
    assert_eq!(hold(&env), ("hold".to_owned(), "source-unavailable".into()));
}

#[test]
fn a_commit_without_servers_makes_the_source_unavailable() {
    let env = TestEnv::new().with_git();
    env.init();
    let remote = env.remote("empty");
    remote.write("README.md", "nothing here\n");
    remote.commit("readme");
    let (code, value, err) = add_git(&env, &[&remote.url()]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["source"]["available"], false);
    assert_eq!(value["data"]["offered"], serde_json::json!([]));
    assert!(err.contains("has no servers/ directory"), "{err}");
    assert!(!store(&env).join("sources/empty/snapshot/servers").exists());
}

#[test]
fn corrupt_lock_and_approvals_files_are_unsafe_and_never_rewritten() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url()]);
    let lock = store(&env).join("lock.toml");
    fs::write(
        &lock,
        "schema_version = 1\n[[pins]]\nsource = \"personal\"\ncommit = \"main\"\n",
    )
    .unwrap();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!((code, error_code(&value)), (3, "lock-corrupt"));
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("pins[0].commit")
    );
    let other = env.remote("other");
    other.write("servers/a.toml", stdio("a", "npx"));
    other.commit("a");
    let (code, value, _) = add_git(&env, &[&other.url()]);
    assert_eq!((code, error_code(&value)), (3, "lock-corrupt"));
    let (code, value, _) = env.json(&["source", "remove", "personal"]);
    assert_eq!((code, error_code(&value)), (3, "lock-corrupt"));
    assert_eq!(
        fs::read_to_string(&lock).unwrap(),
        "schema_version = 1\n[[pins]]\nsource = \"personal\"\ncommit = \"main\"\n"
    );
    assert_eq!(sources_entries(&env), ["personal"]);
    // `doctor` still reports instead of failing on the file.
    let (code, _, _) = env.json(&["doctor"]);
    assert_ne!(code, 3);

    fs::remove_file(&lock).unwrap();
    fs::write(
        store(&env).join("approvals.toml"),
        "schema_version = 1\nbogus = 1\n",
    )
    .unwrap();
    for args in [&["resolve"][..], &["status"], &["inventory"]] {
        let (code, value, _) = env.json(args);
        assert_eq!(
            (code, error_code(&value)),
            (3, "approvals-corrupt"),
            "{args:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_failure_midway_leaves_no_half_registered_source() {
    for subcommand in ["update-ref", "cat-file", "ls-tree"] {
        let (env, remote) = setup();
        let before = snapshot(&store(&env));
        fail_git_on(&env, subcommand);
        let (code, value, _) = add_git(&env, &[&remote.url()]);
        assert_eq!(
            (code, error_code(&value)),
            (4, "git-failed"),
            "{subcommand}"
        );
        assert!(sources_entries(&env).is_empty(), "{subcommand}");
        fs::remove_dir(store(&env).join("sources")).unwrap();
        assert_eq!(snapshot(&store(&env)), before, "{subcommand}");
    }
    // An unreachable remote.
    let (env, _) = setup();
    let before = snapshot(&store(&env));
    let missing = env.root().join("missing.git");
    let (code, value, _) = add_git(&env, &[missing.to_str().unwrap()]);
    assert_eq!(
        (code, error_code(&value)),
        (4, "git-unreachable"),
        "{value}"
    );
    assert!(sources_entries(&env).is_empty());
    fs::remove_dir(store(&env).join("sources")).unwrap();
    assert_eq!(snapshot(&store(&env)), before);
}

#[test]
fn leftovers_of_a_killed_run_are_swept_and_replaced() {
    let (env, remote) = setup();
    let sources = store(&env).join("sources");
    fs::create_dir_all(sources.join(".tmp-stale/repo")).unwrap();
    fs::create_dir_all(sources.join("personal/junk")).unwrap();
    // A crash between `lock.toml` and `config.toml` left a stale pin.
    fs::write(
        store(&env).join("lock.toml"),
        format!(
            "schema_version = 1\n\n[[pins]]\nsource = \"personal\"\ncommit = \"{}\"\n",
            "a".repeat(40)
        ),
    )
    .unwrap();
    let (code, value, err) = add_git(&env, &[&remote.url()]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["source"]["commit"], remote.head().as_str());
    assert_eq!(sources_entries(&env), ["personal"]);
    assert!(!sources.join("personal/junk").exists());
    assert!(
        fs::read_to_string(store(&env).join("lock.toml"))
            .unwrap()
            .contains(&remote.head())
    );
}

#[test]
fn git_must_be_on_path() {
    let env = TestEnv::new();
    env.init();
    let before = snapshot(&store(&env));
    let (code, value, _) = add_git(&env, &["/srv/git/defs.git"]);
    assert_eq!((code, error_code(&value)), (4, "git-not-found"));
    assert_eq!(snapshot(&store(&env)), before);
}

#[cfg(unix)]
#[test]
fn hostile_urls_are_rejected_without_echo_and_without_git() {
    let (env, _) = setup();
    let marker = record_git_calls(&env);
    let before = snapshot(&store(&env));
    let cases: [(&[&str], &str); 8] = [
        (
            &["https://user:CANARYPASS@github.com/me/defs.git"],
            "url-userinfo",
        ),
        (&["git:CANARYPASS@github.com:me/defs.git"], "url-userinfo"),
        (
            &["https://github.com/me/defs.git?token=CANARYTOKEN"],
            "url-query",
        ),
        (&["https://github.com/me/defs.git#CANARYFRAG"], "url-query"),
        (&["ext::sh -c touch% /tmp/CANARYEXT"], "url-invalid"),
        (&["ext::CANARYEXT"], "url-scheme"),
        (&["http://github.com/me/CANARYHTTP.git"], "url-scheme"),
        (&["--", "--upload-pack=CANARYDASH"], "url-invalid"),
    ];
    for (args, rule) in cases {
        let mut all = vec!["source", "add-git"];
        all.extend_from_slice(args);
        for json in [true, false] {
            let (code, out, err) = if json {
                let (code, value, err) = env.json(&all);
                assert_eq!(error_code(&value), "source-url-invalid", "{args:?}");
                assert!(
                    value["error"]["hint"].as_str().unwrap().contains(rule),
                    "{args:?}"
                );
                (code, value.to_string(), err)
            } else {
                env.text(&all)
            };
            assert_eq!(code, 1, "{args:?}");
            assert!(
                !out.contains("CANARY") && !err.contains("CANARY"),
                "{args:?}: {out} {err}"
            );
        }
    }
    // A leading dash without `--` is a usage error.
    let (code, _, _) = env.text(&["source", "add-git", "--upload-pack=x"]);
    assert_eq!(code, 2);
    assert!(!marker.exists());
    assert_eq!(snapshot(&store(&env)), before);
}

#[cfg(unix)]
#[test]
fn hostile_repository_entries_are_neutralized() {
    let env = TestEnv::new().with_git();
    env.init();
    let remote = env.remote("hostile");
    remote.write("servers/good.toml", stdio("good", "npx"));
    remote.write("servers/big.toml", "#".repeat(70 * 1024));
    remote.write("servers/binary.toml", b"\xff\xfe");
    remote.write("servers/Bad__Stem.toml", stdio("x", "npx"));
    remote.write("servers/-dash.toml", stdio("x", "npx"));
    remote.write("servers/.hidden.toml", stdio("x", "npx"));
    remote.write("servers/notes.md", "x");
    remote.write("servers/nested/inner.toml", stdio("inner", "npx"));
    std::os::unix::fs::symlink(
        "../../../../../../etc/passwd",
        remote.work.join("servers/link.toml"),
    )
    .unwrap();
    remote.git(&["add", "-A"]);
    let anchor = remote.commit_staged("hostile");
    // A submodule (gitlink) under `servers/`.
    remote.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{anchor},servers/vendor.toml"),
    ]);
    remote.commit_staged("submodule");
    remote.push();

    let (code, value, err) = add_git(&env, &[&remote.url()]);
    assert_eq!(code, 0, "{err}");
    let snapshot_dir = store(&env).join("sources/hostile/snapshot/servers");
    // Nothing outside the repository was read or linked.
    let link = fs::symlink_metadata(snapshot_dir.join("link.toml")).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(
        fs::read_link(snapshot_dir.join("link.toml")).unwrap(),
        PathBuf::from("d-mcp-not-a-regular-file")
    );
    assert!(
        fs::symlink_metadata(snapshot_dir.join("vendor.toml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::metadata(snapshot_dir.join("big.toml")).unwrap().len(),
        64 * 1024 + 1
    );
    assert!(snapshot_dir.join("nested").is_dir());
    assert!(!snapshot_dir.join("nested/inner.toml").exists());
    let mut offered: Vec<&str> = value["data"]["offered"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    offered.sort_unstable();
    assert_eq!(offered, ["big", "binary", "good", "link", "vendor"]);

    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    let mut problems: Vec<(String, String)> = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| {
            (
                problem["file"].as_str().unwrap().to_owned(),
                problem["rule"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    problems.sort();
    let expected: Vec<(String, String)> = [
        ("servers/-dash.toml", "invalid-stem"),
        ("servers/.hidden.toml", "hidden-file"),
        ("servers/Bad__Stem.toml", "invalid-stem"),
        ("servers/big.toml", "file-too-large"),
        ("servers/binary.toml", "not-utf8"),
        ("servers/link.toml", "not-regular-file"),
        ("servers/nested", "subdirectory"),
        ("servers/notes.md", "non-toml-file"),
        ("servers/vendor.toml", "not-regular-file"),
    ]
    .iter()
    .map(|(file, rule)| ((*file).to_owned(), (*rule).to_owned()))
    .collect();
    assert_eq!(problems, expected);
}

#[cfg(unix)]
#[test]
fn canary_hooks_from_git_config_and_template_do_not_run() {
    use std::os::unix::fs::PermissionsExt;
    let (env, remote) = setup();
    let marker = env.root().join("hook-ran");
    let hook = format!("#!/bin/sh\necho \"$0\" >> '{}'\n", marker.display());
    let hooks = env.root().join("canary-hooks");
    let template = env.root().join("canary-template");
    for dir in [hooks.clone(), template.join("hooks")] {
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "reference-transaction",
            "post-checkout",
            "post-merge",
            "post-rewrite",
        ] {
            let path = dir.join(name);
            fs::write(&path, &hook).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    fs::write(
        env.git_config(),
        format!(
            "[core]\n\thooksPath = {}\n[init]\n\ttemplateDir = {}\n",
            hooks.display(),
            template.display()
        ),
    )
    .unwrap();
    // Positive control: with this configuration a plain git run fires them.
    let scratch = env.root().join("scratch");
    let status = remote
        .command(env.root())
        .env("GIT_CONFIG_GLOBAL", env.git_config())
        .args(["clone", "-q", &remote.url(), scratch.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    assert!(marker.exists(), "the canary hooks must work outside d-mcp");
    fs::remove_file(&marker).unwrap();

    let (code, _, err) = add_git(&env, &[&remote.url()]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !marker.exists(),
        "a hook ran: {}",
        fs::read_to_string(&marker).unwrap()
    );
    let hooks_in_clone = store(&env).join("sources/personal/repo/.git/hooks");
    assert!(fs::read_dir(&hooks_in_clone).map_or(true, |mut entries| entries.next().is_none()));
    // Neither does a refresh: a fetch into the clone, a re-clone, a dry run.
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    remote.commit("slack");
    for args in [
        &["source", "refresh", "personal"][..],
        &["--dry-run", "source", "refresh", "personal"],
    ] {
        let (code, _, err) = env.json(args);
        assert_eq!(code, 0, "{args:?}: {err}");
    }
    fs::remove_dir_all(store(&env).join("sources/personal/repo")).unwrap();
    let (code, value, err) = env.json(&["source", "refresh", "personal"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["recloned"], true);
    assert!(
        !marker.exists(),
        "a hook ran: {}",
        fs::read_to_string(&marker).unwrap()
    );
}

fn refresh(env: &TestEnv, args: &[&str]) -> (i32, Value, String) {
    let mut all = vec!["source", "refresh"];
    all.extend_from_slice(args);
    env.json(&all)
}

/// The store without the clone of `id` (a refused refresh may have fetched
/// into it).
fn store_without_clone(env: &TestEnv, id: &str) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let repo = store(env).join(format!("sources/{id}/repo"));
    snapshot(&store(env))
        .into_iter()
        .filter(|(path, _)| !path.starts_with(&repo))
        .collect()
}

/// `git <args>` in the store clone of `id`, with the builder's git.
fn in_clone(env: &TestEnv, remote: &Remote, id: &str, args: &[&str]) -> std::process::Output {
    remote
        .command(&store(env).join(format!("sources/{id}/repo")))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn refresh_refuses_a_rewrite_and_keeps_the_old_pin_reachable() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    let first = remote.head();
    remote.write("servers/github.toml", stdio("github", "rewritten"));
    remote.git(&["add", "-A"]);
    remote.git(&["commit", "-q", "--amend", "-m", "rewritten"]);
    remote.push();
    let rewritten = remote.head();
    let before = snapshot(&store(&env));
    let (code, value, _) = refresh(&env, &["personal", "--dry-run"]);
    assert_eq!((code, error_code(&value)), (1, "source-rewrite"));
    assert_eq!(snapshot(&store(&env)), before);
    let before = store_without_clone(&env, "personal");
    let (code, value, _) = refresh(&env, &["personal"]);
    assert_eq!((code, error_code(&value)), (1, "source-rewrite"));
    assert_eq!(store_without_clone(&env, "personal"), before);
    // The old pin survives a gc in the clone.
    let gc = in_clone(&env, &remote, "personal", &["gc", "-q", "--prune=now"]);
    assert!(gc.status.success());
    let pin = in_clone(&env, &remote, "personal", &["rev-parse", "refs/d-mcp/pin"]);
    assert_eq!(String::from_utf8(pin.stdout).unwrap().trim(), first);
    let exists = in_clone(
        &env,
        &remote,
        "personal",
        &["cat-file", "-e", &format!("{first}^{{commit}}")],
    );
    assert!(exists.status.success());

    let (code, value, err) = refresh(&env, &["personal", "--allow-rewrite"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["rewrite"], true);
    assert_eq!(value["data"]["previous_commit"], first.as_str());
    assert_eq!(value["data"]["commit"], rewritten.as_str());
    assert_eq!(value["data"]["changes"][0]["change"], "changed");
    let (_, text, _) = env.text(&["source", "list"]);
    assert!(text.contains("personal\t1\tgit\tavailable"), "{text}");
    let pin = in_clone(&env, &remote, "personal", &["rev-parse", "refs/d-mcp/pin"]);
    assert_eq!(String::from_utf8(pin.stdout).unwrap().trim(), rewritten);
}

#[test]
fn a_moved_tag_is_a_rewrite_and_a_deleted_ref_is_reported() {
    let (env, remote) = setup();
    remote.git(&["tag", "v1"]);
    remote.git(&["push", "-q", "origin", "v1"]);
    let (code, _, err) = add_git(&env, &[&remote.url(), "--id", "tagged", "--ref", "v1"]);
    assert_eq!(code, 0, "{err}");
    // The tag moves forward to a descendant: still a rewrite.
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let second = remote.commit("second");
    remote.git(&["tag", "-f", "v1"]);
    remote.git(&["push", "-q", "--force", "origin", "v1"]);
    for args in [&["tagged", "--dry-run"][..], &["tagged"]] {
        let (code, value, _) = refresh(&env, args);
        assert_eq!(
            (code, error_code(&value)),
            (1, "source-rewrite"),
            "{args:?}"
        );
    }
    let (code, value, err) = refresh(&env, &["tagged", "--allow-rewrite"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["commit"], second.as_str());
    assert_eq!(value["data"]["changes"][0]["server"], "slack");
    assert_eq!(value["data"]["changes"][0]["change"], "added");
    // An unchanged tag is no rewrite.
    let (code, _, _) = refresh(&env, &["tagged"]);
    assert_eq!(code, 0);

    // A deleted branch.
    remote.git(&["checkout", "-q", "-b", "feature"]);
    remote.write("servers/feature.toml", stdio("feature", "npx"));
    let feature = remote.commit("feature");
    remote.git(&["checkout", "-q", "main"]);
    let (code, _, err) = add_git(
        &env,
        &[&remote.url(), "--id", "feature", "--ref", "feature"],
    );
    assert_eq!(code, 0, "{err}");
    remote.bare_git(&["branch", "-q", "-D", "feature"]);
    let before = snapshot(&store(&env));
    for args in [&["feature", "--dry-run"][..], &["feature"]] {
        let (code, value, _) = refresh(&env, args);
        assert_eq!(
            (code, error_code(&value)),
            (1, "source-ref-deleted"),
            "{args:?}"
        );
    }
    assert_eq!(store_without_clone(&env, "feature"), {
        let repo = store(&env).join("sources/feature/repo");
        before
            .into_iter()
            .filter(|(path, _)| !path.starts_with(&repo))
            .collect::<Vec<_>>()
    });
    let lock = fs::read_to_string(store(&env).join("lock.toml")).unwrap();
    assert!(lock.contains(&feature), "{lock}");
}

#[test]
fn a_broken_clone_is_recloned_and_an_unreachable_pin_fails() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let second = remote.commit("second");
    fs::remove_dir_all(store(&env).join("sources/personal/repo")).unwrap();
    let (code, value, err) = refresh(&env, &["personal"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["recloned"], true);
    assert_eq!(value["data"]["commit"], second.as_str());
    assert!(store(&env).join("sources/personal/repo/.git").is_dir());
    let (_, value, _) = env.json(&["source", "list"]);
    assert_eq!(value["data"]["sources"][0]["available"], true);
    assert_eq!(sources_entries(&env), ["personal"]);

    // The pin is rewritten away upstream and pruned; the clone breaks.
    remote.write("servers/slack.toml", stdio("slack", "other"));
    remote.git(&["add", "-A"]);
    remote.git(&["commit", "-q", "--amend", "-m", "rewritten"]);
    remote.push();
    remote.bare_git(&["reflog", "expire", "--expire=now", "--all"]);
    remote.bare_git(&["gc", "-q", "--prune=now"]);
    fs::write(
        store(&env).join("sources/personal/repo/.git/HEAD"),
        "garbage",
    )
    .unwrap();
    let before = store_without_clone(&env, "personal");
    let (code, value, _) = refresh(&env, &["personal", "--allow-rewrite"]);
    assert_eq!((code, error_code(&value)), (1, "source-commit-unreachable"));
    assert_eq!(store_without_clone(&env, "personal"), before);
    assert_eq!(sources_entries(&env), ["personal"]);
}

#[cfg(unix)]
#[test]
fn a_refresh_racing_a_change_fails_with_source_changed() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    let first = remote.head();
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    remote.commit("second");
    // Another process re-pins the source while this one fetches.
    let lock = store(&env).join("lock.toml");
    replace_git(
        &env,
        &format!(
            "#!/bin/sh
for arg in \"$@\"; do\n  if [ \"$arg\" = fetch ]; then\n    '{git}' \"$@\"; rc=$?\n    printf 'schema_version = 1\\n' > '{lock}'\n    exit $rc\n  fi\ndone\nexec '{git}' \"$@\"\n",
            git = system_git().display(),
            lock = lock.display()
        ),
    );
    let (code, value, _) = refresh(&env, &["personal"]);
    assert_eq!((code, error_code(&value)), (3, "source-changed"), "{value}");
    assert_eq!(
        fs::read_to_string(store(&env).join("sources/personal/snapshot/commit")).unwrap(),
        format!("{first}\n")
    );
    assert!(
        !store(&env)
            .join("sources/personal/snapshot/servers/slack.toml")
            .exists()
    );
    let leftovers: Vec<String> = sources_entries(&env)
        .into_iter()
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    // Without git, a refresh cannot run.
    fs::remove_file(env.git_bin().join("git")).unwrap();
    let (code, value, _) = refresh(&env, &["personal"]);
    assert_eq!((code, error_code(&value)), (4, "git-not-found"));
    let (code, value, _) = refresh(&env, &["nope"]);
    assert_eq!((code, error_code(&value)), (1, "source-not-found"));
}

#[cfg(unix)]
#[test]
fn a_refresh_racing_a_source_removal_fails_with_source_changed() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    remote.commit("second");
    // Another process removes the source (config entry and pin) while this
    // one fetches.
    let store = store(&env);
    let config = store.join("config.toml");
    let lock = store.join("lock.toml");
    replace_git(
        &env,
        &format!(
            "#!/bin/sh
for arg in \"$@\"; do\n  if [ \"$arg\" = fetch ]; then\n    '{git}' \"$@\"; rc=$?\n    printf 'schema_version = 1\\n' > '{config}'\n    printf 'schema_version = 1\\n' > '{lock}'\n    exit $rc\n  fi\ndone\nexec '{git}' \"$@\"\n",
            git = system_git().display(),
            config = config.display(),
            lock = lock.display()
        ),
    );
    let (code, value, _) = refresh(&env, &["personal"]);
    assert_eq!((code, error_code(&value)), (3, "source-changed"), "{value}");
    // The refresh wrote nothing: no new snapshot content, no pin, no temp.
    assert!(
        !store
            .join("sources/personal/snapshot/servers/slack.toml")
            .exists()
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), "schema_version = 1\n");
    assert!(
        sources_entries(&env)
            .iter()
            .all(|name| !name.starts_with(".tmp-"))
    );
}
