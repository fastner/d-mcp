//! `d-mcp adopt`: preview, literal replacement, variable naming, refusals,
//! scope widening, cross-harness conflicts, the OAuth flags, the `--expect`
//! binding, the variable gate and the scoped apply, against the fake
//! `claude` and Codex config files in an isolated environment.

mod common;

use std::fs;

use common::{CANARY, TestEnv, snapshot};
use d_mcp::adapter::Scope;
use d_mcp::config::Harness;
use d_mcp::state::{AdoptedFrom, OriginScope, RecordKey, RecordMeta, ReplacedField};
use d_mcp::store::StorePaths;
use serde_json::{Value, json};

/// An initialized store with the fake `claude` on `PATH`, and these Claude
/// Code user entries and Codex configuration.
fn setup(claude: Value, codex: &str) -> TestEnv {
    let env = TestEnv::new();
    let bin = env.fake_claude_dir();
    let env = env.with_path(&[bin]);
    env.init();
    fs::write(
        env.claude_config(),
        json!({ "mcpServers": claude }).to_string(),
    )
    .unwrap();
    fs::write(env.codex_config(), codex).unwrap();
    env
}

/// Runs `d-mcp --json adopt <args>` with extra environment variables.
fn adopt(env: &TestEnv, vars: &[(&str, &str)], args: &[&str]) -> (i32, Value, String) {
    let mut cmd = env.cmd();
    for (name, value) in vars {
        cmd.env(name, value);
    }
    let output = cmd.arg("--json").arg("adopt").args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}): {stdout}"));
    (
        output.status.code().unwrap(),
        value,
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// The `--expect` flags for every entry of a preview.
fn expects(data: &Value) -> Vec<String> {
    data["servers"]
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
        .collect()
}

/// Previews, then applies with the preview's `--expect` flags.
fn adopt_apply(env: &TestEnv, vars: &[(&str, &str)], args: &[&str]) -> (i32, Value, String) {
    let (code, preview, stderr) = adopt(env, vars, args);
    assert_eq!(code, 0, "{preview:#}\n{stderr}");
    let expects = expects(&preview["data"]);
    let mut all: Vec<&str> = args.to_vec();
    all.push("--apply");
    all.extend(expects.iter().map(String::as_str));
    adopt(env, vars, &all)
}

/// Every refusal as `entry field reason`.
fn refusals(value: &Value) -> Vec<String> {
    let data = &value["data"];
    let line = |refusal: &Value| {
        format!(
            "{} {} {}",
            refusal["entry"].as_str().unwrap_or("-"),
            refusal["field"].as_str().unwrap(),
            refusal["reason"].as_str().unwrap()
        )
    };
    let mut all: Vec<String> = data["refusals"]
        .as_array()
        .unwrap()
        .iter()
        .map(line)
        .collect();
    for server in data["servers"].as_array().unwrap() {
        all.extend(server["refusals"].as_array().unwrap().iter().map(line));
    }
    all
}

fn server<'a>(value: &'a Value, name: &str) -> &'a Value {
    value["data"]["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == name)
        .unwrap_or_else(|| panic!("no server {name}: {value:#}"))
}

fn origin<'a>(value: &'a Value, name: &str, entry: &str) -> &'a Value {
    server(value, name)["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["entry"] == entry)
        .unwrap_or_else(|| panic!("no entry {entry}: {value:#}"))
}

fn claude_entry(env: &TestEnv, name: &str) -> Value {
    let text = fs::read_to_string(env.claude_config()).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["mcpServers"][name].clone()
}

/// A TOML item as JSON, sub-tables included.
fn to_json(item: &toml_edit::Item) -> Value {
    fn value(item: &toml_edit::Value) -> Value {
        match item {
            toml_edit::Value::String(text) => json!(text.value()),
            toml_edit::Value::Integer(number) => json!(number.value()),
            toml_edit::Value::Float(number) => json!(number.value()),
            toml_edit::Value::Boolean(flag) => json!(flag.value()),
            toml_edit::Value::Datetime(time) => json!(time.value().to_string()),
            toml_edit::Value::Array(items) => Value::Array(items.iter().map(value).collect()),
            toml_edit::Value::InlineTable(table) => Value::Object(
                table
                    .iter()
                    .map(|(key, inner)| (key.to_owned(), value(inner)))
                    .collect(),
            ),
        }
    }
    match item {
        toml_edit::Item::Value(inner) => value(inner),
        toml_edit::Item::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, item)| (key.to_owned(), to_json(item)))
                .collect(),
        ),
        toml_edit::Item::ArrayOfTables(tables) => Value::Array(
            tables
                .iter()
                .map(|table| to_json(&toml_edit::Item::Table(table.clone())))
                .collect(),
        ),
        toml_edit::Item::None => Value::Null,
    }
}

fn read_codex(env: &TestEnv) -> Value {
    let text = fs::read_to_string(env.codex_config()).unwrap();
    let document: toml_edit::DocumentMut = text.parse().unwrap();
    to_json(document.as_item())
}

fn codex_table(env: &TestEnv, name: &str) -> Value {
    read_codex(env)["mcp_servers"][name].clone()
}

fn definition(env: &TestEnv, name: &str) -> String {
    fs::read_to_string(
        env.default_store()
            .join(format!("local/servers/{name}.toml")),
    )
    .unwrap()
}

/// The record metadata of a user-scope entry, from `state.toml` parsed as
/// d-mcp parses it.
fn meta(env: &TestEnv, harness: Harness, name: &str) -> RecordMeta {
    let state = d_mcp::state::load(&StorePaths::new(env.default_store())).unwrap();
    let key = RecordKey {
        harness,
        scope: Scope::User,
        name: name.to_owned(),
    };
    state
        .records
        .get(&key)
        .unwrap_or_else(|| panic!("no record for {name}: {:?}", state.records.keys()))
        .meta
        .clone()
}

fn replaced(field: &str, var: &str) -> ReplacedField {
    ReplacedField {
        field: field.to_owned(),
        var: var.to_owned(),
    }
}

fn adopted_from(harness: Harness, scope: OriginScope) -> Option<AdoptedFrom> {
    Some(AdoptedFrom { harness, scope })
}

const CODEX_KEY: &str = "[mcp_servers.key]\nurl = \"https://key.example.test/mcp\"\n\
                         [mcp_servers.key.http_headers]\nX-Api-Key = \"lit-key\"\n";

#[test]
fn every_env_and_header_literal_becomes_a_reference() {
    let env = setup(
        json!({
            "gh": {"command": "gh-mcp", "env": {"GH_HOST": "github.example.test", "FWD": "${FWD}"}},
            "api": {"type": "http", "url": "https://api.example.test/mcp",
                    "headers": {"Authorization": "Bearer lit-bearer"}},
        }),
        CODEX_KEY,
    );
    let vars = [
        ("GH_HOST", "github.example.test"),
        ("API_TOKEN", "lit-bearer"),
        ("KEY_X_API_KEY", "lit-key"),
    ];
    let (code, value, stderr) = adopt(
        &env,
        &vars,
        &["claude:user:gh", "claude:user:api", "codex:user:key"],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    // The preview names fields, kinds and variables; it writes nothing.
    let gh = server(&value, "gh");
    assert_eq!(
        gh["replaced"],
        json!([{"origin": "claude:user:gh", "field": "env.GH_HOST", "kind": "env", "var": "GH_HOST"}])
    );
    assert_eq!(
        gh["normalizations"],
        json!([{"origin": "claude:user:gh", "field": "env.FWD", "kind": "empty-default"}])
    );
    assert_eq!(origin(&value, "gh", "claude:user:gh")["write"], "update");
    assert_eq!(
        server(&value, "api")["replaced"][0]["kind"],
        "bearer-header"
    );
    assert_eq!(server(&value, "key")["replaced"][0]["var"], "KEY_X_API_KEY");
    assert!(!env.default_store().join("local/servers/gh.toml").exists());
    assert!(!value.to_string().contains("lit-"), "{value:#}");

    let (code, value, stderr) = adopt_apply(
        &env,
        &vars,
        &["claude:user:gh", "claude:user:api", "codex:user:key"],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(origin(&value, "gh", "claude:user:gh")["outcome"], "applied");
    let gh = definition(&env, "gh");
    assert!(gh.contains("GH_HOST = { forward = true }"), "{gh}");
    assert!(gh.contains("FWD = { forward = true }"), "{gh}");
    assert!(definition(&env, "api").contains("type = \"bearer_env\"\nvar = \"API_TOKEN\""));
    let key = definition(&env, "key");
    assert!(
        key.contains("type = \"header_env\"\nheader = \"X-Api-Key\"\nvar = \"KEY_X_API_KEY\""),
        "{key}"
    );
    assert_eq!(
        claude_entry(&env, "gh")["env"],
        json!({"FWD": "${FWD:-}", "GH_HOST": "${GH_HOST:-}"})
    );
    assert_eq!(
        claude_entry(&env, "api")["headers"],
        json!({"Authorization": "Bearer ${API_TOKEN}"})
    );
    assert_eq!(
        codex_table(&env, "key"),
        json!({"url": "https://key.example.test/mcp",
               "env_http_headers": {"X-Api-Key": "KEY_X_API_KEY"}})
    );
    for (path, contents) in snapshot(&env.default_store()) {
        let text = String::from_utf8_lossy(contents.as_deref().unwrap_or_default()).into_owned();
        assert!(!text.contains("lit-"), "{}: {text}", path.display());
    }
    let key = meta(&env, Harness::Codex, "key");
    assert_eq!(
        key.adopted_from,
        adopted_from(Harness::Codex, OriginScope::User)
    );
    assert_eq!(
        key.replaced_fields,
        [replaced("http_headers.X-Api-Key", "KEY_X_API_KEY")]
    );
    // The adopted entries are managed now; a plain sync changes nothing.
    let (code, value, _) = env.json(&["sync"]);
    assert_eq!(code, 0, "{value:#}");
    let operations: Vec<&str> = value["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|harness| harness["entries"].as_array().unwrap())
        .map(|entry| entry["operation"].as_str().unwrap())
        .collect();
    assert_eq!(operations, ["unchanged", "unchanged", "unchanged"]);
}

#[test]
fn keep_literal_and_var_choose_what_stays_and_how_it_is_named() {
    let env = setup(
        json!({
            "tool": {"command": "tool-mcp", "env": {"MODE": "fast",
                     "SECRET": "ghp_abcdefghijklmnopqrstuvwxyz0123456789"}},
            "api": {"type": "http", "url": "https://api.example.test/mcp",
                    "headers": {"Authorization": "Bearer t"}},
        }),
        "",
    );
    let vars = [
        ("SECRET", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        ("API_PAT", "t"),
    ];
    // A literal that looks like a secret is never kept.
    let (code, value, _) = adopt(
        &env,
        &vars,
        &[
            "claude:user:tool",
            "--keep-literal",
            "MODE",
            "--keep-literal",
            "SECRET",
        ],
    );
    assert_eq!(code, 1);
    assert_eq!(refusals(&value), ["- env.SECRET keep-literal-secret"]);
    // `--keep-literal` is for env only, and `--var` for replaced headers.
    let (code, value, _) = adopt(
        &env,
        &vars,
        &[
            "claude:user:api",
            "--keep-literal",
            "API_TOKEN",
            "--var",
            "env.MODE=X",
        ],
    );
    assert_eq!(code, 1);
    let found = refusals(&value);
    assert!(
        found.contains(&"- env.API_TOKEN keep-literal-unknown".to_owned()),
        "{found:?}"
    );
    assert!(
        found.contains(&"- env.MODE var-unknown-field".to_owned()),
        "{found:?}"
    );
    let (code, value, stderr) = adopt_apply(
        &env,
        &vars,
        &[
            "claude:user:tool",
            "claude:user:api",
            "--keep-literal",
            "MODE",
            "--var",
            "headers.Authorization=API_PAT",
        ],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(server(&value, "tool")["kept_literals"], json!(["MODE"]));
    let tool = definition(&env, "tool");
    assert!(tool.contains("MODE = { value = \"fast\" }"), "{tool}");
    assert!(tool.contains("SECRET = { forward = true }"), "{tool}");
    assert_eq!(
        claude_entry(&env, "tool")["env"],
        json!({"MODE": "fast", "SECRET": "${SECRET:-}"})
    );
    assert!(definition(&env, "api").contains("var = \"API_PAT\""));
    assert_eq!(
        claude_entry(&env, "api")["headers"]["Authorization"],
        "Bearer ${API_PAT}"
    );
    assert_eq!(
        meta(&env, Harness::Claude, "api").replaced_fields,
        [replaced("headers.Authorization", "API_PAT")]
    );
}

#[test]
fn header_variables_must_work_for_both_adapters() {
    let codex = "[mcp_servers.aws]\nurl = \"https://aws.example.test/mcp\"\n\
                 [mcp_servers.aws.http_headers]\nAuthorization = \"Bearer t\"\n\
                 [mcp_servers.2fa]\nurl = \"https://twofa.example.test/mcp\"\n\
                 [mcp_servers.2fa.http_headers]\nX-Key = \"k\"\n";
    let env = setup(
        json!({"aws": {"type": "http", "url": "https://aws.example.test/mcp",
                       "headers": {"Authorization": "Bearer t"}}}),
        codex,
    );
    // `AWS_TOKEN` is a name Claude Code reads as empty in headers;
    // `2FA_X_KEY` is no variable name at all.
    let (code, value, _) = adopt(
        &env,
        &[],
        &["claude:user:aws", "codex:user:aws", "codex:user:2fa"],
    );
    assert_eq!(code, 1);
    let found = refusals(&value);
    assert!(
        found.contains(&"- headers.Authorization proposal-credential-name".to_owned()),
        "{found:?}"
    );
    assert!(
        found.contains(&"- http_headers.X-Key proposal-invalid".to_owned()),
        "{found:?}"
    );
    // A `--var` that is no better is refused as well.
    let (_, value, _) = adopt(
        &env,
        &[],
        &[
            "codex:user:aws",
            "--var",
            "http_headers.Authorization=AWS_SESSION",
        ],
    );
    assert!(refusals(&value).contains(&"- http_headers.Authorization credential-name".to_owned()));
    let (_, value, _) = adopt(
        &env,
        &[],
        &["codex:user:2fa", "--var", "http_headers.X-Key=1X"],
    );
    assert!(refusals(&value).contains(&"- http_headers.X-Key variable-name".to_owned()));
    // One name for both harnesses, through either native field.
    let (code, value, stderr) = adopt_apply(
        &env,
        &[("CLOUD_TOKEN", "t"), ("TWOFA_KEY", "k")],
        &[
            "claude:user:aws",
            "codex:user:aws",
            "codex:user:2fa",
            "--var",
            "headers.Authorization=CLOUD_TOKEN",
            "--var",
            "http_headers.X-Key=TWOFA_KEY",
        ],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(server(&value, "aws")["verdict"], "duplicate");
    assert_eq!(
        claude_entry(&env, "aws")["headers"]["Authorization"],
        "Bearer ${CLOUD_TOKEN}"
    );
    assert_eq!(
        codex_table(&env, "aws")["bearer_token_env_var"],
        "CLOUD_TOKEN"
    );
    assert_eq!(
        codex_table(&env, "2fa")["env_http_headers"],
        json!({"X-Key": "TWOFA_KEY"})
    );
    assert_eq!(
        meta(&env, Harness::Codex, "aws").replaced_fields,
        [replaced("http_headers.Authorization", "CLOUD_TOKEN")]
    );
    assert_eq!(
        meta(&env, Harness::Claude, "aws").replaced_fields,
        [replaced("headers.Authorization", "CLOUD_TOKEN")]
    );
}

/// Each refusal reason, one entry each: `(arguments, expected refusal)`.
#[test]
fn every_refusal_names_the_field_and_reason_only() {
    let codex = "[mcp_servers.helper]\nurl = \"https://h.example.test/mcp\"\nhttp_headers_helper = \"/bin/h\"\n\
                 [mcp_servers.cwd]\ncommand = \"x\"\ncwd = \"/srv/ghp_abcdefghijklmnopqrstuvwxyz0123456789\"\n\
                 [mcp_servers.query]\nurl = \"https://q.example.test/mcp?token=t\"\n\
                 [mcp_servers.Upper]\ncommand = \"x\"\n\
                 [mcp_servers.tools]\ncommand = \"x\"\nenabled_tools = [\"a\"]\n\
                 [mcp_servers.plain]\ncommand = \"plain-mcp\"\n\
                 [mcp_servers.vars]\ncommand = \"x\"\nargs = [\"$HOME/x\"]\n\
                 [mcp_servers]\ninline = { command = \"x\" }\n";
    let claude = json!({
        "multi": {"type": "http", "url": "https://m.example.test", "headers": {"A": "${A}", "B": "${B}"}},
        "mixed": {"type": "http", "url": "https://m.example.test", "headers": {"A": "${A}"},
                  "oauth": {"clientId": "c"}},
        "unknown": {"command": "x", "weird": 1},
        "hhelper": {"type": "http", "url": "https://h.example.test", "headersHelper": "/bin/h"},
        "renamed": {"command": "x", "env": {"A": "${B}"}},
        "default": {"command": "x", "env": {"A": "${A:-d}"}},
        "sse": {"type": "sse", "url": "https://s.example.test/sse"},
        "args": {"command": "x", "args": ["postgres://u:pw@db.example.test/x"]},
        "token": {"command": "x", "args": ["--key", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"]},
        "vars": {"command": "x", "args": ["$HOME/x"]},
        "relcmd": {"command": "bin/x"},
        "scheme": {"type": "http", "url": "http://plain.example.test/mcp"},
        "lower": {"type": "http", "url": "https://l.example.test", "headers": {"authorization": "Bearer t"}},
        "awsvar": {"type": "http", "url": "https://a.example.test", "headers": {"X-Key": "${AWS_SECRET_ACCESS_KEY}"}},
        "plain": {"command": "other-mcp"},
        "taken": {"command": "taken-mcp"},
        "tools": {"command": "x"},
    });
    let env = setup(claude, codex);
    fs::write(env.claude_config(), {
        let mut config: Value =
            serde_json::from_str(&fs::read_to_string(env.claude_config()).unwrap()).unwrap();
        config["projects"] = json!({"/p": {"mcpServers": {"local1": {"command": "x"}}}});
        config.to_string()
    })
    .unwrap();
    fs::write(env.work().join(".mcp.json"), r#"{"mcpServers": {"proj": {"command": "x", "args": ["./run"]}, "taken": {"command": "taken-mcp"}}}"#).unwrap();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    env.write_local("exists", &common::stdio("exists", "x"));
    let team = env.source_dir("team");
    fs::write(team.join("servers/plain.toml"), common::stdio("plain", "y")).unwrap();
    let cases: &[(&[&str], &str)] = &[
        (
            &["claude:user:multi"],
            "claude:user:multi headers.B multiple-headers",
        ),
        (
            &["claude:user:mixed"],
            "claude:user:mixed oauth header-with-oauth",
        ),
        (
            &["claude:user:unknown"],
            "claude:user:unknown weird unknown-key",
        ),
        (
            &["claude:user:hhelper"],
            "claude:user:hhelper headersHelper excluded-key",
        ),
        (
            &["codex:user:helper"],
            "codex:user:helper http_headers_helper excluded-key",
        ),
        (
            &["codex:user:inline"],
            "codex:user:inline  unsupported-style",
        ),
        (
            &["claude:user:renamed"],
            "claude:user:renamed env.A renamed-forward",
        ),
        (
            &["claude:user:default"],
            "claude:user:default env.A forward-default",
        ),
        (&["claude:user:sse"], "claude:user:sse type sse-transport"),
        (&["claude:user:args"], "- transport.args[0] url-userinfo"),
        (&["claude:user:token"], "- transport.args[1] secret-prefix"),
        (
            &["claude:user:vars"],
            "claude:user:vars args[0] embedded-variable",
        ),
        (&["codex:user:vars"], "- transport.args[0] variable-syntax"),
        (&["codex:user:cwd"], "- harness.codex.cwd secret-prefix"),
        (&["codex:user:query"], "- transport.url url-query"),
        (&["claude:user:scheme"], "- transport.url url-scheme"),
        (&["claude:user:relcmd"], "- transport.command command-rule"),
        (&["codex:user:Upper"], "- name name-rule"),
        (&["claude:user:lower"], "claude:user:lower  round-trip"),
        (
            &["claude:user:awsvar"],
            "claude:user:awsvar auth.var render-gap",
        ),
        (
            &["codex:user:tools", "claude:user:tools"],
            "claude:user:tools harness.codex.enabled_tools render-gap",
        ),
        (&["claude:local:local1"], "claude:local:local1  local-scope"),
        (
            &["claude:project:proj"],
            "claude:project:proj  project-origin",
        ),
        (
            &["claude:project:proj", "--to-user-scope"],
            "claude:project:proj args[0] relative-path",
        ),
        (
            &["claude:project:taken", "--to-user-scope"],
            "claude:project:taken  user-entry-exists",
        ),
        (&["claude:user:nothing"], "claude:user:nothing  not-found"),
        (&["claude:user:plain", "codex:user:plain"], "- - conflict"),
        (
            &["claude:user:plain", "claude:user:plain"],
            "claude:user:plain  duplicate-origin",
        ),
        (
            &["claude:user:plain", "--from", "codex:user"],
            "- codex:user from-unknown",
        ),
        (
            &["claude:user:plain", "--prefer-oauth"],
            "- auth prefer-oauth-invalid",
        ),
        (
            &["claude:user:plain", "--oauth-supported"],
            "- auth oauth-supported-invalid",
        ),
        (
            &["claude:user:plain", "--client-secret-env", "S"],
            "- auth.client_secret_env client-secret-env-invalid",
        ),
    ];
    for (args, expected) in cases {
        let (code, value, stderr) = adopt(&env, &[], args);
        let found = refusals(&value);
        let expected = expected.replace("- - conflict", "-  conflict");
        assert!(
            found.contains(&expected),
            "{args:?}: expected `{expected}` in {found:?}\n{value:#}"
        );
        assert_eq!(code, 1, "{args:?}");
        assert!(stderr.is_empty() || !stderr.contains("ghp_"), "{stderr}");
        assert!(!value.to_string().contains("ghp_"), "{value:#}");
        assert!(!value.to_string().contains("pw@"), "{value:#}");
    }
    // `codex:user:tools` alone renders an important gap for Claude Code only
    // when adopted for Claude Code too; for Codex alone it is fine.
    let (code, value, _) = adopt(&env, &[], &["codex:user:tools"]);
    assert_eq!(code, 0, "{value:#}");
    // The store side: an existing definition, a higher-priority source, an
    // existing selection with another harness set, an existing record.
    let (_, value, _) = adopt(&env, &[], &["claude:user:exists"]);
    assert!(refusals(&value).contains(&"claude:user:exists  not-found".to_owned()));
    fs::write(env.claude_config(), json!({"mcpServers": {"exists": {"command": "x"}, "plain": {"command": "other-mcp"}, "taken": {"command": "taken-mcp"}}}).to_string()).unwrap();
    let (_, value, _) = adopt(&env, &[], &["claude:user:exists"]);
    assert_eq!(refusals(&value), ["-  local-exists"]);
    let team = team.to_str().unwrap();
    env.cmd()
        .args(["source", "add", team, "--id", "team", "--priority", "5"])
        .assert()
        .success();
    let (_, value, _) = adopt(&env, &[], &["claude:user:plain"]);
    assert_eq!(refusals(&value), ["-  not-winning"]);
    env.cmd()
        .args(["select", "taken", "--harness", "codex"])
        .assert()
        .success();
    let (_, value, _) = adopt(&env, &[], &["claude:user:taken"]);
    assert_eq!(refusals(&value), ["-  selection-differs"]);
    env.cmd()
        .args(["select", "taken", "--harness", "claude"])
        .assert()
        .success();
    env.write_local("taken", &common::stdio("taken", "taken-mcp"));
    let (code, value, _) = env.json(&["sync", "--take-over", "taken"]);
    assert_eq!(code, 0, "{value:#}");
    let (_, value, _) = adopt(&env, &[], &["claude:user:taken"]);
    assert!(refusals(&value).contains(&"claude:user:taken  already-managed".to_owned()));
}

#[test]
fn a_project_entry_moves_to_the_user_scope_and_keeps_shadowing() {
    let env = setup(json!({}), "");
    fs::create_dir_all(env.work().join(".git")).unwrap();
    fs::write(
        env.work().join(".mcp.json"),
        r#"{"mcpServers": {"review": {"type": "http", "url": "https://review.example.test/mcp",
            "headers": {"Authorization": "Bearer lit"}}}}"#,
    )
    .unwrap();
    let (code, value, stderr) = adopt_apply(
        &env,
        &[("REVIEW_TOKEN", "lit")],
        &["claude:project:review", "--to-user-scope"],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    let entry = origin(&value, "review", "claude:project:review");
    assert_eq!(entry["write"], "add");
    assert_eq!(entry["outcome"], "applied");
    assert_eq!(entry["path"], env.work().to_str().unwrap());
    let notices = server(&value, "review")["notices"].to_string();
    assert!(notices.contains("still holds its literals"), "{notices}");
    assert!(
        notices.contains("shadows the adopted user entry"),
        "{notices}"
    );
    assert_eq!(
        claude_entry(&env, "review")["headers"]["Authorization"],
        "Bearer ${REVIEW_TOKEN}"
    );
    // The origin is left as it is.
    assert!(
        fs::read_to_string(env.work().join(".mcp.json"))
            .unwrap()
            .contains("Bearer lit")
    );
    let review = meta(&env, Harness::Claude, "review");
    assert_eq!(
        review.adopted_from,
        adopted_from(Harness::Claude, OriginScope::Project { root: env.work() })
    );
    assert_eq!(
        review.replaced_fields,
        [replaced("headers.Authorization", "REVIEW_TOKEN")]
    );
}

#[test]
fn from_chooses_the_core_of_a_conflict() {
    let env = setup(
        json!({"fs": {"command": "fs-mcp", "args": ["--old"]}}),
        "[mcp_servers.fs]\ncommand = \"fs-mcp\"\nargs = [\"--new\"]\ncwd = \"/srv\"\n",
    );
    let (code, value, _) = adopt(&env, &[], &["claude:user:fs", "codex:user:fs"]);
    assert_eq!(code, 1);
    assert_eq!(server(&value, "fs")["verdict"], "conflict");
    let args = ["claude:user:fs", "codex:user:fs", "--from", "codex:user"];
    let (code, value, _) = adopt(&env, &[], &args);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(server(&value, "fs")["from"], "codex:user");
    // The preview shows that Claude Code's entry takes Codex's core, and
    // that Codex's entry is taken over unchanged.
    assert_eq!(
        origin(&value, "fs", "claude:user:fs")["changed"],
        json!(["args"])
    );
    assert_eq!(origin(&value, "fs", "codex:user:fs")["write"], "none");
    let (code, value, stderr) = adopt_apply(&env, &[], &args);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(claude_entry(&env, "fs")["args"], json!(["--new"]));
    let fs_definition = definition(&env, "fs");
    assert!(
        fs_definition.contains("[harness.codex]\ncwd = \"/srv\""),
        "{fs_definition}"
    );
    // The untouched Codex entry is a take-over: its hash is shown keyed.
    let codex = value["data"]["sync"]["harnesses"][0]["entries"][0].clone();
    assert_eq!(codex["outcome"], "recorded");
    assert_eq!(codex["hash"], Value::Null);
    assert_eq!(codex["display_hash"].as_str().unwrap().len(), 64);
    let claude = value["data"]["sync"]["harnesses"][1]["entries"][0].clone();
    assert_eq!(claude["display_hash"], Value::Null);
    assert_eq!(claude["hash"].as_str().unwrap().len(), 64);
}

#[test]
fn oauth_flags() {
    let env = setup(
        json!({
            "bearer": {"type": "http", "url": "https://b.example.test/mcp",
                       "headers": {"Authorization": "Bearer t"}},
            "flag": {"type": "http", "url": "https://f.example.test/mcp",
                     "headers": {"Authorization": "Bearer t"}},
            "login": {"type": "http", "url": "https://l.example.test/mcp", "oauth": {"scopes": "b a"}},
            "client": {"type": "http", "url": "https://c.example.test/mcp",
                       "oauth": {"clientId": "cid", "scopes": "b a"}},
            "same": {"type": "http", "url": "https://s.example.test/mcp", "oauth": {"clientId": "cid"}},
        }),
        "",
    );
    // `--prefer-oauth`: the static header goes, and its variable with it.
    let (code, value, stderr) = adopt_apply(&env, &[], &["claude:user:bearer", "--prefer-oauth"]);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert!(
        server(&value, "bearer")["variables"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(definition(&env, "bearer").contains("[auth]\ntype = \"oauth\""));
    assert_eq!(claude_entry(&env, "bearer").get("headers"), None);
    // `--oauth-supported` keeps the bearer token.
    let (code, value, _) = adopt_apply(
        &env,
        &[("FLAG_TOKEN", "t")],
        &["claude:user:flag", "--oauth-supported"],
    );
    assert_eq!(code, 0, "{value:#}");
    assert!(definition(&env, "flag").contains("oauth_supported = true"));
    // Rewriting an OAuth-capable Claude Code entry clears its tokens.
    let (code, value, _) = adopt(&env, &[], &["claude:user:login"]);
    assert_eq!(code, 1);
    assert_eq!(
        refusals(&value),
        ["claude:user:login oauth relogin-required"]
    );
    let (code, value, _) = adopt_apply(&env, &[], &["claude:user:login", "--accept-relogin"]);
    assert_eq!(code, 0, "{value:#}");
    assert!(
        server(&value, "login")["notices"]
            .to_string()
            .contains("claude mcp login login")
    );
    assert_eq!(claude_entry(&env, "login")["oauth"]["scopes"], "a b");
    // With a client id, the client secret must come from a variable.
    let (_, value, _) = adopt(&env, &[], &["claude:user:client", "--accept-relogin"]);
    assert_eq!(
        refusals(&value),
        ["claude:user:client oauth.clientId client-secret-env-required"]
    );
    let args = [
        "claude:user:client",
        "--accept-relogin",
        "--client-secret-env",
        "DMCP_TEST_CLIENT_SECRET",
    ];
    let (_, value, _) = adopt(&env, &[], &args);
    assert_eq!(
        refusals(&value),
        ["- auth.client_secret_env variable-missing"]
    );
    let (code, value, stderr) = adopt_apply(&env, &[("DMCP_TEST_CLIENT_SECRET", "s")], &args);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert!(definition(&env, "client").contains("client_secret_env = \"DMCP_TEST_CLIENT_SECRET\""));
    let add = env
        .claude_calls()
        .into_iter()
        .rfind(|call| call["args"].to_string().contains("add-json"))
        .unwrap();
    assert_eq!(add["secret"], "present", "{add:#}");
    // An identical rendering needs no write, so no re-login either.
    let (code, value, _) = adopt(&env, &[], &["claude:user:same"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(origin(&value, "same", "claude:user:same")["write"], "none");
}

#[test]
fn apply_is_bound_to_the_preview() {
    let env = setup(
        json!({"a": {"command": "a-mcp", "env": {"DMCP_TEST_A_KEY": "v"}}}),
        "",
    );
    let vars = [("DMCP_TEST_A_KEY", "v")];
    let (code, preview, _) = adopt(&env, &vars, &["claude:user:a"]);
    assert_eq!(code, 0);
    let expect = expects(&preview["data"]);
    // Without `--expect`, nothing is applied.
    let (code, value, _) = adopt(&env, &vars, &["claude:user:a", "--apply"]);
    assert_eq!(code, 1);
    assert_eq!(refusals(&value), ["claude:user:a  expect-missing"]);
    let other = format!("--expect=claude:user:b={}", "0".repeat(64));
    let (code, value, _) = adopt(
        &env,
        &vars,
        &["claude:user:a", "--apply", &expect[0], &other],
    );
    assert_eq!(code, 1);
    assert_eq!(refusals(&value), ["claude:user:b  expect-unknown"]);
    // `--apply --dry-run` is the preview.
    let (code, value, _) = adopt(
        &env,
        &vars,
        &["--dry-run", "claude:user:a", "--apply", &expect[0]],
    );
    assert_eq!(code, 0);
    assert_eq!(value["data"]["apply"], false);
    // The entry changes after the preview: exit 3, nothing written.
    fs::write(
        env.claude_config(),
        json!({"mcpServers": {"a": {"command": "a-mcp", "env": {"DMCP_TEST_A_KEY": "w"}}}})
            .to_string(),
    )
    .unwrap();
    let (code, value, _) = adopt(
        &env,
        &[("DMCP_TEST_A_KEY", "w")],
        &["claude:user:a", "--apply", &expect[0]],
    );
    assert_eq!(code, 3, "{value:#}");
    assert_eq!(value["error"]["code"], "adopt-entry-changed");
    assert_eq!(value["data"]["written"], Value::Null);
    assert!(!env.default_store().join("local/servers/a.toml").exists());
    assert!(
        !fs::read_to_string(env.default_store().join("config.toml"))
            .unwrap()
            .contains("[[selections]]")
    );
    // Precedence 3 over 1: a changed entry wins over a refusal.
    let (code, _, _) = adopt(&env, &[], &["claude:user:a", "--apply", &expect[0]]);
    assert_eq!(code, 3);
}

#[test]
fn the_variable_gate_compares_in_memory_and_overrides_say_what_is_lost() {
    let entry = json!({"a": {"command": "a-mcp", "env": {"DMCP_TEST_A_KEY": "lit-value"}}});
    let env = setup(entry.clone(), "");
    let (code, value, _) = adopt(&env, &[], &["claude:user:a"]);
    assert_eq!(code, 1);
    assert_eq!(
        refusals(&value),
        ["claude:user:a env.DMCP_TEST_A_KEY variable-missing"]
    );
    assert_eq!(server(&value, "a")["variables"][0]["status"], "missing");
    let (code, value, _) = adopt(&env, &[("DMCP_TEST_A_KEY", "")], &["claude:user:a"]);
    assert_eq!(code, 1);
    assert_eq!(server(&value, "a")["variables"][0]["status"], "missing");
    let (code, value, _) = adopt(
        &env,
        &[("DMCP_TEST_A_KEY", "shell-value-0815")],
        &["claude:user:a"],
    );
    assert_eq!(code, 1);
    assert_eq!(
        refusals(&value),
        ["claude:user:a env.DMCP_TEST_A_KEY variable-mismatch"]
    );
    assert!(!value.to_string().contains("lit-value"));
    assert!(!value.to_string().contains("shell-value-0815"), "{value:#}");
    let (code, value, stderr) = adopt_apply(
        &env,
        &[("DMCP_TEST_A_KEY", "shell-value-0815")],
        &["claude:user:a", "--allow-env-mismatch"],
    );
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    let check = &server(&value, "a")["variables"][0];
    assert_eq!(
        (check["status"].as_str(), check["accepted"].as_bool()),
        (Some("differs"), Some(true))
    );
    assert!(
        server(&value, "a")["notices"]
            .to_string()
            .contains("unrecoverable after apply")
    );
    let env = setup(entry, "");
    let (code, value, _) = adopt_apply(&env, &[], &["claude:user:a", "--allow-missing-env"]);
    assert_eq!(code, 0, "{value:#}");
    assert!(
        server(&value, "a")["notices"]
            .to_string()
            .contains("unset or empty")
    );
    assert_eq!(
        claude_entry(&env, "a")["env"]["DMCP_TEST_A_KEY"],
        "${DMCP_TEST_A_KEY:-}"
    );
}

#[test]
fn a_claude_add_failing_after_the_remove_names_server_and_variables() {
    let env = setup(
        json!({"api": {"type": "http", "url": "https://api.example.test/mcp",
                       "headers": {"Authorization": "Bearer lit"}}}),
        "",
    );
    env.claude_mode(&["fail add-json api"]);
    let (code, value, stderr) = adopt_apply(&env, &[("API_TOKEN", "lit")], &["claude:user:api"]);
    assert_eq!(code, 1, "{value:#}\n{stderr}");
    assert_eq!(value["error"]["code"], "harness-cli-failed");
    assert_eq!(origin(&value, "api", "claude:user:api")["outcome"], "lost");
    let notices = value["data"]["notices"].to_string();
    assert!(
        notices.contains("`api` was removed from Claude Code"),
        "{notices}"
    );
    assert!(notices.contains("`API_TOKEN`"), "{notices}");
    assert_eq!(claude_entry(&env, "api"), Value::Null);
    // Definition and selection stay; the next sync re-adds it in reference
    // form and counts the vanish.
    assert!(definition(&env, "api").contains("var = \"API_TOKEN\""));
    env.claude_mode(&[]);
    let (code, value, _) = env.json(&["sync"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(
        value["data"]["harnesses"][1]["entries"][0]["operation"],
        "vanished"
    );
    assert_eq!(
        claude_entry(&env, "api")["headers"]["Authorization"],
        "Bearer ${API_TOKEN}"
    );
    let api = meta(&env, Harness::Claude, "api");
    assert_eq!(api.vanish_count, 1);
    assert!(api.last_vanish.is_some());
    assert_eq!(
        api.adopted_from,
        adopted_from(Harness::Claude, OriginScope::User)
    );
}

#[test]
fn a_failure_after_the_definition_write_is_reported_per_entry() {
    let env = setup(
        json!({"tasks": {"command": "tasks-mcp", "env": {"T": "v"}}}),
        "[mcp_servers.tasks]\ncommand = \"tasks-mcp\"\n[mcp_servers.tasks.env]\nT = \"v\"\n",
    );
    env.claude_mode(&["fail remove tasks"]);
    let (code, value, stderr) = adopt_apply(
        &env,
        &[("T", "v")],
        &["claude:user:tasks", "codex:user:tasks"],
    );
    assert_eq!(code, 1, "{value:#}\n{stderr}");
    let claude = origin(&value, "tasks", "claude:user:tasks");
    assert_eq!(claude["outcome"], "failed");
    assert_eq!(claude["error"]["code"], "harness-cli-failed");
    assert_eq!(
        origin(&value, "tasks", "codex:user:tasks")["outcome"],
        "applied"
    );
    assert!(
        value["data"]["notices"]
            .to_string()
            .contains("was not taken over in claude (failed)")
    );
    assert_eq!(value["data"]["written"]["selections"], json!(["tasks"]));
    assert!(definition(&env, "tasks").contains("T = { forward = true }"));
    assert_eq!(codex_table(&env, "tasks")["env_vars"], json!(["T"]));
    // The Claude Code entry is untouched; its take-over is recorded with the
    // adoption, so the next sync completes it.
    assert_eq!(claude_entry(&env, "tasks")["env"]["T"], "v");
    env.claude_mode(&[]);
    let (code, value, _) = env.json(&["sync"]);
    assert_eq!(code, 0, "{value:#}");
    assert_eq!(claude_entry(&env, "tasks")["env"]["T"], "${T:-}");
    for harness in [Harness::Claude, Harness::Codex] {
        assert_eq!(
            meta(&env, harness, "tasks").adopted_from,
            adopted_from(harness, OriginScope::User)
        );
    }
}

#[test]
fn a_missing_claude_cli_is_an_environment_error() {
    let env = setup(
        json!({"a": {"command": "a-mcp", "env": {"DMCP_TEST_A_KEY": "v"}}}),
        "",
    );
    let env = env.without_claude();
    let (code, value, _) = adopt(&env, &[("DMCP_TEST_A_KEY", "v")], &["claude:user:a"]);
    assert_eq!(code, 4, "{value:#}");
    assert_eq!(value["error"]["code"], "harness-cli-not-found");
    assert_eq!(value["data"]["servers"][0]["name"], "a");
}

/// The acceptance criterion on the combined fixture: adopt plus sync
/// changes nothing outside the adopted entries, and inside them only the
/// replaced fields and the declared normalizations.
#[test]
fn adopt_and_sync_change_only_the_adopted_entries() {
    let env = TestEnv::new();
    let bin = env.fake_claude_dir();
    let env = env.with_path(&[bin]);
    let setup = env.install_real_setup();
    // A selected server that the scoped sync must leave alone.
    env.cmd().args(["select", "fetch"]).assert().success();
    let read_claude = || -> Value {
        serde_json::from_str(&fs::read_to_string(env.claude_config()).unwrap()).unwrap()
    };
    let (claude_before, codex_before) = (read_claude(), read_codex(&env));
    let vars = [
        ("TASKS_TOKEN", "CANARY_ENV"),
        ("API_TOKEN", "CANARY_HEADER"),
    ];
    let mut cmd_args = vec!["claude:user:tasks", "codex:user:tasks", "claude:user:api"];
    let (code, preview, _) = {
        let mut cmd = env.cmd_in(&setup.cwd);
        for (name, value) in vars {
            cmd.env(name, value);
        }
        let output = cmd
            .arg("--json")
            .arg("adopt")
            .args(&cmd_args)
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            (),
        )
    };
    assert_eq!(code, 0, "{preview:#}");
    let expects = expects(&preview["data"]);
    cmd_args.push("--apply");
    cmd_args.extend(expects.iter().map(String::as_str));
    let mut cmd = env.cmd_in(&setup.cwd);
    for (name, value) in vars {
        cmd.env(name, value);
    }
    let output = cmd
        .arg("--json")
        .arg("adopt")
        .args(&cmd_args)
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{value:#}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!value.to_string().contains(CANARY), "{value:#}");
    assert!(!stderr.contains(CANARY), "{stderr}");
    // The scoped sync planned and reported the adopted servers only.
    let reported: Vec<&str> = value["data"]["sync"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|harness| harness["entries"].as_array().unwrap())
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(reported, ["tasks", "api", "tasks"]);
    let (claude_after, codex_after) = (read_claude(), read_codex(&env));

    // Claude Code: every other key and entry is unchanged.
    let mut expected = claude_before;
    expected["mcpServers"]["tasks"]["env"] =
        json!({"TASKS_HOME": "${TASKS_HOME:-}", "TASKS_TOKEN": "${TASKS_TOKEN:-}"});
    expected["mcpServers"]["api"]["headers"] = json!({"Authorization": "Bearer ${API_TOKEN}"});
    assert_eq!(claude_after, expected);
    // Codex: every other key and table is unchanged; `tasks` moved its
    // literal into `env_vars`.
    let mut expected = codex_before;
    expected["mcp_servers"]["tasks"] = json!({
        "command": "tasks-mcp", "args": ["--stdio"], "env_vars": ["TASKS_HOME", "TASKS_TOKEN"],
    });
    assert_eq!(codex_after, expected);
    // `fetch` was not synced by the adoption; a full sync adds it and
    // leaves the adopted entries unchanged.
    assert_eq!(codex_after["mcp_servers"].get("fetch"), None);
    let (code, value, _) = env.json(&["sync"]);
    assert_eq!(code, 0, "{value:#}");
    let entries: Vec<(String, String)> = value["data"]["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|harness| harness["entries"].as_array().unwrap())
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_owned(),
                entry["operation"].as_str().unwrap().to_owned(),
            )
        })
        .filter(|(name, _)| name == "tasks" || name == "api")
        .collect();
    // `tasks` in both harnesses and Claude Code's `api`.
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert!(
        entries
            .iter()
            .all(|(_, operation)| operation == "unchanged"),
        "{entries:?}"
    );
    let synced = read_codex(&env);
    assert_eq!(
        synced["mcp_servers"]["tasks"],
        codex_after["mcp_servers"]["tasks"]
    );
    assert!(synced["mcp_servers"]["fetch"].is_object());
}

/// With a conflict, the literals of every origin pass the variable gate,
/// not only those of the `--from` origin: the other entry is rewritten to
/// read the same variable, so its own literal would be lost silently.
#[test]
fn the_variable_gate_checks_the_literals_of_every_origin() {
    let env = setup(
        json!({"api": {"command": "api-mcp", "env": {
            "DMCP_TEST_API_URL": "staging-4711", "DMCP_TEST_EXTRA": "claude-only-4711"}}}),
        "[mcp_servers.api]\ncommand = \"api-mcp\"\n\
         [mcp_servers.api.env]\nDMCP_TEST_API_URL = \"prod-4711\"\n",
    );
    let args = ["claude:user:api", "codex:user:api", "--from", "codex:user"];
    let vars = [("DMCP_TEST_API_URL", "prod-4711")];
    let (code, value, _) = adopt(&env, &vars, &args);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(server(&value, "api")["verdict"], "conflict");
    assert_eq!(
        refusals(&value),
        ["claude:user:api env.DMCP_TEST_API_URL variable-mismatch"]
    );
    let checks: Vec<(&str, &str)> = server(&value, "api")["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| {
            (
                check["origin"].as_str().unwrap(),
                check["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        checks,
        [("codex:user:api", "ok"), ("claude:user:api", "differs")]
    );
    // Claude Code's extra literal is not carried by Codex's core.
    let notices = server(&value, "api")["notices"].to_string();
    assert!(
        notices.contains(
            "`claude:user:api`: the adopted definition does not carry the literal of `env.DMCP_TEST_EXTRA`"
        ),
        "{notices}"
    );
    let text = value.to_string();
    for literal in ["staging-4711", "prod-4711", "claude-only-4711"] {
        assert!(!text.contains(literal), "{literal}: {value:#}");
    }
    // The shell matches Claude Code's literal instead: Codex's is refused.
    let (_, value, _) = adopt(&env, &[("DMCP_TEST_API_URL", "staging-4711")], &args);
    assert_eq!(
        refusals(&value),
        ["codex:user:api env.DMCP_TEST_API_URL variable-mismatch"]
    );
    // `--allow-env-mismatch` accepts it and says what is lost.
    let mut accepted: Vec<&str> = args.to_vec();
    accepted.push("--allow-env-mismatch");
    let (code, value, stderr) = adopt_apply(&env, &vars, &accepted);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    let notices = server(&value, "api")["notices"].to_string();
    assert!(
        notices.contains("`env.DMCP_TEST_API_URL` of claude:user:api is unrecoverable after apply"),
        "{notices}"
    );
    assert_eq!(
        claude_entry(&env, "api")["env"],
        json!({"DMCP_TEST_API_URL": "${DMCP_TEST_API_URL:-}"})
    );
    // Only the fields the definition carries are recorded.
    for harness in [Harness::Claude, Harness::Codex] {
        assert_eq!(
            meta(&env, harness, "api").replaced_fields,
            [replaced("env.DMCP_TEST_API_URL", "DMCP_TEST_API_URL")],
            "{harness:?}"
        );
    }
}

#[test]
fn a_project_origin_needs_a_present_harness_and_the_local_source() {
    let env = setup(json!({"plain": {"command": "plain-mcp"}}), "");
    fs::create_dir_all(env.work().join(".git")).unwrap();
    fs::create_dir_all(env.work().join(".codex")).unwrap();
    fs::write(
        env.work().join(".codex/config.toml"),
        "[mcp_servers.proj]\ncommand = \"proj-mcp\"\n",
    )
    .unwrap();
    // No Codex file and no `codex` on `PATH`: there is no user scope to add
    // the project entry to.
    fs::remove_file(env.codex_config()).unwrap();
    let (code, value, _) = adopt(&env, &[], &["codex:project:proj", "--to-user-scope"]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(
        refusals(&value),
        ["codex:project:proj  harness-not-present"]
    );
    // Without `local/servers`, the built-in source cannot take the
    // definition.
    fs::remove_dir_all(env.default_store().join("local/servers")).unwrap();
    let (code, value, _) = adopt(&env, &[], &["claude:user:plain"]);
    assert_eq!(code, 1, "{value:#}");
    assert_eq!(refusals(&value), ["-  local-unavailable"]);
}

#[test]
fn a_codex_project_entry_moves_to_the_user_scope() {
    let env = TestEnv::new();
    let bin = env.fake_claude_dir();
    let env = env.with_path(&[bin]);
    env.init();
    fs::create_dir_all(env.work().join(".git")).unwrap();
    fs::create_dir_all(env.work().join(".codex")).unwrap();
    let project = "[mcp_servers.proj]\ncommand = \"proj-mcp\"\n\
                   [mcp_servers.proj.env]\nDMCP_TEST_PROJ_TOKEN = \"proj-literal-4711\"\n";
    fs::write(env.work().join(".codex/config.toml"), project).unwrap();
    fs::write(
        env.codex_config(),
        format!(
            "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
            env.work().display()
        ),
    )
    .unwrap();
    let vars = [("DMCP_TEST_PROJ_TOKEN", "proj-literal-4711")];
    let args = ["codex:project:proj", "--to-user-scope"];
    let (code, value, stderr) = adopt(&env, &vars, &args);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    let entry = origin(&value, "proj", "codex:project:proj");
    assert_eq!(entry["write"], "add");
    assert_eq!(entry["scope"], "project");
    assert_eq!(entry["path"], env.work().to_str().unwrap());
    assert_eq!(codex_table(&env, "proj"), Value::Null);
    assert!(!value.to_string().contains("proj-literal-4711"));

    let (code, value, stderr) = adopt_apply(&env, &vars, &args);
    assert_eq!(code, 0, "{value:#}\n{stderr}");
    assert_eq!(
        origin(&value, "proj", "codex:project:proj")["outcome"],
        "applied"
    );
    assert_eq!(
        codex_table(&env, "proj"),
        json!({"command": "proj-mcp", "env_vars": ["DMCP_TEST_PROJ_TOKEN"]})
    );
    // The project layer is left as it is, and keeps shadowing.
    assert_eq!(
        fs::read_to_string(env.work().join(".codex/config.toml")).unwrap(),
        project
    );
    assert!(
        server(&value, "proj")["notices"]
            .to_string()
            .contains("still holds its literals")
    );
    let proj = meta(&env, Harness::Codex, "proj");
    assert_eq!(
        proj.adopted_from,
        adopted_from(Harness::Codex, OriginScope::Project { root: env.work() })
    );
    assert_eq!(
        proj.replaced_fields,
        [replaced("env.DMCP_TEST_PROJ_TOKEN", "DMCP_TEST_PROJ_TOKEN")]
    );
}
