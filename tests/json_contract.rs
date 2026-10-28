//! JSON-contract tests: the `--json` output of every command matches the
//! documents in `docs/reference.md`.
//!
//! The documented examples are parsed from the reference and compared with
//! real output by shape: every object has exactly the documented keys, and
//! arrays are compared element-wise against the first documented element.
//! `null` on either side matches any value, because optional fields vary,
//! and an empty documented object is a placeholder.

mod common;

use std::collections::BTreeSet;
use std::fs;

use common::{TestEnv, stdio};
use serde_json::Value;

const REFERENCE: &str = include_str!("../docs/reference.md");

/// Returns the first ```json block after the given heading line.
fn documented(heading: &str) -> Value {
    let after = REFERENCE
        .split_once(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("heading `{heading}` not found in docs/reference.md"))
        .1;
    let block = after
        .split_once("```json\n")
        .and_then(|(_, rest)| rest.split_once("\n```"))
        .unwrap_or_else(|| panic!("no json block after `{heading}`"))
        .0;
    serde_json::from_str(block)
        .unwrap_or_else(|error| panic!("invalid json after `{heading}`: {error}"))
}

fn assert_shape(documented: &Value, actual: &Value, path: &str) {
    match (documented, actual) {
        (Value::Null, _) | (_, Value::Null) => {}
        // An empty documented object (the envelope's `data`) is a placeholder.
        (Value::Object(doc), Value::Object(_)) if doc.is_empty() => {}
        (Value::Object(doc), Value::Object(act)) => {
            let doc_keys: BTreeSet<&String> = doc.keys().collect();
            let act_keys: BTreeSet<&String> = act.keys().collect();
            assert_eq!(doc_keys, act_keys, "keys differ at `{path}`");
            for (key, value) in doc {
                assert_shape(value, &act[key], &format!("{path}.{key}"));
            }
        }
        (Value::Array(doc), Value::Array(act)) => {
            if let Some(first) = doc.first() {
                for (index, item) in act.iter().enumerate() {
                    assert_shape(first, item, &format!("{path}[{index}]"));
                }
            }
        }
        (Value::String(_), Value::String(_))
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_)) => {}
        _ => panic!("type differs at `{path}`: documented {documented}, actual {actual}"),
    }
}

/// Checks the envelope and the `data` shape of a successful run.
fn check_success(value: &Value, heading: &str) {
    assert_shape(&documented("### Envelope"), value, "envelope");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["error"], Value::Null);
    assert_shape(&documented(heading), &value["data"], heading);
    if let Some(problems) = value["problems"].as_array() {
        let problem = documented("### Problems");
        for item in problems {
            assert_shape(&problem, item, "problem");
            let object = item.as_object().unwrap();
            assert_eq!(object.len(), problem.as_object().unwrap().len());
        }
    }
}

#[test]
fn every_command_matches_the_reference() {
    let env = TestEnv::new();
    let (code, value, _) = env.json(&["--dry-run", "init"]);
    assert_eq!(code, 0);
    check_success(&value, "### `init`");
    let (code, value, _) = env.json(&["init"]);
    assert_eq!(code, 0);
    check_success(&value, "### `init`");

    let team = env.source_dir("team");
    fs::write(
        team.join("servers/github.toml"),
        include_str!("fixtures/definitions/valid/github.toml"),
    )
    .unwrap();
    fs::write(team.join("servers/broken.toml"), "name = ").unwrap();
    env.write_local("github", &stdio("github", "npx"));
    env.write_local(
        "linear",
        include_str!("fixtures/definitions/valid/linear.toml"),
    );
    fs::write(env.default_store().join("local/servers/notes.txt"), "x").unwrap();

    let (code, value, _) = env.json(&[
        "source",
        "add",
        team.to_str().unwrap(),
        "--id",
        "team",
        "--priority",
        "10",
    ]);
    assert_eq!(code, 0);
    check_success(&value, "### `source add`");
    assert_shape(
        &documented("### Source object"),
        &value["data"]["source"],
        "source",
    );

    let (code, value, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);
    check_success(&value, "### `source list`");

    for args in [
        vec!["select", "github"],
        vec!["select", "linear", "--harness", "claude"],
        vec!["select", "broken"],
        vec!["select", "ghost"],
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 0);
        check_success(&value, "### `select`");
    }

    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    check_success(&value, "### `resolve`");
    let problem_codes: BTreeSet<&str> = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap())
        .collect();
    let documented_codes: BTreeSet<&str> = [
        "invalid-definition",
        "invalid-winner",
        "not-offered",
        "source-unavailable",
        "shadowed",
        "notice",
    ]
    .into();
    assert!(problem_codes.is_subset(&documented_codes));
    for code in &documented_codes {
        assert!(
            REFERENCE.contains(&format!("| `{code}` |")),
            "problem code {code} is not documented"
        );
    }
    let linear = value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == "linear")
        .unwrap();
    assert_eq!(linear["auth"], "oauth");
    assert_eq!(
        linear["forwarded_env"],
        serde_json::json!(["LINEAR_CLIENT_SECRET"])
    );
    // The limited view never shows literals, args or override contents.
    let text = value.to_string();
    for hidden in [
        "info",
        "server-github",
        "/srv/github",
        "d-mcp-linear",
        "read\"",
    ] {
        assert!(!text.contains(hidden), "resolve output shows `{hidden}`");
    }

    let (code, value, _) = env.json(&["unselect", "ghost"]);
    assert_eq!(code, 0);
    check_success(&value, "### `unselect`");

    let (code, value, _) = env.json(&["source", "remove", "team"]);
    assert_eq!(code, 0);
    check_success(&value, "### `source remove`");
}

#[test]
fn failures_use_the_error_envelope() {
    let env = TestEnv::new();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 1);
    assert_shape(&documented("### Envelope"), &value, "envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["data"], Value::Null);
    assert_eq!(value["problems"], serde_json::json!([]));
    let error = value["error"].as_object().unwrap();
    assert_eq!(
        error.keys().collect::<Vec<_>>(),
        ["code", "hint"],
        "error object keys"
    );
    assert_eq!(value["error"]["code"], "store-not-initialized");
    assert!(
        REFERENCE.contains("| `store-not-initialized` | 1 |"),
        "error code documented with its exit code"
    );
}

#[test]
fn dry_run_documents_match_the_reference() {
    let env = TestEnv::new();
    let team = env.source_dir("team");
    env.write_local("github", &stdio("github", "npx"));
    let (code, value, _) = env.json(&["--dry-run", "init"]);
    assert_eq!(code, 0);
    check_success(&value, "### `init`");
    env.init();
    let team_arg = team.to_str().unwrap();
    let steps: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["source", "add", team_arg, "--id", "team", "--priority", "4"],
            "### `source add`",
        ),
        (vec!["select", "github"], "### `select`"),
    ];
    for (args, heading) in &steps {
        let mut dry = vec!["--dry-run"];
        dry.extend(args.iter().copied());
        let (code, value, _) = env.json(&dry);
        assert_eq!(code, 0, "{dry:?}");
        assert_eq!(value["data"]["dry_run"], true);
        check_success(&value, heading);
        let (code, value, _) = env.json(args);
        assert_eq!(code, 0, "{args:?}");
        assert_eq!(value["data"]["dry_run"], false);
        check_success(&value, heading);
    }
    for (args, heading) in [
        (vec!["--dry-run", "unselect", "github"], "### `unselect`"),
        (
            vec!["--dry-run", "source", "remove", "team"],
            "### `source remove`",
        ),
        (vec!["--dry-run", "resolve"], "### `resolve`"),
        (vec!["--dry-run", "source", "list"], "### `source list`"),
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 0, "{args:?}");
        check_success(&value, heading);
    }
}

#[test]
fn source_unavailable_problem_and_hold_match_the_reference() {
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
        "2",
    ]);
    env.json(&["select", "github"]);
    fs::remove_dir_all(team.join("servers")).unwrap();
    let (code, value, _) = env.json(&["resolve"]);
    assert_eq!(code, 0);
    check_success(&value, "### `resolve`");
    let codes: Vec<&str> = value["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|problem| problem["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"source-unavailable"), "{codes:?}");
    let (code, value, _) = env.json(&["source", "list"]);
    assert_eq!(code, 0);
    check_success(&value, "### `source list`");
}

#[test]
fn every_exit_class_uses_the_documented_error_envelope() {
    let env = TestEnv::new();
    env.init();
    let config = env.default_store().join("config.toml");
    fs::write(&config, "schema_version = 9\n").unwrap();
    let mut seen = Vec::new();
    let (code, value, _) = env.json(&["resolve"]);
    seen.push((code, value));
    fs::write(&config, "[[").unwrap();
    let (code, value, _) = env.json(&["select", "x"]);
    seen.push((code, value));
    let output = env
        .cmd()
        .env_remove("HOME")
        .args(["--json", "resolve"])
        .output()
        .unwrap();
    seen.push((
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stdout).unwrap(),
    ));
    let (code, value, _) = env.json(&["unselect", "x"]);
    seen.push((code, value));
    let expected = [
        (3, "schema-unsupported"),
        (3, "config-corrupt"),
        (4, "home-unset"),
        (3, "config-corrupt"),
    ];
    for ((code, value), (exit, error)) in seen.iter().zip(expected) {
        assert_eq!(*code, exit);
        assert_shape(&documented("### Envelope"), value, "envelope");
        assert_eq!(value["ok"], false);
        assert_eq!(value["data"], Value::Null);
        assert_eq!(value["error"]["code"], error);
        assert!(
            REFERENCE.contains(&format!("| `{error}` | {exit} |")),
            "{error} documented with exit {exit}"
        );
    }
}

/// Checks the envelope of a `sync` run that exits non-zero: `data` is still
/// the documented document, with `ok: false` and the aggregate error.
fn check_sync_failure(value: &Value, exit: i32, code: i32, error: &str) {
    assert_eq!(code, exit);
    assert_shape(&documented("### Envelope"), value, "envelope");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], error);
    assert_shape(&documented("### `sync`"), &value["data"], "sync");
    assert!(
        REFERENCE.contains(&format!("| `{error}` | {exit} |")),
        "{error} documented with exit {exit}"
    );
}

#[test]
fn sync_and_unmanage_documents_match_the_reference() {
    let env = TestEnv::new().without_codex();
    env.init();
    // Codex not present.
    env.write_local("github", &stdio("github", "npx"));
    env.json(&["select", "github"]);
    let (code, value, _) = env.json(&["sync"]);
    assert_eq!(code, 0);
    check_success(&value, "### `sync`");
    assert_eq!(value["data"]["harnesses"][0]["status"], "not-present");

    fs::create_dir_all(env.codex_home()).unwrap();
    fs::write(env.codex_config(), "").unwrap();
    env.write_local(
        "linear",
        include_str!("fixtures/definitions/valid/linear.toml"),
    );
    env.json(&["select", "linear", "--harness", "codex"]);
    for args in [vec!["--dry-run", "sync"], vec!["sync"], vec!["sync"]] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 0, "{args:?}");
        check_success(&value, "### `sync`");
        let gaps = &value["data"]["harnesses"][0]["entries"][1]["gaps"];
        assert_eq!(gaps[0]["field"], "auth.client_secret_env");
    }

    // Exit 1: drift, with data.
    let text = fs::read_to_string(env.codex_config()).unwrap();
    fs::write(
        env.codex_config(),
        text.replace("command = \"npx\"", "command = \"npx\"\nargs = [\"x\"]"),
    )
    .unwrap();
    let (code, value, _) = env.json(&["sync"]);
    check_sync_failure(&value, 1, code, "sync-blocked");
    assert_eq!(
        value["data"]["harnesses"][0]["entries"][0]["operation"],
        "drift"
    );

    // Exit 3: an invalid harness file, with data.
    fs::write(env.codex_config(), "[[").unwrap();
    let (code, value, _) = env.json(&["sync"]);
    check_sync_failure(&value, 3, code, "harness-config-invalid");
    assert_eq!(value["data"]["harnesses"][0]["status"], "failed");
    assert_eq!(
        value["data"]["harnesses"][0]["error"]["code"],
        "harness-config-invalid"
    );
    fs::write(env.codex_config(), text).unwrap();

    for args in [
        vec!["--dry-run", "unmanage", "github"],
        vec!["unmanage", "github"],
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 0, "{args:?}");
        check_success(&value, "### `unmanage`");
    }
    let (code, value, _) = env.json(&["unmanage", "linear"]);
    assert_eq!(code, 0);
    check_success(&value, "### `unmanage`");
    assert_eq!(value["data"]["selection"], Value::Null);
    assert_eq!(value["data"]["selection_removed"], true);

    // Global errors carry no data.
    let (code, value, _) = env.json(&["sync", "--take-over", "nobody"]);
    assert_eq!(code, 1);
    assert_eq!(value["data"], Value::Null);
    assert_eq!(value["error"]["code"], "unknown-take-over");
    let (code, value, _) = env.json(&["unmanage", "github"]);
    assert_eq!(code, 1);
    assert_eq!(value["data"], Value::Null);
    assert_eq!(value["error"]["code"], "not-managed");
}

#[test]
fn every_error_variant_maps_to_its_documented_code() {
    for error in d_mcp::error::Error::samples() {
        assert!(
            REFERENCE.contains(&format!("| `{}` | {} |", error.code(), error.exit_code())),
            "`{}` with exit {} is not documented",
            error.code(),
            error.exit_code()
        );
    }
}

#[test]
fn claude_sync_and_unmanage_documents_match_the_reference() {
    let env = TestEnv::new();
    let bin = env.fake_claude_dir();
    let env = env.with_path(&[bin]);
    env.init();
    env.write_local("github", &stdio("github", "/usr/bin/github"));
    env.write_local(
        "linear",
        include_str!("fixtures/definitions/valid/linear.toml"),
    );
    env.json(&["select", "github", "--harness", "claude"]);
    env.json(&["select", "linear", "--harness", "claude"]);
    for args in [
        vec!["--dry-run", "sync", "--harness", "claude"],
        vec!["sync", "--harness", "claude"],
        vec!["sync", "--harness", "claude"],
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 0, "{args:?}: {value:#}");
        check_success(&value, "### `sync`");
        let harnesses = value["data"]["harnesses"].as_array().unwrap();
        assert_eq!(harnesses.len(), 1);
        assert_eq!(harnesses[0]["harness"], "claude");
    }

    // Exit 1: a failed CLI call, with the per-entry error.
    env.write_local("github", &stdio("github", "/usr/bin/github-v2"));
    env.claude_mode(&["fail add-json github"]);
    let (code, value, _) = env.json(&["sync", "--harness", "claude"]);
    check_sync_failure(&value, 1, code, "harness-cli-failed");
    let entry = &value["data"]["harnesses"][0]["entries"][0];
    assert_eq!(entry["outcome"], "lost");
    assert_eq!(entry["error"]["code"], "harness-cli-failed");
    assert!(entry["error"]["hint"].is_string());

    // Exit 3: the entry does not read back as written.
    env.claude_mode(&["alter add-json github"]);
    let (code, value, _) = env.json(&["sync", "--harness", "claude"]);
    check_sync_failure(&value, 3, code, "harness-verify-failed");
    env.claude_mode(&[]);

    // Exit 4: the CLI is missing while there are operations.
    let without = env.root().join("empty-bin");
    let output = env
        .cmd()
        .env("PATH", &without)
        .args([
            "--json",
            "unmanage",
            "github",
            "--harness",
            "claude",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    env.write_local("tools", &stdio("tools", "/usr/bin/tools"));
    env.json(&["select", "tools", "--harness", "claude"]);
    let output = env
        .cmd()
        .env("PATH", &without)
        .args(["--json", "sync", "--harness", "claude"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    check_sync_failure(
        &value,
        4,
        output.status.code().unwrap(),
        "harness-cli-not-found",
    );

    for args in [
        vec!["--dry-run", "unmanage", "linear", "--harness", "claude"],
        vec!["unmanage", "linear", "--harness", "claude"],
    ] {
        let (code, value, _) = env.json(&args);
        assert_eq!(code, 0, "{args:?}");
        check_success(&value, "### `unmanage`");
        assert_eq!(value["data"]["harnesses"], serde_json::json!(["claude"]));
    }
}
