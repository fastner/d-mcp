//! The redaction canary suite: every command, in text and JSON form, run on
//! the `real-setup` fixture, whose harness files, repository files and team
//! definitions carry canaries in every position d-mcp must never show (env
//! values, headers, `args`, `command`, `cwd`, URL user info, path, query and
//! fragment, a definition `description`, an OAuth client id and secret,
//! account data, local-scope values, and broken TOML lines), and with fake
//! harness CLIs that print canaries on stdout and echo their input on
//! stderr. No canary may reach stdout, stderr, any store file or any file
//! under `local/`.
//!
//! The existing commands are covered as well as `inventory`, `adopt`
//! (previews full of refusals, an apply whose replaced literals are
//! canaries, a variable gate that compares canaries, and a changed entry),
//! `status` and `doctor` (also with `--auth`, with missing variables, with
//! fake CLIs whose `--version` prints a canary, and on broken harness files)
//! and `uninstall` (preview, `--apply` releasing adopted entries whose
//! literals were canaries, and `--purge`).
//!
//! The two forms run in twin environments installed from the same fixture
//! (the text twin shares the JSON twin's `.hash-key`, so display hashes
//! match): each command runs once per twin, so the text form of a command
//! that writes (`adopt --apply`, `sync --take-over`, `uninstall --apply`)
//! succeeds as well, instead of meeting the state the JSON run left.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use common::{CANARY, RealSetup, TestEnv, snapshot};
use serde_json::Value;

/// The placeholder for the team source's path in command arguments.
const TEAM: &str = "@TEAM@";

/// Runs every command in both output forms, each in its own twin, and
/// collects their streams.
struct Run {
    text: (TestEnv, RealSetup),
    json: (TestEnv, RealSetup),
    seen: Vec<(String, String)>,
}

impl Run {
    /// Twin environments from the `real-setup` fixture, with both fakes on
    /// `PATH` and one `.hash-key`.
    fn new() -> Self {
        let twin = || {
            let env = with_fakes();
            let setup = env.install_real_setup();
            (env, setup)
        };
        let (text, json) = (twin(), twin());
        fs::copy(
            json.0.default_store().join(".hash-key"),
            text.0.default_store().join(".hash-key"),
        )
        .expect("share the hash key");
        Self {
            text,
            json,
            seen: Vec::new(),
        }
    }

    /// Both twins, text first.
    fn twins(&self) -> [&(TestEnv, RealSetup); 2] {
        [&self.text, &self.json]
    }

    /// Runs `args` in both forms; both must exit with `expected`. Returns
    /// the JSON document.
    fn both(&mut self, expected: i32, args: &[&str]) -> Value {
        self.both_with(expected, &[], args)
    }

    /// Runs `args` in text form in the text twin, then in JSON form in the
    /// JSON twin, with extra environment variables; both must exit with
    /// `expected`. Returns the JSON document.
    fn both_with(&mut self, expected: i32, vars: &[(&str, &str)], args: &[&str]) -> Value {
        let mut document = Value::Null;
        for (json, (env, setup)) in [(false, &self.text), (true, &self.json)] {
            let team = setup.team.to_str().expect("UTF-8 team path");
            let args: Vec<String> = args.iter().map(|arg| arg.replace(TEAM, team)).collect();
            let mut cmd = env.cmd_in(&setup.cwd);
            for (name, value) in vars {
                cmd.env(name, value);
            }
            if json {
                cmd.arg("--json");
            }
            let output = cmd.args(&args).output().expect("run d-mcp");
            let code = output.status.code().expect("exit code");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let form = if json { "json" } else { "text" };
            assert_eq!(
                code, expected,
                "{args:?} ({form}) exited with {code}:\n{stdout}\n{stderr}"
            );
            if json {
                document = serde_json::from_str(&stdout).expect("a JSON document");
            }
            self.seen
                .push((format!("{args:?} ({form})"), format!("{stdout}\n{stderr}")));
        }
        document
    }

    fn assert_clean(&self) {
        for (command, output) in &self.seen {
            assert!(
                !output.contains(CANARY),
                "a canary leaked from {command}:\n{output}"
            );
        }
    }

    /// The store files of both twins are canary-free; `present` must exist
    /// in each (relative to the store), so the check covers real content.
    fn assert_files_clean(&self, present: &[&str]) {
        for (env, _) in self.twins() {
            let store = env.default_store();
            for file in present {
                assert!(store.join(file).is_file(), "{file} is missing");
            }
            assert_files_clean(&store);
        }
    }
}

/// Every file below `root` (the store, `local/` included) is canary-free.
fn assert_files_clean(root: &Path) {
    for (path, contents) in snapshot(root) {
        if let Some(contents) = contents {
            let text = String::from_utf8_lossy(&contents);
            assert!(
                !text.contains(CANARY),
                "a canary reached {}:\n{text}",
                path.display()
            );
        }
    }
}

fn with_fakes() -> TestEnv {
    let env = TestEnv::new();
    let claude = env.fake_claude_dir();
    let codex = env.fake_codex_dir();
    env.with_path(&[claude, codex])
}

/// The names in the entries of an `inventory`, `status` or `uninstall`
/// document (`harnesses[].scopes[].entries[]` or `harnesses[].entries[]`).
fn entry_names(value: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for harness in value["data"]["harnesses"].as_array().expect("harnesses") {
        let scopes = harness["scopes"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![harness.clone()]);
        for scope in &scopes {
            for entry in scope["entries"].as_array().into_iter().flatten() {
                names.insert(entry["name"].as_str().expect("name").to_owned());
            }
        }
    }
    names
}

/// Asserts that `value` names every one of `expected`.
fn names_all(value: &Value, expected: &[&str]) {
    let names = entry_names(value);
    for name in expected {
        assert!(names.contains(*name), "{name} missing in {names:?}");
    }
}

/// The servers of an `adopt` document.
fn adopted_servers(value: &Value) -> Vec<&str> {
    value["data"]["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .map(|server| server["name"].as_str().expect("name"))
        .collect()
}

#[test]
fn no_canary_reaches_any_output_or_store_file() {
    let mut run = Run::new();
    run.both(0, &["init"]);
    run.both(
        0,
        &[
            "--dry-run",
            "source",
            "add",
            TEAM,
            "--id",
            "team",
            "--priority",
            "5",
        ],
    );
    run.both(
        0,
        &["source", "add", TEAM, "--id", "team", "--priority", "5"],
    );
    run.both(0, &["source", "list"]);
    for args in [
        &["select", "fetch"][..],
        &["select", "time", "--harness", "codex"],
        &["select", "wiki", "--harness", "codex"],
        &["select", "broken"],
        &["select", "github"],
        &["select", "notes", "--harness", "claude"],
    ] {
        run.both(0, args);
    }
    // `broken` is invalid: its problem names the position only.
    run.both(0, &["resolve"]);
    let inventory = run.both(0, &["inventory"]);
    names_all(
        &inventory,
        &[
            "github", "linear", "docs", "multi", "tasks", "api", "notes", "search", "helper",
            "inline", "scratch", "build", "deploy", "review",
        ],
    );
    run.both(1, &["sync", "--dry-run"]);
    run.both(0, &["sync", "--take-over", "time"]);
    // `github` and `notes` collide with foreign entries full of canaries.
    run.both(0, &["sync"]);
    run.both(0, &["--dry-run", "sync", "--take-over", "github"]);
    run.both(1, &["sync", "--take-over", "unknown-name"]);
    run.both(0, &["inventory", "--harness", "claude"]);
    run.both(0, &["--dry-run", "unmanage", "fetch"]);
    run.both(0, &["unmanage", "fetch", "--harness", "claude"]);
    run.both(0, &["unselect", "wiki"]);
    run.both(1, &["select", "Not A Name"]);
    adopt_everything(&mut run);
    status_and_doctor(&mut run);
    let preview = run.both(0, &["uninstall"]);
    names_all(&preview, &["tasks", "api", "time"]);
    run.both(0, &["uninstall", "--purge"]);
    run.both(0, &["--dry-run", "uninstall", "--apply", "--purge"]);
    // Broken harness files: every error names a position only.
    for (env, _) in run.twins() {
        let codex = fs::read_to_string(env.codex_config()).unwrap();
        fs::write(
            env.codex_config(),
            format!("{codex}\n[mcp_servers.late]\ncommand = \"CANARY_BROKEN\n"),
        )
        .unwrap();
        fs::write(
            env.claude_config(),
            "{\"mcpServers\": {\"a\": {\"env\": {\"T\": CANARY_BROKEN}}}}",
        )
        .unwrap();
    }
    run.both(3, &["sync"]);
    run.both(0, &["inventory"]);
    status_and_doctor_on_broken_files(&mut run);
    // The store still holds the adopted definition and the records.
    run.assert_files_clean(&["state.toml", "config.toml", "local/servers/tasks.toml"]);
    run.both(3, &["uninstall", "--apply", "--purge"]);
    run.both(0, &["source", "remove", "team"]);
    run.assert_clean();
    for (env, _) in run.twins() {
        assert_files_clean(&env.default_store());
    }
}

/// `status` and `doctor`, also with `--auth` and with missing variables;
/// the fake CLIs print a canary for `--version` in one round.
fn status_and_doctor(run: &mut Run) {
    let status = run.both(0, &["status"]);
    names_all(&status, &["tasks", "api", "time", "github", "notes"]);
    let auth = run.both(0, &["status", "--auth"]);
    names_all(&auth, &["tasks", "api"]);
    run.both(
        0,
        &["status", "--harness", "claude", "tasks", "api", "github"],
    );
    run.both_with(0, &[("TASKS_TOKEN", "CANARY_ENV")], &["status", "tasks"]);
    run.both(0, &["doctor"]);
    run.both(0, &["doctor", "--auth"]);
    for (env, _) in run.twins() {
        fs::write(env.codex_home().join("fake-codex-mode"), "version-garbage").unwrap();
        env.claude_mode(&["version CANARY_FAKE_VERSION (Claude Code)"]);
    }
    run.both(0, &["doctor"]);
    for (env, _) in run.twins() {
        fs::remove_file(env.codex_home().join("fake-codex-mode")).unwrap();
        env.claude_mode(&[]);
    }
}

/// `status` and `doctor` when the user-scope harness files cannot be read:
/// `doctor` reports them as errors (exit 1).
fn status_and_doctor_on_broken_files(run: &mut Run) {
    run.both(0, &["status"]);
    run.both(0, &["status", "--auth"]);
    run.both_with(0, &[("TASKS_TOKEN", "CANARY_ENV")], &["status", "tasks"]);
    run.both(1, &["doctor"]);
    run.both(1, &["doctor", "--auth"]);
}

/// `uninstall --apply` releases the adopted entries (whose replaced
/// literals were canaries) and removes the others; the store files stay
/// canary-free until `--purge` deletes them.
#[test]
fn uninstall_never_leaks() {
    let mut run = Run::new();
    run.both(
        0,
        &["source", "add", TEAM, "--id", "team", "--priority", "5"],
    );
    run.both(0, &["select", "fetch"]);
    run.both(0, &["select", "time", "--harness", "codex"]);
    run.both(0, &["sync", "--take-over", "time"]);
    adopt_everything(&mut run);
    let preview = run.both(0, &["uninstall"]);
    names_all(&preview, &["tasks", "api", "fetch", "time"]);
    let applied = run.both(0, &["uninstall", "--apply"]);
    names_all(&applied, &["tasks", "api", "fetch", "time"]);
    run.assert_files_clean(&["state.toml", "config.toml", "local/servers/tasks.toml"]);
    run.both(0, &["uninstall", "--apply", "--purge"]);
    run.both(1, &["doctor"]);
    run.assert_clean();
    for (env, _) in run.twins() {
        assert!(!env.default_store().join("config.toml").exists());
    }
}

/// Adoption: previews of every fixture entry (most are refused, by field
/// and reason), the gate with a canary in the shell, the apply of entries
/// whose literals are canaries, and an apply bound to a stale preview.
fn adopt_everything(run: &mut Run) {
    let all = [
        "claude:user:github",
        "codex:user:github",
        "claude:user:linear",
        "codex:user:linear",
        "claude:user:notes",
        "claude:user:multi",
        "claude:user:docs",
        "codex:user:search",
        "codex:user:helper",
        "codex:user:inline",
        "codex:user:time",
        "claude:local:notes",
        "claude:project:notes",
        "claude:project:review",
        "claude:project:build",
        "claude:project:deploy",
        "codex:project:build",
        "codex:project:deploy",
    ];
    let mut args = vec!["adopt"];
    args.extend(all);
    let preview = run.both(1, &args);
    let servers = adopted_servers(&preview);
    for name in [
        "github", "linear", "notes", "multi", "docs", "search", "helper", "inline", "time",
        "review", "build", "deploy",
    ] {
        assert!(servers.contains(&name), "{name} missing in {servers:?}");
    }
    run.both(
        1,
        &[
            "adopt",
            "claude:user:linear",
            "codex:user:linear",
            "--from",
            "codex:user",
            "--accept-relogin",
            "--client-secret-env",
            "LINEAR_SECRET",
        ],
    );
    run.both(1, &["adopt", "claude:project:notes", "--to-user-scope"]);
    run.both(
        1,
        &[
            "adopt",
            "claude:project:build",
            "codex:project:deploy",
            "--to-user-scope",
        ],
    );
    let entries = [
        "adopt",
        "claude:user:tasks",
        "codex:user:tasks",
        "claude:user:api",
    ];
    // The gate compares canaries in memory: a mismatch and a missing value.
    run.both_with(1, &[("TASKS_TOKEN", "CANARY_OTHER")], &entries);
    let vars = [
        ("TASKS_TOKEN", "CANARY_ENV"),
        ("API_TOKEN", "CANARY_HEADER"),
    ];
    let preview = run.both_with(0, &vars, &entries);
    assert_eq!(adopted_servers(&preview), ["api", "tasks"]);
    let expects: Vec<String> = preview["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|server| server["entries"].as_array().unwrap())
        .map(|entry| {
            format!(
                "--expect={}={}",
                entry["entry"].as_str().unwrap(),
                entry["display_hash"].as_str().unwrap()
            )
        })
        .collect();
    let mut apply: Vec<&str> = entries.to_vec();
    apply.push("--apply");
    apply.extend(expects.iter().map(String::as_str));
    // The twins share the hash key: the same `--expect` binds both.
    let value = run.both_with(0, &vars, &apply);
    assert_eq!(
        value["data"]["written"]["selections"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    run.assert_files_clean(&[
        "state.toml",
        "local/servers/tasks.toml",
        "local/servers/api.toml",
    ]);
    // Applied once: the entries changed since the preview (exit 3).
    run.both_with(3, &vars, &apply);
    let stale = format!("--expect=claude:user:docs={}", "0".repeat(64));
    run.both(3, &["adopt", "claude:user:docs", "--apply", &stale]);
}

/// Secret-shaped env keys and header names (here: `ghp_` names that
/// contain the canary) never reach an output or a store file. The import
/// refuses to turn such a literal into a variable reference (an
/// `invalid-name` gap on the redacted field); a header variable proposed
/// from a secret-shaped server name is shown redacted and refused
/// (`proposal-invalid`) until `--var` names another one. The same holds for
/// the forwarded and referenced forms of both harnesses: a forwarded env
/// key, a referenced header name, and the variable a header or bearer token
/// reads are all `invalid-name` gaps.
#[test]
fn secret_shaped_names_never_reach_any_output_or_store_file() {
    let env_key = "ghp_CANARY_ENV_KEY_0123";
    let header = "ghp_CANARY_HEADER_NAME";
    let var = "ghp_CANARY_VAR_NAME_0123";
    let forward = format!("${{{env_key}}}");
    let referenced = format!("${{{var}}}");
    let claude = serde_json::json!({
        "keyed": {"command": "keyed-mcp", "env": {env_key: "keyed-literal-4711"}},
        "hdr": {"type": "http", "url": "https://hdr.example.test/mcp",
                "headers": {header: "header-literal-4711"}},
        "fwd": {"command": "keyed-mcp", "env": {env_key: forward}},
        "refhdr": {"type": "http", "url": "https://hdr.example.test/mcp",
                   "headers": {header: "${SAFE_VAR}"}},
        "refvar": {"type": "http", "url": "https://hdr.example.test/mcp",
                   "headers": {"X-Key": referenced}},
        "refbearer": {"type": "http", "url": "https://hdr.example.test/mcp",
                      "headers": {"Authorization": format!("Bearer {referenced}")}},
    });
    let codex = format!(
        "[mcp_servers.keyed2]\ncommand = \"keyed-mcp\"\n[mcp_servers.keyed2.env]\n{env_key} = \"keyed-literal-4711\"\n\
         [mcp_servers.hdr2]\nurl = \"https://hdr.example.test/mcp\"\n[mcp_servers.hdr2.http_headers]\n{header} = \"header-literal-4711\"\n\
         [mcp_servers.akiacanary]\nurl = \"https://akia.example.test/mcp\"\n[mcp_servers.akiacanary.http_headers]\nX-Key = \"akia-literal-4711\"\n\
         [mcp_servers.fwd2]\ncommand = \"keyed-mcp\"\nenv_vars = [\"{env_key}\"]\n\
         [mcp_servers.fwd3]\ncommand = \"keyed-mcp\"\nenv_vars = [{{ name = \"{env_key}\" }}]\n\
         [mcp_servers.refhdr2]\nurl = \"https://hdr.example.test/mcp\"\n[mcp_servers.refhdr2.env_http_headers]\n{header} = \"SAFE_VAR\"\n\
         [mcp_servers.refvar2]\nurl = \"https://hdr.example.test/mcp\"\n[mcp_servers.refvar2.env_http_headers]\nX-Key = \"{var}\"\n\
         [mcp_servers.refbearer2]\nurl = \"https://hdr.example.test/mcp\"\nbearer_token_env_var = \"{var}\"\n"
    );
    let mut run = Run::new();
    for (env, _) in run.twins() {
        fs::write(
            env.claude_config(),
            serde_json::json!({ "mcpServers": claude }).to_string(),
        )
        .unwrap();
        fs::write(env.codex_config(), &codex).unwrap();
    }
    let inventory = run.both(0, &["inventory"]);
    let entries: Vec<&Value> = inventory["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|harness| harness["scopes"].as_array().unwrap())
        .flat_map(|scope| scope["entries"].as_array().unwrap())
        .collect();
    let import = |name: &str| {
        entries
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("no entry {name}"))["import"]
            .clone()
    };
    let gap = |field: &str| serde_json::json!([{"field": field, "reason": "invalid-name"}]);
    assert_eq!(import("keyed")["gaps"], gap("env.<redacted-key>"));
    assert_eq!(import("keyed2")["gaps"], gap("env.<redacted-key>"));
    assert_eq!(import("hdr")["gaps"], gap("headers.<redacted-key>"));
    assert_eq!(import("hdr2")["gaps"], gap("http_headers.<redacted-key>"));
    assert_eq!(import("akiacanary")["replaced"][0]["var"], "<redacted-key>");
    // The forwarded and referenced forms.
    let referenced_gaps = [
        ("claude", "fwd", "env.<redacted-key>"),
        ("claude", "refhdr", "headers.<redacted-key>"),
        ("claude", "refvar", "headers.X-Key"),
        ("claude", "refbearer", "headers.Authorization"),
        ("codex", "fwd2", "env_vars[0]"),
        ("codex", "fwd3", "env_vars[0]"),
        ("codex", "refhdr2", "env_http_headers.<redacted-key>"),
        ("codex", "refvar2", "env_http_headers.X-Key"),
        ("codex", "refbearer2", "bearer_token_env_var"),
    ];
    for (_, name, field) in referenced_gaps {
        assert_eq!(import(name)["gaps"], gap(field), "{name}");
    }
    run.both(0, &["inventory", "--harness", "codex"]);

    let refusals = |value: &Value| -> Vec<String> {
        value["data"]["servers"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|server| server["refusals"].as_array().unwrap())
            .map(|refusal| {
                format!(
                    "{} {} {}",
                    refusal["entry"].as_str().unwrap_or("-"),
                    refusal["field"].as_str().unwrap(),
                    refusal["reason"].as_str().unwrap()
                )
            })
            .collect()
    };
    let preview = run.both(
        1,
        &[
            "adopt",
            "claude:user:keyed",
            "codex:user:keyed2",
            "claude:user:hdr",
            "codex:user:hdr2",
        ],
    );
    let found = refusals(&preview);
    for expected in [
        "claude:user:keyed env.<redacted-key> invalid-name",
        "codex:user:keyed2 env.<redacted-key> invalid-name",
        "claude:user:hdr headers.<redacted-key> invalid-name",
        "codex:user:hdr2 http_headers.<redacted-key> invalid-name",
    ] {
        assert!(found.contains(&expected.to_owned()), "{found:?}");
    }
    // Each forwarded or referenced form is refused on its own, so none can
    // reach `local/servers/*.toml`.
    for (harness, name, field) in referenced_gaps {
        let entry = format!("{harness}:user:{name}");
        let preview = run.both(1, &["adopt", &entry]);
        let expected = format!("{entry} {field} invalid-name");
        assert!(refusals(&preview).contains(&expected), "{preview:#}");
    }
    // The proposal `AKIACANARY_X_KEY` looks like a secret; so does a
    // `--var` that is no better.
    let vars = [("SAFE_KEY_VAR", "akia-literal-4711")];
    let preview = run.both_with(1, &vars, &["adopt", "codex:user:akiacanary"]);
    assert!(
        refusals(&preview).contains(&"- http_headers.X-Key proposal-invalid".to_owned()),
        "{preview:#}"
    );
    assert_eq!(
        preview["data"]["servers"][0]["replaced"][0]["var"],
        "<redacted-key>"
    );
    let preview = run.both_with(
        1,
        &vars,
        &[
            "adopt",
            "codex:user:akiacanary",
            "--var",
            "http_headers.X-Key=AKIA_CANARY_VAR_NAME",
        ],
    );
    assert!(
        refusals(&preview).contains(&"- http_headers.X-Key variable-name".to_owned()),
        "{preview:#}"
    );
    let args = [
        "adopt",
        "codex:user:akiacanary",
        "--var",
        "http_headers.X-Key=SAFE_KEY_VAR",
    ];
    let preview = run.both_with(0, &vars, &args);
    let entry = &preview["data"]["servers"][0]["entries"][0];
    let expect = format!(
        "--expect=codex:user:akiacanary={}",
        entry["display_hash"].as_str().unwrap()
    );
    let mut apply = args.to_vec();
    apply.extend(["--apply", expect.as_str()]);
    run.both_with(0, &vars, &apply);
    run.both(0, &["status"]);
    run.both(0, &["uninstall"]);
    run.assert_files_clean(&["state.toml", "local/servers/akiacanary.toml"]);
    run.assert_clean();
    for (env, _) in run.twins() {
        let state = fs::read_to_string(env.default_store().join("state.toml")).unwrap();
        assert!(state.contains("SAFE_KEY_VAR"), "{state}");
    }
}

#[test]
fn login_state_listings_never_leak() {
    use d_mcp::adapter::Adapter;
    use d_mcp::adapter::LoginState;
    use d_mcp::adapter::claude::ClaudeAdapter;
    use d_mcp::adapter::codex::CodexAdapter;
    use d_mcp::process::ProcessRunner;
    let env = with_fakes();
    env.install_real_setup();
    let names: Vec<String> = ["linear", "github", "stdio1"]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let runner = ProcessRunner::default();
    for adapter in [&CodexAdapter as &dyn Adapter, &ClaudeAdapter] {
        let states = adapter.login_state(&env.environment(), &runner, &names);
        assert!(!format!("{states:?}").contains(CANARY));
        // The fakes answered: the listing was parsed, not just dropped.
        assert!(
            states.values().any(|state| *state != LoginState::Unknown),
            "{:?}: {states:?}",
            adapter.harness()
        );
    }
}

#[test]
fn discovery_debug_forms_never_leak() {
    use d_mcp::adapter::{registry, repository_root};
    let env = TestEnv::new();
    let setup = env.install_real_setup();
    let mut environment = env.environment();
    environment.cwd = Some(setup.cwd.clone());
    let mut imported_any = false;
    for adapter in registry() {
        let reads = adapter.discover(&environment, &repository_root(&setup.cwd));
        assert!(!reads.is_empty());
        assert!(
            reads.iter().any(|read| !read.entries.is_empty()),
            "{:?} read no entries",
            adapter.harness()
        );
        assert!(!format!("{reads:?}").contains(CANARY));
        for read in &reads {
            for name in read.entries.keys() {
                let imported = adapter.import(read, name);
                imported_any |= imported.is_ok();
                assert!(!format!("{imported:?}").contains(CANARY), "{name}");
            }
        }
    }
    assert!(imported_any);
}
