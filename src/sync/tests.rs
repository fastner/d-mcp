//! Unit tests of the pure planner, without files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::*;
use crate::adapter::{
    AdapterTexts, ApplyContext, ApplyMode, ApplyResult, HarnessPlan, Hooks, Journal, Location,
    LoginState, OverrideKey, PrivateRead, Scope, foreign_security_gaps,
};
use crate::config::Harness;
use crate::definition::{Definition, parse_definition};
use crate::error::Result;
use crate::process::ProcessRunner;
use crate::state::RecordState;
use crate::store::Environment;

/// A planner test adapter: hashes are not comparable, and the Codex
/// `disabled_tools` override is security-relevant for it.
struct TestAdapter {
    comparable: bool,
}

impl Adapter for TestAdapter {
    fn harness(&self) -> Harness {
        Harness::Claude
    }
    fn apply_mode(&self) -> ApplyMode {
        ApplyMode::HarnessCli
    }
    fn discover_scopes(&self) -> &'static [Scope] {
        &[Scope::User]
    }
    fn write_scopes(&self) -> &'static [Scope] {
        &[Scope::User]
    }
    fn hashes_comparable(&self) -> bool {
        self.comparable
    }
    fn override_keys(&self) -> &'static [OverrideKey] {
        &[]
    }
    fn excluded_override_keys(&self) -> &'static [&'static str] {
        &[]
    }
    fn foreign_security_keys(&self) -> &'static [(Harness, &'static str)] {
        &[(Harness::Codex, "disabled_tools")]
    }
    fn reserved_names(&self) -> &'static [&'static str] {
        &[]
    }
    fn entries_can_vanish(&self) -> bool {
        false
    }
    fn verified_versions(&self) -> &'static [&'static str] {
        &[]
    }
    fn texts(&self) -> &'static AdapterTexts {
        &crate::adapter::claude::TEXTS
    }
    fn locate(&self, _env: &Environment, scope: Scope) -> Result<Location> {
        Ok(location(scope))
    }
    fn read(&self, location: &Location) -> Result<HarnessRead> {
        Ok(read(location.clone(), BTreeMap::new()))
    }
    fn render(&self, definition: &Definition) -> Rendered {
        let mut gaps = Vec::new();
        gaps.extend(foreign_security_gaps(self, definition));
        Rendered {
            entry: Vec::new(),
            sent_hash: hash(&definition.definition_hash()[..1]),
            gaps,
            secret_env: None,
        }
    }
    fn apply(
        &self,
        _ctx: &ApplyContext<'_>,
        _read: &HarnessRead,
        _plan: &HarnessPlan,
        _journal: &mut dyn Journal,
        _hooks: &Hooks<'_>,
    ) -> Result<ApplyResult> {
        Ok(ApplyResult::default())
    }
    fn login_state(
        &self,
        _env: &Environment,
        _runner: &ProcessRunner,
        names: &[String],
    ) -> BTreeMap<String, LoginState> {
        names
            .iter()
            .map(|name| (name.clone(), LoginState::Unknown))
            .collect()
    }
}

fn hash(seed: &str) -> String {
    seed.repeat(64)[..64].to_owned()
}

fn location(scope: Scope) -> Location {
    Location {
        scope,
        file: PathBuf::from("/c/config.toml"),
        file_exists: true,
        binary_found: false,
    }
}

fn read(location: Location, entries: BTreeMap<String, ActualEntry>) -> HarnessRead {
    HarnessRead {
        location,
        entries,
        root_supported: true,
        private: PrivateRead::default(),
        shadowed: BTreeMap::new(),
    }
}

fn actual(seed: &str) -> ActualEntry {
    ActualEntry {
        hash: hash(seed),
        supported_style: true,
        preserved: BTreeSet::new(),
    }
}

fn owned(sent: &str, readback: &str) -> Record {
    Record::owned(
        PathBuf::from("/c/config.toml"),
        Hashes {
            sent: hash(sent),
            readback: hash(readback),
            definition_hash: hash("d"),
            written: true,
        },
    )
}

fn render(sent: &str) -> Desired {
    Desired::Render {
        rendered: Rendered {
            entry: Vec::new(),
            sent_hash: hash(sent),
            gaps: Vec::new(),
            secret_env: None,
        },
        definition_hash: hash("d"),
    }
}

struct Case {
    entries: BTreeMap<String, ActualEntry>,
    records: BTreeMap<String, Record>,
    desired: BTreeMap<String, Desired>,
    take_over: BTreeSet<String>,
    relocate: bool,
    root_supported: bool,
    file: PathBuf,
}

impl Case {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            records: BTreeMap::new(),
            desired: BTreeMap::new(),
            take_over: BTreeSet::new(),
            relocate: false,
            root_supported: true,
            file: PathBuf::from("/c/config.toml"),
        }
    }

    fn actual(mut self, name: &str, entry: ActualEntry) -> Self {
        self.entries.insert(name.to_owned(), entry);
        self
    }

    fn record(mut self, name: &str, record: Record) -> Self {
        self.records.insert(name.to_owned(), record);
        self
    }

    fn desire(mut self, name: &str, desired: Desired) -> Self {
        self.desired.insert(name.to_owned(), desired);
        self
    }

    fn plan_with(&self, adapter: &dyn Adapter) -> Planned {
        let mut harness_read = read(
            Location {
                file: self.file.clone(),
                ..location(Scope::User)
            },
            self.entries.clone(),
        );
        harness_read.root_supported = self.root_supported;
        plan_harness(&PlanInput {
            adapter,
            read: &harness_read,
            records: &self.records,
            desired: &self.desired,
            take_over: &self.take_over,
            relocate: self.relocate,
        })
    }

    fn plan(&self) -> Planned {
        self.plan_with(&TestAdapter { comparable: true })
    }

    fn one(&self) -> PlannedEntry {
        let planned = self.plan();
        assert_eq!(planned.entries.len(), 1, "{planned:?}");
        planned.entries[0].clone()
    }
}

#[test]
fn add_update_remove_unchanged() {
    let add = Case::new().desire("a", render("1")).one();
    assert_eq!(add.operation, Operation::Add);
    assert_eq!(add.write, Some(WriteKind::Add));
    assert!(add.rendered.is_some());

    let update = Case::new()
        .desire("a", render("2"))
        .record("a", owned("1", "r"))
        .actual("a", actual("r"))
        .one();
    assert_eq!(update.operation, Operation::Update);
    assert_eq!(update.write, Some(WriteKind::Update));
    assert_eq!(update.owned_hash, Some(hash("r")));

    let remove = Case::new()
        .record("a", owned("1", "r"))
        .actual("a", actual("r"))
        .one();
    assert_eq!(remove.operation, Operation::Remove);
    assert_eq!(remove.write, Some(WriteKind::Remove));

    let unchanged = Case::new()
        .desire("a", render("1"))
        .record("a", owned("1", "r"))
        .actual("a", actual("r"))
        .one();
    assert_eq!(unchanged.operation, Operation::Unchanged);
    assert_eq!(unchanged.record, RecordChange::Keep);
}

#[test]
fn a_new_definition_hash_with_the_same_rendering_refreshes_the_record() {
    let mut case = Case::new()
        .record("a", owned("1", "r"))
        .actual("a", actual("r"));
    case = case.desire(
        "a",
        Desired::Render {
            rendered: Rendered {
                entry: Vec::new(),
                sent_hash: hash("1"),
                gaps: Vec::new(),
                secret_env: None,
            },
            definition_hash: hash("e"),
        },
    );
    let entry = case.one();
    assert_eq!(entry.operation, Operation::Unchanged);
    let RecordChange::Set(record) = entry.record else {
        panic!("expected a refreshed record");
    };
    assert_eq!(record.hashes().unwrap().definition_hash, hash("e"));
    assert_eq!(record.file, PathBuf::from("/c/config.toml"));
}

#[test]
fn drift_collision_and_identical_collision() {
    let drift = Case::new()
        .desire("a", render("1"))
        .record("a", owned("1", "r"))
        .actual("a", actual("x"))
        .one();
    assert_eq!(drift.operation, Operation::Drift);
    assert!(drift.operation.blocks());
    assert_eq!(drift.write, None);
    let drift_unselected = Case::new()
        .record("a", owned("1", "r"))
        .actual("a", actual("x"))
        .one();
    assert_eq!(drift_unselected.operation, Operation::Drift);

    let collision = Case::new()
        .desire("a", render("1"))
        .actual("a", actual("x"))
        .one();
    assert_eq!(collision.operation, Operation::Collision);
    assert_eq!(collision.owned_hash, None);

    let identical = Case::new()
        .desire("a", render("1"))
        .actual("a", actual("1"));
    assert_eq!(identical.one().operation, Operation::IdenticalCollision);
    // Without comparable hashes an identical rendering is just a collision.
    let planned = identical.plan_with(&TestAdapter { comparable: false });
    assert_eq!(planned.entries[0].operation, Operation::Collision);
}

#[test]
fn take_over_accepts_the_actual_entry_then_plans_normally() {
    let mut drift = Case::new()
        .desire("a", render("1"))
        .record("a", owned("1", "r"))
        .actual("a", actual("x"));
    drift.take_over.insert("a".to_owned());
    let entry = drift.one();
    assert!(entry.taken_over);
    assert_eq!(entry.operation, Operation::Update);
    assert!(entry.desired);
    // The write journals the taken-over hashes as its previous ones, not the
    // stored record, so a failed write keeps the take-over.
    let previous = entry.previous.unwrap();
    assert_eq!(previous.readback, hash("x"));
    assert_eq!(previous.sent, hash("x"));

    // A take-over of a collision that leads to an Update has previous
    // hashes too, although nothing was recorded before.
    let mut collision = Case::new()
        .desire("a", render("1"))
        .actual("a", actual("x"));
    collision.take_over.insert("a".to_owned());
    let entry = collision.one();
    assert_eq!(entry.operation, Operation::Update);
    assert_eq!(entry.record, RecordChange::Keep);
    assert_eq!(entry.previous.unwrap().readback, hash("x"));

    let mut identical = Case::new()
        .desire("a", render("1"))
        .actual("a", actual("1"));
    identical.take_over.insert("a".to_owned());
    let entry = identical.one();
    assert_eq!(entry.operation, Operation::Unchanged);
    let RecordChange::Set(record) = entry.record else {
        panic!("a take-over is recorded");
    };
    assert_eq!(record.hashes().unwrap().readback, hash("1"));

    // A take-over of an absent entry changes nothing.
    let mut absent = Case::new().desire("a", render("1"));
    absent.take_over.insert("a".to_owned());
    let entry = absent.one();
    assert_eq!(entry.operation, Operation::Add);
    assert!(!entry.taken_over);

    // A held, unrecorded server has no definition hash to take over.
    let mut held = Case::new()
        .desire("a", Desired::Held(HeldReason::InvalidWinner))
        .actual("a", actual("1"));
    held.take_over.insert("a".to_owned());
    assert!(held.plan().entries.is_empty());

    // A drifted, unselected entry keeps its recorded definition hash.
    let mut unselected = Case::new()
        .record("a", owned("1", "r"))
        .actual("a", actual("x"));
    unselected.take_over.insert("a".to_owned());
    let entry = unselected.one();
    assert_eq!(entry.operation, Operation::Remove);
    assert!(!entry.desired);
    assert_eq!(entry.previous.unwrap().readback, hash("x"));
}

#[test]
fn vanished_held_orphaned() {
    let vanished = Case::new()
        .desire("a", render("1"))
        .record("a", owned("1", "r"))
        .one();
    assert_eq!(vanished.operation, Operation::Vanished);
    assert_eq!(vanished.write, Some(WriteKind::Add));

    for reason in [
        HeldReason::InvalidWinner,
        HeldReason::SourceUnavailable,
        HeldReason::NotOffered,
    ] {
        let held = Case::new()
            .desire("a", Desired::Held(reason))
            .record("a", owned("1", "r"))
            .actual("a", actual("x"))
            .one();
        assert_eq!(held.operation, Operation::Held);
        assert_eq!(held.reason, Some(reason));
        assert_eq!(held.write, None);
        assert!(!held.operation.blocks());
        assert!(!reason.as_str().is_empty());
    }
    let unrecorded = Case::new().desire("a", Desired::Held(HeldReason::NotOffered));
    assert!(unrecorded.plan().entries.is_empty());

    let orphaned = Case::new().record("a", owned("1", "r")).one();
    assert_eq!(orphaned.operation, Operation::Orphaned);
    assert_eq!(orphaned.record, RecordChange::Drop);
    assert_eq!(orphaned.owned_hash, None);
}

#[test]
fn location_change_and_relocate() {
    let mut moved = Case::new()
        .desire("a", render("1"))
        .record("a", owned("1", "r"))
        .actual("a", actual("r"));
    moved.file = PathBuf::from("/other/config.toml");
    let planned = moved.plan();
    assert_eq!(planned.status, PlanStatus::LocationChanged);
    assert_eq!(planned.entries[0].operation, Operation::LocationChanged);
    assert!(planned.entries[0].operation.blocks());

    moved.relocate = true;
    let planned = moved.plan();
    assert_eq!(planned.status, PlanStatus::Ready);
    assert_eq!(
        planned.relocated_from,
        BTreeSet::from([PathBuf::from("/c/config.toml")])
    );
    let RecordChange::Set(record) = &planned.entries[0].record else {
        panic!("the relocated record is persisted");
    };
    assert_eq!(record.file, PathBuf::from("/other/config.toml"));
    assert_eq!(planned.entries[0].operation, Operation::Unchanged);

    // A relocated record that leads to a write journals its hashes as the
    // previous ones.
    let mut update = Case::new()
        .desire("a", render("2"))
        .record("a", owned("1", "r"))
        .actual("a", actual("r"));
    update.file = PathBuf::from("/other/config.toml");
    update.relocate = true;
    let entry = update.one();
    assert_eq!(entry.operation, Operation::Update);
    assert_eq!(entry.previous.unwrap().readback, hash("r"));
}

#[test]
fn unsupported_root_and_entry_style() {
    let mut root = Case::new().desire("a", render("1"));
    root.root_supported = false;
    let planned = root.plan();
    assert_eq!(planned.status, PlanStatus::UnsupportedRoot);
    assert!(planned.entries.is_empty());

    let inline = ActualEntry {
        supported_style: false,
        ..actual("r")
    };
    let update = Case::new()
        .desire("a", render("2"))
        .record("a", owned("1", "r"))
        .actual("a", inline.clone())
        .one();
    assert_eq!(update.operation, Operation::Unsupported);
    let remove = Case::new()
        .record("a", owned("1", "r"))
        .actual("a", inline)
        .one();
    assert_eq!(remove.operation, Operation::Unsupported);
}

#[test]
fn an_important_gap_blocks_the_entry() {
    let adapter = TestAdapter { comparable: false };
    let definition = parse_definition(
        "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
         [harness.codex]\ndisabled_tools = [\"t\"]\n",
        "a",
    )
    .unwrap();
    let rendered = adapter.render(&definition);
    assert_eq!(rendered.gaps.len(), 1);
    assert_eq!(rendered.gaps[0].field, "harness.codex.disabled_tools");
    let case = Case::new().desire(
        "a",
        Desired::Render {
            rendered,
            definition_hash: definition.definition_hash(),
        },
    );
    let entry = case.plan_with(&adapter).entries[0].clone();
    assert_eq!(entry.operation, Operation::Blocked);
    assert_eq!(entry.write, None);
    assert!(entry.operation.blocks());
    assert_eq!(entry.gaps[0].severity, GapSeverity::Important);
    // The test adapter's other declarations.
    let env = Environment::default();
    assert!(adapter.locate(&env, Scope::User).unwrap().file_exists);
    let read = adapter.read(&location(Scope::User)).unwrap();
    assert!(read.entries.is_empty());
    assert_eq!(
        adapter.login_state(&env, &ProcessRunner::default(), &["a".to_owned()])["a"],
        LoginState::Unknown
    );
    assert_eq!(adapter.apply_mode(), ApplyMode::HarnessCli);
    assert!(adapter.import(&read, "a").is_none());
}

#[test]
fn a_note_gap_is_dropped_once_its_preserved_key_is_present() {
    let gap = Gap {
        field: "auth.client_secret_env".to_owned(),
        severity: GapSeverity::Note,
        reason: "manual step".to_owned(),
        unless_present: Some("oauth.client_secret"),
    };
    let desired = Desired::Render {
        rendered: Rendered {
            entry: Vec::new(),
            sent_hash: hash("1"),
            gaps: vec![gap],
            secret_env: None,
        },
        definition_hash: hash("d"),
    };
    let missing = Case::new()
        .desire("a", desired.clone())
        .record("a", owned("1", "r"))
        .actual("a", actual("r"))
        .one();
    assert_eq!(missing.gaps.len(), 1);
    let mut with_secret = actual("r");
    with_secret.preserved.insert("oauth.client_secret");
    let present = Case::new()
        .desire("a", desired)
        .record("a", owned("1", "r"))
        .actual("a", with_secret)
        .one();
    assert!(present.gaps.is_empty());
}

#[test]
fn an_update_reports_preserved_keys_that_no_longer_fit() {
    let codex = crate::adapter::codex::CodexAdapter;
    let rendered = |entry: Vec<(String, serde_json::Value)>, gaps: Vec<Gap>| Desired::Render {
        rendered: Rendered {
            entry,
            sent_hash: hash("2"),
            gaps,
            secret_env: None,
        },
        definition_hash: hash("d"),
    };
    let manual = Gap {
        field: "auth.client_secret_env".to_owned(),
        severity: GapSeverity::Note,
        reason: "manual step".to_owned(),
        unless_present: Some("oauth.client_secret"),
    };
    let mut present = actual("r");
    present
        .preserved
        .extend(["oauth.client_secret", "oauth.callback_url"]);
    let plan = |desired: Desired, actual: ActualEntry| {
        let case = Case::new()
            .desire("a", desired)
            .record("a", owned("1", "r"))
            .actual("a", actual);
        case.plan_with(&codex).entries[0].clone()
    };
    let fields = |entry: &PlannedEntry| -> Vec<String> {
        entry.gaps.iter().map(|gap| gap.field.clone()).collect()
    };

    // A switch to stdio drops both keys, by name only.
    let stdio = plan(
        rendered(
            vec![("command".to_owned(), serde_json::json!("npx"))],
            Vec::new(),
        ),
        present.clone(),
    );
    assert_eq!(stdio.operation, Operation::Update);
    assert_eq!(
        fields(&stdio),
        ["oauth.callback_url", "oauth.client_secret"]
    );
    assert!(
        stdio
            .gaps
            .iter()
            .all(|gap| gap.severity == GapSeverity::Note)
    );
    assert_eq!(
        stdio.gaps[1].reason,
        "oauth.client_secret removed: no longer used by this definition"
    );

    // OAuth without a client ID keeps the callback but drops the secret, so
    // the secret no longer satisfies the manual step either.
    let no_id = plan(
        rendered(
            vec![("url".to_owned(), serde_json::json!("https://x.test/mcp"))],
            vec![manual.clone()],
        ),
        present.clone(),
    );
    assert_eq!(
        fields(&no_id),
        ["auth.client_secret_env", "oauth.client_secret"]
    );

    // A compatible rendering keeps both keys and reports nothing.
    let compatible = plan(
        rendered(
            vec![
                ("url".to_owned(), serde_json::json!("https://x.test/mcp")),
                ("oauth".to_owned(), serde_json::json!({"client_id": "cid"})),
            ],
            vec![manual],
        ),
        present.clone(),
    );
    assert!(compatible.gaps.is_empty(), "{compatible:?}");

    // Only an Update removes keys: an unchanged entry reports nothing.
    let unchanged = Case::new()
        .desire(
            "a",
            rendered(
                vec![("command".to_owned(), serde_json::json!("npx"))],
                Vec::new(),
            ),
        )
        .record("a", owned("2", "r"))
        .actual("a", present);
    let entry = unchanged.plan_with(&codex).entries[0].clone();
    assert_eq!(entry.operation, Operation::Unchanged);
    assert!(entry.gaps.is_empty());
}

#[test]
fn operation_names_are_stable() {
    let all = [
        (Operation::Add, "add"),
        (Operation::Update, "update"),
        (Operation::Remove, "remove"),
        (Operation::Drift, "drift"),
        (Operation::Collision, "collision"),
        (Operation::IdenticalCollision, "identical-collision"),
        (Operation::Vanished, "vanished"),
        (Operation::Held, "held"),
        (Operation::Orphaned, "orphaned"),
        (Operation::LocationChanged, "location-changed"),
        (Operation::Unchanged, "unchanged"),
        (Operation::Blocked, "blocked"),
        (Operation::Unsupported, "unsupported"),
    ];
    for (operation, name) in all {
        assert_eq!(operation.as_str(), name);
        assert_eq!(serde_json::to_value(operation).unwrap(), name);
    }
    // A pending record is never planned as owned.
    let pending = Record {
        state: RecordState::Pending {
            op: WriteKind::Add,
            expected: None,
            previous: None,
        },
        ..owned("1", "r")
    };
    assert_eq!(pending.hashes(), None);
}
