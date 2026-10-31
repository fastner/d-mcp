//! Git source hardening: the SSH batch mode after an `insteadOf` rewrite,
//! fully qualified followed refs, a missing pin, a concurrent sweep during
//! a refresh and the entry limit of a snapshot. Every remote is a local bare
//! repository and the only SSH is a fake; no test uses the network.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Remote, TestEnv, snapshot, stdio};
use serde_json::Value;

/// A store with one remote offering `github` on `main`.
fn setup() -> (TestEnv, Remote) {
    let env = TestEnv::new().with_git();
    env.init();
    let remote = env.remote("personal");
    remote.write("servers/github.toml", stdio("github", "npx"));
    remote.commit("first");
    (env, remote)
}

fn store(env: &TestEnv) -> PathBuf {
    env.default_store()
}

fn error_code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or_default()
}

fn run(env: &TestEnv, base: &[&str], args: &[&str]) -> (i32, Value, String) {
    let mut all = base.to_vec();
    all.extend_from_slice(args);
    env.json(&all)
}

fn add_git(env: &TestEnv, args: &[&str]) -> (i32, Value, String) {
    run(env, &["source", "add-git"], args)
}

fn refresh(env: &TestEnv, args: &[&str]) -> (i32, Value, String) {
    run(env, &["source", "refresh"], args)
}

fn lock_text(env: &TestEnv) -> String {
    fs::read_to_string(store(env).join("lock.toml")).unwrap()
}

/// The `[[pins]]` entry of `source` as written: source, commit and ref.
fn pin_entry(source: &str, commit: &str, reference: &str) -> String {
    format!("source = \"{source}\"\ncommit = \"{commit}\"\nref = \"{reference}\"\n")
}

/// The entries of `<store>/sources`, sorted.
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

/// Writes an executable shell script.
#[cfg(unix)]
fn script(path: &std::path::Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake `ssh` that records its arguments in `<root>/ssh-args` and fails;
/// `PATH` holds `git` and it.
#[cfg(unix)]
fn with_fake_ssh() -> (TestEnv, PathBuf) {
    let env = TestEnv::new().with_git();
    let bin = env.root().join("ssh-bin");
    fs::create_dir_all(&bin).unwrap();
    let marker = env.root().join("ssh-args");
    script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 255\n",
            marker.display()
        ),
    );
    let git_bin = env.git_bin();
    let env = env.with_path(&[git_bin, bin]);
    env.init();
    (env, marker)
}

/// An `https` URL that the user's configuration rewrites to SSH still runs
/// SSH in batch mode; a user-configured `core.sshCommand` is honoured.
#[cfg(unix)]
#[test]
fn an_insteadof_rewrite_to_ssh_still_gets_batch_mode() {
    let (env, marker) = with_fake_ssh();
    fs::write(
        env.git_config(),
        "[url \"ssh://git@fake.invalid/\"]\n\tinsteadOf = https://example.invalid/\n",
    )
    .unwrap();
    let before = snapshot(&store(&env));
    let (code, value, _) = add_git(&env, &["https://example.invalid/me/defs.git"]);
    assert_eq!(code, 4, "{value}");
    assert!(error_code(&value).starts_with("git-"), "{value}");
    let args = fs::read_to_string(&marker).expect("the fake ssh ran");
    let args: Vec<&str> = args.lines().collect();
    assert!(args.contains(&"-oBatchMode=yes"), "{args:?}");
    assert!(args.contains(&"-oConnectTimeout=15"), "{args:?}");
    assert!(
        args.iter().any(|arg| arg.contains("fake.invalid")),
        "{args:?}"
    );
    assert!(
        sources_entries(&env)
            .iter()
            .all(|name| !name.starts_with(".tmp-"))
    );
    fs::remove_dir(store(&env).join("sources")).unwrap();
    assert_eq!(snapshot(&store(&env)), before);

    fs::remove_file(&marker).unwrap();
    fs::write(
        env.git_config(),
        "[url \"ssh://git@fake.invalid/\"]\n\tinsteadOf = https://example.invalid/\n\
         [core]\n\tsshCommand = ssh -oUserConfigured=yes\n",
    )
    .unwrap();
    let (code, value, _) = add_git(&env, &["https://example.invalid/me/defs.git"]);
    assert_eq!(code, 4, "{value}");
    let args = fs::read_to_string(&marker).expect("the fake ssh ran");
    assert!(
        args.lines().any(|arg| arg == "-oUserConfigured=yes"),
        "{args}"
    );
    assert!(!args.contains("BatchMode"), "{args}");
}

/// `lock.toml` records the followed ref fully qualified, and a refresh
/// fetches exactly that ref: a tag pushed with the name of the followed
/// branch is not taken instead (git prefers a tag for an unqualified name),
/// and a branch pushed with the name of a followed tag is not either.
#[test]
fn the_followed_ref_is_fully_qualified() {
    let (env, remote) = setup();
    let first = remote.head();
    let (code, _, err) = add_git(&env, &[&remote.url(), "--id", "personal"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        lock_text(&env).contains(&pin_entry("personal", &first, "refs/heads/main")),
        "{}",
        lock_text(&env)
    );
    remote.bare_git(&["tag", "v1", &first]);
    let (code, _, err) = add_git(&env, &[&remote.url(), "--id", "tagged", "--ref", "v1"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        lock_text(&env).contains(&pin_entry("tagged", &first, "refs/tags/v1")),
        "{}",
        lock_text(&env)
    );

    // A diverging commit gets a tag named `main`; the branch moves on.
    remote.git(&["checkout", "-q", "-b", "side"]);
    remote.write("servers/side.toml", stdio("side", "npx"));
    let side = remote.commit_local("side");
    remote.git(&["push", "-q", "origin", "side"]);
    remote.git(&["checkout", "-q", "main"]);
    remote.bare_git(&["tag", "main", &side]);
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let next = remote.commit("next");
    // And a branch named `v1` appears ahead of the tag.
    remote.bare_git(&["branch", "v1", &next]);

    let (code, value, err) = refresh(&env, &["personal"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["commit"], next.as_str());
    assert_eq!(value["data"]["rewrite"], false);
    let (code, value, err) = refresh(&env, &["tagged"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["commit"], first.as_str());
    assert_eq!(value["data"]["rewrite"], false);

    // With both a branch and a tag of the name, `add-git` follows the
    // branch, like `git clone --branch`.
    let (code, value, err) = add_git(&env, &[&remote.url(), "--id", "both", "--ref", "main"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(value["data"]["source"]["commit"], next.as_str());
    assert_eq!(value["data"]["source"]["ref"], "main");
    assert!(
        lock_text(&env).contains(&pin_entry("both", &next, "refs/heads/main")),
        "{}",
        lock_text(&env)
    );
}

/// A pin written without the qualified ref (an older `lock.toml`) is
/// qualified with `ls-remote` by the next refresh, a branch winning over a
/// tag of the same name, and written with it; other pins stay as they are.
#[test]
fn a_pin_without_a_recorded_ref_is_qualified_by_the_next_refresh() {
    let (env, remote) = setup();
    let first = remote.head();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    add_git(&env, &[&remote.url(), "--id", "other"]);
    remote.bare_git(&["tag", "main", &first]);
    let legacy: String = lock_text(&env)
        .lines()
        .filter(|line| !line.starts_with("ref = "))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(store(&env).join("lock.toml"), &legacy).unwrap();
    // A read-only command accepts the older file.
    let (code, _, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);

    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let next = remote.commit("next");
    let (code, value, err) = refresh(&env, &["personal", "--dry-run"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(lock_text(&env), legacy, "a dry run writes nothing");
    let (code, value, err) = refresh(&env, &["personal"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["commit"], next.as_str());
    let lock = lock_text(&env);
    assert!(
        lock.contains(&pin_entry("personal", &next, "refs/heads/main")),
        "{lock}"
    );
    assert!(
        lock.contains(&format!("source = \"other\"\ncommit = \"{first}\"\n\n"))
            || lock.ends_with(&format!("source = \"other\"\ncommit = \"{first}\"\n")),
        "{lock}"
    );

    // An unchanged pin is qualified too (only `lock.toml` is written).
    let strip = |text: &str| -> String {
        text.lines()
            .filter(|line| !line.starts_with("ref = "))
            .map(|line| format!("{line}\n"))
            .collect()
    };
    fs::write(store(&env).join("lock.toml"), strip(&lock_text(&env))).unwrap();
    let snapshot_before =
        fs::read_to_string(store(&env).join("sources/personal/snapshot/commit")).unwrap();
    let (code, value, err) = refresh(&env, &["personal"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["commit"], next.as_str());
    assert!(
        lock_text(&env).contains(&pin_entry("personal", &next, "refs/heads/main")),
        "{}",
        lock_text(&env)
    );
    assert_eq!(
        fs::read_to_string(store(&env).join("sources/personal/snapshot/commit")).unwrap(),
        snapshot_before
    );

    // A ref the remote has neither as a branch nor as a tag is deleted.
    remote.bare_git(&["update-ref", "-d", "refs/tags/main"]);
    remote.bare_git(&["branch", "-m", "main", "trunk"]);
    let before = lock_text(&env);
    let (code, value, _) = refresh(&env, &["other"]);
    assert_eq!(
        (code, error_code(&value)),
        (1, "source-ref-deleted"),
        "{value}"
    );
    assert_eq!(lock_text(&env), before);
}

/// A registered source without a pin (a hand-edited `lock.toml`) has
/// nothing to check the new commit against: the refresh counts as a
/// rewrite and needs `--allow-rewrite`.
#[test]
fn a_missing_pin_counts_as_a_rewrite() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let next = remote.commit("next");
    fs::write(store(&env).join("lock.toml"), "schema_version = 1\n").unwrap();
    let before = snapshot(&store(&env));
    for args in [&["personal", "--dry-run"][..], &["personal"]] {
        let (code, value, _) = refresh(&env, args);
        assert_eq!(
            (code, error_code(&value)),
            (1, "source-rewrite"),
            "{args:?}"
        );
    }
    let after: Vec<_> = snapshot(&store(&env))
        .into_iter()
        .filter(|(path, _)| !path.starts_with(store(&env).join("sources/personal/repo")))
        .collect();
    let before: Vec<_> = before
        .into_iter()
        .filter(|(path, _)| !path.starts_with(store(&env).join("sources/personal/repo")))
        .collect();
    assert_eq!(after, before);
    let (code, value, err) = refresh(&env, &["personal", "--allow-rewrite"]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["rewrite"], true);
    assert_eq!(value["data"]["previous_commit"], Value::Null);
    assert_eq!(value["data"]["commit"], next.as_str());
    assert!(
        lock_text(&env).contains(&pin_entry("personal", &next, "refs/heads/main")),
        "{}",
        lock_text(&env)
    );
}

/// A refresh that replaces a broken clone keeps its fresh clone in
/// `sources/.tmp-*` outside the store lock; an `add-git` that runs meanwhile
/// sweeps stale temporary directories but skips this live one.
#[cfg(unix)]
#[test]
fn a_concurrent_sweep_skips_the_directory_of_a_live_refresh() {
    let (env, remote) = setup();
    add_git(&env, &[&remote.url(), "--id", "personal"]);
    let other = env.remote("other");
    other.write("servers/wiki.toml", stdio("wiki", "npx"));
    other.commit("first");
    remote.write("servers/slack.toml", stdio("slack", "npx"));
    let next = remote.commit("next");
    // The clone breaks, so the refresh clones afresh into `sources/.tmp-*`.
    fs::remove_dir_all(store(&env).join("sources/personal/repo")).unwrap();
    // A stale directory of a killed run.
    fs::create_dir_all(store(&env).join("sources/.tmp-killed/repo")).unwrap();
    let nested = env.root().join("nested-failed");
    let git = env.git_bin().join("git");
    fs::remove_file(&git).unwrap();
    script(
        &git,
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = fetch ]; then\n    '{git}' \"$@\"; rc=$?\n    (unset GIT_DIR; '{d_mcp}' source add-git '{url}' --id other >/dev/null 2>&1) || : > '{nested}'\n    exit $rc\n  fi\ndone\nexec '{git}' \"$@\"\n",
            git = common::system_git().display(),
            d_mcp = env!("CARGO_BIN_EXE_d-mcp"),
            url = other.url(),
            nested = nested.display(),
        ),
    );
    let (code, value, err) = refresh(&env, &["personal"]);
    assert!(!nested.exists(), "the concurrent add-git failed");
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["data"]["recloned"], true);
    assert_eq!(value["data"]["commit"], next.as_str());
    assert_eq!(sources_entries(&env), ["other", "personal"]);
    assert!(
        store(&env)
            .join("sources/personal/snapshot/servers/slack.toml")
            .is_file()
    );
    let (_, value, _) = env.json(&["source", "list"]);
    for source in value["data"]["sources"].as_array().unwrap() {
        assert_eq!(source["available"], true, "{source}");
    }
}

/// `servers/` with more entries than the limit fails before anything is
/// written, so a snapshot never starts more than 1000 `cat-file` calls.
#[test]
fn a_servers_tree_over_the_entry_limit_is_refused() {
    let env = TestEnv::new().with_git();
    env.init();
    let remote = env.remote("crowded");
    for index in 0..=1000 {
        remote.write(&format!("servers/s{index}.toml"), stdio("x", "npx"));
    }
    remote.commit("crowded");
    let before = snapshot(&store(&env));
    let (code, value, _) = add_git(&env, &[&remote.url()]);
    assert_eq!((code, error_code(&value)), (4, "git-failed"), "{value}");
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("more than 1000 entries"),
        "{value}"
    );
    assert!(sources_entries(&env).is_empty());
    fs::remove_dir(store(&env).join("sources")).unwrap();
    assert_eq!(snapshot(&store(&env)), before);
    // At the limit, the source is added.
    remote.remove("servers/s1000.toml");
    remote.commit("fewer");
    let (code, _, err) = add_git(&env, &[&remote.url()]);
    assert_eq!(code, 0, "{err}");
}
