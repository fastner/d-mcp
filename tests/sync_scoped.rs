//! The scoped sync that adoption runs (WP4): under a caller-held store lock,
//! limited to some servers (`only`), with take-overs bound to an expected
//! entry hash and the adoption provenance; and the vanish counter of the
//! write state. Driven through the library API against the Codex adapter.

mod common;

use std::collections::BTreeMap;
use std::fs;

use common::TestEnv;
use d_mcp::config::Harness;
use d_mcp::error::Error;
use d_mcp::state::{self, AdoptedFrom, OriginScope, RecordKey, ReplacedField};
use d_mcp::store::{StoreLock, StorePaths};
use d_mcp::sync::{BoundTakeOver, ExpectedHash, SyncOptions, sync, sync_under_lock};
use serde_json::Value;

fn setup(codex: &str) -> (TestEnv, StorePaths) {
    let env = TestEnv::new().without_codex();
    env.init();
    fs::write(env.codex_config(), codex).unwrap();
    let paths = StorePaths::new(env.default_store());
    (env, paths)
}

fn scoped(env: &TestEnv, paths: &StorePaths, options: &SyncOptions<'_>) -> (Value, Option<Error>) {
    let lock = StoreLock::acquire(paths).unwrap();
    let mut warnings = Vec::new();
    let (report, error) =
        sync_under_lock(paths, &env.environment(), &lock, options, &mut warnings).unwrap();
    (report.data, error)
}

fn names(data: &Value) -> Vec<String> {
    data["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|harness| harness["entries"].as_array().unwrap())
        .map(|entry| entry["name"].as_str().unwrap().to_owned())
        .collect()
}

fn key(name: &str) -> RecordKey {
    RecordKey {
        harness: Harness::Codex,
        scope: d_mcp::adapter::Scope::User,
        name: name.to_owned(),
    }
}

fn codex_only(mut options: SyncOptions<'_>) -> SyncOptions<'_> {
    options.harnesses.insert(Harness::Codex);
    options
}

#[test]
fn only_limits_planning_writes_and_records() {
    let (env, paths) = setup("");
    for name in ["a", "b"] {
        env.write_local(name, &common::stdio(name, "x"));
        env.cmd().args(["select", name]).assert().success();
    }
    // `b` has a write in flight that a scoped run must leave pending.
    let pending = format!(
        "schema_version = 1\n\n[[records]]\nharness = \"codex\"\nscope = \"user\"\nname = \"b\"\n\
         file = \"{}\"\nstatus = \"pending\"\nop = \"add\"\nsent = \"{h}\"\nreadback = \"{h}\"\n\
         definition_hash = \"{h}\"\nwritten = true\ntime = 1\nversion = \"0.1.0\"\n",
        env.codex_config().display(),
        h = "1".repeat(64)
    );
    fs::write(env.default_store().join("state.toml"), &pending).unwrap();
    let options = codex_only(SyncOptions {
        only: ["a".to_owned()].into(),
        ..SyncOptions::default()
    });
    let (data, error) = scoped(&env, &paths, &options);
    assert!(error.is_none(), "{data:#}");
    assert_eq!(names(&data), ["a"]);
    let text = fs::read_to_string(env.codex_config()).unwrap();
    assert!(
        text.contains("[mcp_servers.a]") && !text.contains("[mcp_servers.b]"),
        "{text}"
    );
    let after = state::load(&paths).unwrap();
    assert!(matches!(
        after.records[&key("b")].state,
        state::RecordState::Pending { .. }
    ));
    assert!(after.records[&key("a")].hashes().unwrap().written);
    // A plain sync then handles everything.
    let mut warnings = Vec::new();
    let (report, _) = sync(
        &paths,
        &env.environment(),
        &codex_only(SyncOptions::default()),
        &mut warnings,
    )
    .unwrap();
    assert_eq!(names(&report.data), ["a", "b"]);
}

#[test]
fn a_bound_take_over_refuses_a_changed_entry_before_journaling() {
    let codex = "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.a.env]\nT = \"lit\"\n";
    let (env, paths) = setup(codex);
    env.write_local("a", "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n[env]\nT = { forward = true }\n");
    env.cmd().args(["select", "a"]).assert().success();
    let take = |expected: &str| BoundTakeOver {
        expected: Some(ExpectedHash::new(expected)),
        adopted_from: AdoptedFrom {
            harness: Harness::Codex,
            scope: OriginScope::User,
        },
        replaced_fields: vec![ReplacedField {
            field: "env.T".to_owned(),
            var: "T".to_owned(),
        }],
    };
    let bound = |expected: &str| {
        let mut options = codex_only(SyncOptions {
            only: ["a".to_owned()].into(),
            ..SyncOptions::default()
        });
        options.bound = BTreeMap::from([(
            Harness::Codex,
            BTreeMap::from([("a".to_owned(), take(expected))]),
        )]);
        options
    };
    let (data, error) = scoped(&env, &paths, &bound(&"0".repeat(64)));
    let error = error.unwrap();
    assert_eq!(error.code(), "adopt-entry-changed");
    assert_eq!(error.exit_code(), 3);
    assert_eq!(data["harnesses"][0]["error"]["code"], "adopt-entry-changed");
    assert_eq!(fs::read_to_string(env.codex_config()).unwrap(), codex);
    assert!(state::load(&paths).unwrap().records.is_empty());
    assert!(format!("{:?}", take("x").expected).contains("<redacted>"));
    // With the entry's actual hash, it is taken over and rewritten, and the
    // record carries the adoption.
    let actual = d_mcp::adapter::codex::entry_hash(&serde_json::json!({
        "command": "x", "env": {"T": "lit"}
    }));
    let (data, error) = scoped(&env, &paths, &bound(&actual));
    assert!(error.is_none(), "{data:#}");
    assert_eq!(data["harnesses"][0]["entries"][0]["operation"], "update");
    let record = state::load(&paths).unwrap().records[&key("a")].clone();
    assert_eq!(
        record
            .meta
            .adopted_from
            .as_ref()
            .map(AdoptedFrom::label)
            .as_deref(),
        Some("codex:user")
    );
    assert_eq!(record.meta.replaced_fields[0].var, "T");
    assert!(
        !fs::read_to_string(env.codex_config())
            .unwrap()
            .contains("lit")
    );
}

#[test]
fn every_promoted_vanished_re_add_is_counted() {
    let (env, paths) = setup("");
    env.write_local("a", &common::stdio("a", "x"));
    env.cmd().args(["select", "a"]).assert().success();
    let run = || {
        let mut warnings = Vec::new();
        let (report, _) = sync(
            &paths,
            &env.environment(),
            &codex_only(SyncOptions::default()),
            &mut warnings,
        )
        .unwrap();
        report.data
    };
    run();
    for expected in [1, 2] {
        fs::write(env.codex_config(), "").unwrap();
        let data = run();
        assert_eq!(data["harnesses"][0]["entries"][0]["operation"], "vanished");
        let meta = state::load(&paths).unwrap().records[&key("a")].meta.clone();
        assert_eq!(meta.vanish_count, expected);
        assert!(meta.last_vanish.is_some());
    }
    // An ordinary update keeps the history.
    env.write_local("a", &common::stdio("a", "y"));
    let data = run();
    assert_eq!(data["harnesses"][0]["entries"][0]["operation"], "update");
    assert_eq!(
        state::load(&paths).unwrap().records[&key("a")]
            .meta
            .vanish_count,
        2
    );
}
