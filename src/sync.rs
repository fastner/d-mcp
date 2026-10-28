//! The sync engine: a pure three-way planner over desired, recorded and
//! actual entries, and (in `executor`) the executor that applies a plan
//! through an adapter, verifies it and promotes or reverts the write state.
//! `report` holds the report model and its rendering, `unmanage` the
//! `unmanage` command.
//!
//! Operations per harness and entry:
//!
//! | Operation | When |
//! |---|---|
//! | Add | desired, not recorded, absent |
//! | Update | the new rendering differs from the recorded sent hash, and the actual hash equals the recorded read-back hash |
//! | Remove | not desired, recorded, actual hash equals the recorded read-back hash |
//! | Drift | recorded, actual hash differs (reported only) |
//! | Collision | desired, not recorded, a same-name entry exists (reported only) |
//! | Identical collision | a collision whose actual hash equals the rendering (comparable hashes only) |
//! | Vanished | recorded, desired, absent: re-added with a notice |
//! | Held | the resolver holds the server, or a recorded, still-selected server is not offered: the entry is left unchanged |
//! | Orphaned | recorded, not desired, absent: the record is dropped |
//! | Location changed | the recorded file differs from the resolved one |
//! | Unchanged | nothing to do (a changed definition hash only refreshes the record) |
//!
//! The planner never compares a rendered entry with the read-back entry
//! directly, except for identical-collision detection in adapters that
//! declare the two hashes comparable.

mod executor;
mod report;
mod unmanage;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::adapter::{ActualEntry, Adapter, Gap, GapSeverity, HarnessRead, Rendered, WriteKind};
use crate::state::{Hashes, Record};

pub use executor::{SyncHooks, SyncOptions, sync};
pub use unmanage::unmanage;

/// Why a server is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeldReason {
    /// The highest-priority definition is invalid.
    InvalidWinner,
    /// An unavailable source could change the result.
    SourceUnavailable,
    /// No source offers the server any more.
    NotOffered,
}

impl HeldReason {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HeldReason::InvalidWinner => "invalid-winner",
            HeldReason::SourceUnavailable => "source-unavailable",
            HeldReason::NotOffered => "not-offered",
        }
    }
}

/// What the resolver wants for a selected server in one harness.
#[derive(Debug, Clone, PartialEq)]
pub enum Desired {
    /// A rendered entry.
    Render {
        /// The rendering.
        rendered: Rendered,
        /// The source definition hash.
        definition_hash: String,
    },
    /// The server is held.
    Held(HeldReason),
}

/// A sync operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    /// Add a new entry.
    Add,
    /// Replace an owned entry.
    Update,
    /// Remove an owned entry.
    Remove,
    /// An owned entry was changed outside d-mcp.
    Drift,
    /// An unmanaged entry has the same name.
    Collision,
    /// An unmanaged entry has the same name and the same content.
    IdenticalCollision,
    /// An owned entry was removed outside d-mcp; it is re-added.
    Vanished,
    /// The server is held; the entry is left unchanged.
    Held,
    /// An owned entry is gone and no longer desired; the record is dropped.
    Orphaned,
    /// The recorded file differs from the resolved one.
    LocationChanged,
    /// Nothing to do.
    Unchanged,
    /// An `important` gap blocks the entry.
    Blocked,
    /// The owned entry is written in a style d-mcp does not edit.
    Unsupported,
}

impl Operation {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Add => "add",
            Operation::Update => "update",
            Operation::Remove => "remove",
            Operation::Drift => "drift",
            Operation::Collision => "collision",
            Operation::IdenticalCollision => "identical-collision",
            Operation::Vanished => "vanished",
            Operation::Held => "held",
            Operation::Orphaned => "orphaned",
            Operation::LocationChanged => "location-changed",
            Operation::Unchanged => "unchanged",
            Operation::Blocked => "blocked",
            Operation::Unsupported => "unsupported",
        }
    }

    /// Whether the operation blocks the entry (exit 1).
    #[must_use]
    pub fn blocks(self) -> bool {
        matches!(
            self,
            Operation::Drift
                | Operation::Collision
                | Operation::IdenticalCollision
                | Operation::LocationChanged
                | Operation::Blocked
                | Operation::Unsupported
        )
    }
}

/// How the record of an entry changes without a harness write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordChange {
    /// No change.
    Keep,
    /// Set the record (take-over, refresh, relocate).
    Set(Box<Record>),
    /// Drop the record (orphaned).
    Drop,
}

/// The plan for one entry.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedEntry {
    /// The entry name.
    pub name: String,
    /// The operation.
    pub operation: Operation,
    /// The write, for Add, Update, Remove and Vanished.
    pub write: Option<WriteKind>,
    /// The rendering, for writes that need one.
    pub rendered: Option<Rendered>,
    /// The definition hash of the rendering.
    pub definition_hash: Option<String>,
    /// The gaps of the rendering.
    pub gaps: Vec<Gap>,
    /// The hold reason.
    pub reason: Option<HeldReason>,
    /// Whether `--take-over` accepted the actual entry.
    pub taken_over: bool,
    /// The record change without a write.
    pub record: RecordChange,
    /// The owned read-back hash to show, if the entry is owned.
    pub owned_hash: Option<String>,
    /// The owned hashes the entry was planned from: the recorded ones, or
    /// the effective ones after a take-over or relocation. A write journals
    /// them as its previous hashes, so a failed or interrupted write reverts
    /// to them and never loses a take-over.
    pub previous: Option<Hashes>,
    /// Whether the resolver wants a rendered entry for this name.
    pub desired: bool,
}

impl PlannedEntry {
    fn new(name: &str, operation: Operation) -> Self {
        Self {
            name: name.to_owned(),
            operation,
            write: None,
            rendered: None,
            definition_hash: None,
            gaps: Vec::new(),
            reason: None,
            taken_over: false,
            record: RecordChange::Keep,
            owned_hash: None,
            previous: None,
            desired: false,
        }
    }
}

/// The input of [`plan_harness`].
pub struct PlanInput<'a> {
    /// The adapter.
    pub adapter: &'a dyn Adapter,
    /// What was read; its location is the resolved one.
    pub read: &'a HarnessRead,
    /// The (settled) records of this harness and scope, by name.
    pub records: &'a BTreeMap<String, Record>,
    /// What the resolver wants, by name, for servers selected for this
    /// harness.
    pub desired: &'a BTreeMap<String, Desired>,
    /// Names to take over.
    pub take_over: &'a BTreeSet<String>,
    /// Whether `--relocate` was given.
    pub relocate: bool,
}

/// The harness-level outcome of planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    /// Entries were planned.
    Ready,
    /// The recorded file differs; nothing is planned until `--relocate`.
    LocationChanged,
    /// The entry root cannot be edited; nothing is planned.
    UnsupportedRoot,
}

/// The plan for one harness location.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    /// The status.
    pub status: PlanStatus,
    /// The entries, sorted by name.
    pub entries: Vec<PlannedEntry>,
    /// The old files whose records were relocated.
    pub relocated_from: BTreeSet<PathBuf>,
}

/// Plans one harness location. Pure: no I/O.
#[must_use]
pub fn plan_harness(input: &PlanInput<'_>) -> Planned {
    let file = &input.read.location.file;
    let mut planned = Planned {
        status: PlanStatus::Ready,
        entries: Vec::new(),
        relocated_from: BTreeSet::new(),
    };
    let moved: BTreeSet<&Path> = input
        .records
        .values()
        .map(|record| record.file.as_path())
        .filter(|recorded| recorded != file)
        .collect();
    let mut records = input.records.clone();
    if !moved.is_empty() {
        if !input.relocate {
            planned.status = PlanStatus::LocationChanged;
            planned.entries = input
                .records
                .iter()
                .map(|(name, record)| {
                    let mut entry = PlannedEntry::new(name, Operation::LocationChanged);
                    entry.owned_hash = record.hashes().map(|hashes| hashes.readback.clone());
                    entry
                })
                .collect();
            return planned;
        }
        planned.relocated_from = moved.into_iter().map(Path::to_path_buf).collect();
        for record in records.values_mut() {
            record.file.clone_from(file);
        }
    }
    if !input.read.root_supported {
        planned.status = PlanStatus::UnsupportedRoot;
        return planned;
    }
    let names: BTreeSet<&String> = input.desired.keys().chain(records.keys()).collect();
    for name in names {
        let original = input.records.get(name);
        let mut record = records.get(name).cloned();
        let actual = input.read.entries.get(name);
        let desired = input.desired.get(name);
        if matches!(desired, Some(Desired::Held(_))) && record.is_none() {
            // Nothing is planned for a held server d-mcp does not own.
            continue;
        }
        let mut taken_over = false;
        if input.take_over.contains(name)
            && let Some(actual) = actual
        {
            let definition_hash = match desired {
                Some(Desired::Render {
                    definition_hash, ..
                }) => Some(definition_hash.clone()),
                _ => record
                    .as_ref()
                    .and_then(Record::hashes)
                    .map(|hashes| hashes.definition_hash.clone()),
            };
            if let Some(definition_hash) = definition_hash {
                record = Some(Record::owned(
                    file.clone(),
                    Hashes {
                        sent: actual.hash.clone(),
                        readback: actual.hash.clone(),
                        definition_hash,
                        // A taken-over entry was not written by d-mcp.
                        written: false,
                    },
                ));
                taken_over = true;
            }
        }
        let mut entry = plan_entry(input.adapter, file, name, desired, record.as_ref(), actual);
        entry.taken_over = taken_over;
        entry.previous = record.as_ref().and_then(Record::hashes).cloned();
        entry.desired = matches!(desired, Some(Desired::Render { .. }));
        // A changed record (take-over, relocate) is persisted unless a
        // write or a drop replaces it anyway.
        if entry.record == RecordChange::Keep
            && entry.write.is_none()
            && record.as_ref() != original
            && let Some(record) = record
        {
            entry.record = RecordChange::Set(Box::new(record));
        }
        planned.entries.push(entry);
    }
    planned
}

fn plan_entry(
    adapter: &dyn Adapter,
    file: &Path,
    name: &str,
    desired: Option<&Desired>,
    record: Option<&Record>,
    actual: Option<&ActualEntry>,
) -> PlannedEntry {
    let hashes = record.and_then(Record::hashes);
    let owned_hash = hashes.map(|hashes| hashes.readback.clone());
    let mut entry = match desired {
        Some(Desired::Held(reason)) => {
            let mut entry = PlannedEntry::new(name, Operation::Held);
            entry.reason = Some(*reason);
            entry
        }
        Some(Desired::Render {
            rendered,
            definition_hash,
        }) => plan_desired(
            adapter,
            file,
            name,
            rendered,
            definition_hash,
            hashes,
            actual,
        ),
        None => match (hashes, actual) {
            (Some(hashes), Some(actual)) if actual.hash != hashes.readback => {
                PlannedEntry::new(name, Operation::Drift)
            }
            (Some(_), Some(actual)) if !actual.supported_style => {
                PlannedEntry::new(name, Operation::Unsupported)
            }
            (Some(_), Some(_)) => {
                let mut entry = PlannedEntry::new(name, Operation::Remove);
                entry.write = Some(WriteKind::Remove);
                entry
            }
            (_, None) => {
                let mut entry = PlannedEntry::new(name, Operation::Orphaned);
                entry.record = RecordChange::Drop;
                entry
            }
            (None, Some(_)) => PlannedEntry::new(name, Operation::Unchanged),
        },
    };
    if entry.owned_hash.is_none() && entry.operation != Operation::Orphaned {
        entry.owned_hash = owned_hash;
    }
    entry
}

fn plan_desired(
    adapter: &dyn Adapter,
    file: &Path,
    name: &str,
    rendered: &Rendered,
    definition_hash: &str,
    hashes: Option<&Hashes>,
    actual: Option<&ActualEntry>,
) -> PlannedEntry {
    // Preserved keys that no longer fit the rendering are removed by an
    // Update, so they never satisfy a manual-step gap.
    let dropped = actual.map_or_else(Vec::new, |actual| {
        adapter.dropped_preserved(rendered, &actual.preserved)
    });
    let kept = |key: &str| {
        actual.is_some_and(|actual| actual.preserved.contains(key)) && !dropped.contains(&key)
    };
    let mut gaps: Vec<Gap> = rendered
        .gaps
        .iter()
        .filter(|gap| gap.unless_present.is_none_or(|key| !kept(key)))
        .cloned()
        .collect();
    let important = gaps
        .iter()
        .any(|gap| gap.severity == GapSeverity::Important);
    let write = |operation: Operation, kind: WriteKind| {
        let mut entry = PlannedEntry::new(name, operation);
        if important {
            entry.operation = Operation::Blocked;
        } else {
            entry.write = Some(kind);
            entry.rendered = Some(rendered.clone());
            entry.definition_hash = Some(definition_hash.to_owned());
        }
        entry
    };
    let mut entry = match (hashes, actual) {
        (None, None) => write(Operation::Add, WriteKind::Add),
        (None, Some(actual)) => {
            if adapter.hashes_comparable() && actual.hash == rendered.sent_hash {
                PlannedEntry::new(name, Operation::IdenticalCollision)
            } else {
                PlannedEntry::new(name, Operation::Collision)
            }
        }
        (Some(_), None) => write(Operation::Vanished, WriteKind::Add),
        (Some(hashes), Some(actual)) => {
            if actual.hash != hashes.readback {
                PlannedEntry::new(name, Operation::Drift)
            } else if rendered.sent_hash != hashes.sent {
                if actual.supported_style {
                    write(Operation::Update, WriteKind::Update)
                } else {
                    PlannedEntry::new(name, Operation::Unsupported)
                }
            } else {
                let mut entry = PlannedEntry::new(name, Operation::Unchanged);
                if hashes.definition_hash != definition_hash {
                    let mut refreshed = hashes.clone();
                    refreshed.definition_hash = definition_hash.to_owned();
                    entry.record =
                        RecordChange::Set(Box::new(Record::owned(file.to_path_buf(), refreshed)));
                }
                entry
            }
        }
    };
    if entry.write == Some(WriteKind::Update) {
        gaps.extend(dropped.iter().map(|key| Gap {
            field: (*key).to_owned(),
            severity: GapSeverity::Note,
            reason: format!("{key} removed: no longer used by this definition"),
            unless_present: None,
        }));
    }
    entry.gaps = gaps;
    entry
}

#[cfg(test)]
mod tests;
