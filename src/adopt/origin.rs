//! One origin entry of an adoption: where it lives, its import, its lossless
//! round trip, and what adopting it does to the user-scope entry of its
//! harness.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use super::{Context, EntryRef, Refusal, ScopeKind};
use crate::adapter::{Adapter, DiscoveredScope, Gap, Imported, RawEntry, ScopeRead, adapter_for};
use crate::error::Error;
use crate::redact::{EntryView, safe_key};
use crate::state::ReplacedField;

/// What adopting an entry writes to its harness's user scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WriteMode {
    /// The rendering equals the entry: it is taken over without a write.
    None,
    /// The entry is rewritten in reference form.
    Update,
    /// A project-scope origin: a new user-scope entry is added.
    Add,
}

impl WriteMode {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WriteMode::None => "none",
            WriteMode::Update => "update",
            WriteMode::Add => "add",
        }
    }
}

/// The plan of one origin entry.
pub struct OriginPlan {
    /// The entry as given on the command line.
    pub entry: EntryRef,
    /// The repository root of a project-scope origin.
    pub path: Option<PathBuf>,
    /// The file the origin lives in.
    pub file: Option<PathBuf>,
    /// The keyed display hash of the origin entry (with a store key).
    pub display_hash: Option<String>,
    /// What the adoption writes to the user scope.
    pub write: Option<WriteMode>,
    /// The redacted view of the origin entry.
    pub before: Option<EntryView>,
    /// The redacted view of the rendering that replaces (or joins) it.
    pub after: Option<EntryView>,
    /// The native fields that differ between the two, names only.
    pub changed: Vec<String>,
    /// The `note` gaps of the rendering.
    pub gaps: Vec<Gap>,
    /// The fields whose literals the adopted definition replaces here.
    pub replaced_fields: Vec<ReplacedField>,
    /// The outcome of the scoped sync, after an apply.
    pub outcome: Option<String>,
    /// The error of the scoped sync, after an apply: `code` and `hint`.
    pub error: Option<Value>,
    /// The plain entry hash; never output.
    pub(super) hash: Option<String>,
    /// The origin entry value; never output, only through `redact`.
    pub(super) raw: Option<RawEntry>,
    /// The import, when it succeeded and round-trips.
    pub(super) imported: Option<Imported>,
}

impl OriginPlan {
    fn new(entry: &EntryRef) -> Self {
        Self {
            entry: entry.clone(),
            path: None,
            file: None,
            display_hash: None,
            write: None,
            before: None,
            after: None,
            changed: Vec::new(),
            gaps: Vec::new(),
            replaced_fields: Vec::new(),
            outcome: None,
            error: None,
            hash: None,
            raw: None,
            imported: None,
        }
    }

    /// `<harness>:<scope>:<name>`.
    #[must_use]
    pub fn label(&self) -> String {
        self.entry.to_string()
    }

    /// The origin's adapter.
    pub(super) fn adapter(&self) -> &'static dyn Adapter {
        adapter_for(self.entry.place.harness)
            .unwrap_or_else(|| unreachable!("every harness on the command line has an adapter"))
    }
}

/// The scope read an entry reference points at: the user scope, or the
/// deepest project layer that holds the name (Codex merges its layers, the
/// deepest wins), else the deepest project layer.
fn locate<'r>(reads: &'r [ScopeRead], entry: &EntryRef) -> Option<&'r ScopeRead> {
    match entry.place.scope {
        ScopeKind::User => reads
            .iter()
            .find(|read| read.scope == DiscoveredScope::User),
        ScopeKind::Project => {
            let projects = || {
                reads
                    .iter()
                    .rev()
                    .filter(|read| matches!(read.scope, DiscoveredScope::Project { .. }))
            };
            projects()
                .find(|read| read.entries.contains_key(&entry.name))
                .or_else(|| projects().next())
        }
        ScopeKind::Local => None,
    }
}

/// Plans one origin: locates and imports it and checks the round trip.
/// Refusals go to `refusals`.
pub(super) fn plan_origin(
    context: &Context<'_>,
    reads: &[ScopeRead],
    entry: &EntryRef,
    refusals: &mut Vec<Refusal>,
) -> OriginPlan {
    let mut plan = OriginPlan::new(entry);
    let label = Some(plan.label());
    if entry.place.scope == ScopeKind::Local {
        // Local-scope values are never read (ADR 0005).
        refusals.push(Refusal::new(label, "", "local-scope"));
        return plan;
    }
    let Some(read) = locate(reads, entry) else {
        refusals.push(Refusal::new(label, "", "not-found"));
        return plan;
    };
    plan.file = Some(read.file.clone());
    plan.path = read.scope.path().map(PathBuf::from);
    if entry.place.scope == ScopeKind::Project && !context.options.to_user_scope {
        refusals.push(Refusal::new(label.clone(), "", "project-origin"));
    }
    let adapter = plan.adapter();
    let imported = match adapter.import(read, &entry.name) {
        Ok(imported) => imported,
        Err(gaps) => {
            refusals.extend(
                gaps.into_iter()
                    .map(|gap| Refusal::new(label.clone(), gap.field, gap.reason.as_str())),
            );
            return plan;
        }
    };
    // The import succeeded, so the entry has a value and a hash.
    let Some(discovered) = read.entries.get(&entry.name) else {
        refusals.push(Refusal::new(label, "", "not-found"));
        return plan;
    };
    plan.hash.clone_from(&discovered.hash);
    plan.display_hash = discovered
        .hash
        .as_deref()
        .and_then(|hash| context.key.as_ref().map(|key| key.display_hash(hash)));
    let Some(raw) = discovered.raw.clone() else {
        refusals.push(Refusal::new(label, "", "not-an-object"));
        return plan;
    };
    if entry.place.scope == ScopeKind::Project {
        relative_paths(&raw, &label, refusals);
    }
    let normalized = adapter.normalized_entry(&raw, &imported);
    if adapter.entry_hash(&normalized) != adapter.render(&imported.definition).sent_hash {
        refusals.push(Refusal::new(label, "", "round-trip"));
    } else {
        plan.imported = Some(imported);
    }
    plan.raw = Some(raw);
    plan
}

/// A project-scope entry moves to the user scope, where a relative path in
/// `args` would point elsewhere: such arguments are refused.
fn relative_paths(raw: &RawEntry, label: &Option<String>, refusals: &mut Vec<Refusal>) {
    let args = raw.value().get("args").and_then(Value::as_array);
    for (index, arg) in args.into_iter().flatten().enumerate() {
        if arg
            .as_str()
            .is_some_and(|arg| arg.starts_with("./") || arg.starts_with("../"))
        {
            refusals.push(Refusal::new(
                label.clone(),
                format!("args[{index}]"),
                "relative-path",
            ));
        }
    }
}

/// The native fields that differ between two canonical entry values: top
/// level keys, and the keys inside object values. Names only.
pub(super) fn changed_fields(before: &Value, after: &Value) -> Vec<String> {
    let (Some(before), Some(after)) = (before.as_object(), after.as_object()) else {
        return if before == after {
            Vec::new()
        } else {
            vec![String::new()]
        };
    };
    let keys: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let mut changed = Vec::new();
    for key in keys {
        let shown = safe_key(key);
        match (before.get(key), after.get(key)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(Value::Object(a)), Some(Value::Object(b))) => {
                let subs: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
                for sub in subs.into_iter().filter(|sub| a.get(*sub) != b.get(*sub)) {
                    changed.push(format!("{shown}.{}", safe_key(sub)));
                }
            }
            _ => changed.push(shown.into_owned()),
        }
    }
    changed
}

/// The error of a missing harness CLI that a write needs.
pub(super) fn cli_missing(harness: crate::config::Harness) -> Error {
    Error::HarnessCliNotFound {
        harness: harness.as_str(),
        program: crate::adapter::claude::PROGRAM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changed_fields_name_keys_and_sub_keys() {
        let before = json!({"command": "x", "env": {"A": "1", "B": "2"}, "args": ["a"]});
        let after = json!({"command": "x", "env": {"A": "${A:-}", "B": "2"}, "type": "stdio"});
        assert_eq!(changed_fields(&before, &after), ["args", "env.A", "type"]);
        assert!(changed_fields(&json!(1), &json!(1)).is_empty());
        assert_eq!(changed_fields(&json!(1), &json!({})), [""]);
        assert_eq!(WriteMode::Add.as_str(), "add");
    }
}
