//! The sync executor: store lock, write state, adapter apply, and promotion
//! or revert. The report model and its rendering live in `report`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::report::{self, HarnessReport, HarnessStatus, Outcome};
use super::{
    Desired, HeldReason, Operation, PlanInput, PlanStatus, PlannedEntry, RecordChange, plan_harness,
};
use crate::adapter::{
    self, Adapter, Gap, GapSeverity, HarnessPlan, HarnessRead, Hooks, Intent, Journal, Scope,
    WriteOp,
};
use crate::config::{Harness, HarnessSet};
use crate::error::{Error, Result};
use crate::output::Report;
use crate::resolver::{HoldReason, Resolution, Status, resolve};
use crate::source::{scan, source_specs};
use crate::state::{self, Hashes, Record, RecordKey, RecordState, Settled, State};
use crate::store::{Environment, StoreLock, StorePaths};
use crate::validate::validate;

/// Test seams of the executor, for failure injection. Not part of the
/// stable API.
#[doc(hidden)]
#[derive(Default)]
pub struct SyncHooks<'a> {
    /// Seams inside the adapter's apply.
    pub adapter: Hooks<'a>,
    /// Runs after a successful apply and before the records are promoted.
    /// An error stops the run at once, as a crash would: the `pending`
    /// records stay for the next sync to settle.
    pub before_promote: Option<&'a dyn Fn() -> Result<()>>,
}

/// The options of `sync`.
#[derive(Default)]
pub struct SyncOptions<'a> {
    /// Plan and report only: no lock, no writes.
    pub dry_run: bool,
    /// The harnesses to sync; empty means every adapter.
    pub harnesses: BTreeSet<Harness>,
    /// Names whose actual entries are accepted as owned.
    pub take_over: BTreeSet<String>,
    /// Rewrite recorded paths to the newly resolved location.
    pub relocate: bool,
    /// Test seams.
    pub hooks: SyncHooks<'a>,
}

/// Writes `pending` records before the adapter persists anything.
struct StateJournal<'a> {
    state: &'a mut State,
    paths: &'a StorePaths,
    lock: &'a StoreLock,
    harness: Harness,
    scope: Scope,
    file: &'a Path,
    planned: &'a [PlannedEntry],
    warnings: &'a mut Vec<String>,
}

impl Journal for StateJournal<'_> {
    fn intend(&mut self, intents: &[Intent]) -> Result<()> {
        for intent in intents {
            let key = RecordKey {
                harness: self.harness,
                scope: self.scope,
                name: intent.name.clone(),
            };
            let planned = self.planned.iter().find(|entry| entry.name == intent.name);
            // The hashes the entry was planned from, after a take-over or a
            // relocation, not the possibly older stored ones.
            let previous = planned.and_then(|entry| entry.previous.clone());
            let expected = match (&intent.expected_readback, planned) {
                (Some(readback), Some(entry)) => Some(Hashes {
                    sent: entry
                        .rendered
                        .as_ref()
                        .map(|rendered| rendered.sent_hash.clone())
                        .unwrap_or_default(),
                    readback: readback.clone(),
                    definition_hash: entry.definition_hash.clone().unwrap_or_default(),
                }),
                _ => None,
            };
            let record = Record {
                file: self.file.to_path_buf(),
                time: state::now(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                state: RecordState::Pending {
                    op: intent.kind,
                    expected,
                    previous,
                },
            };
            self.state.records.insert(key, record);
        }
        let warning = state::save(self.paths, self.lock, self.state)?;
        self.warnings
            .extend(warning.map(|warning| warning.to_string()));
        Ok(())
    }
}

/// What the resolver wants for each server selected for a harness.
fn desired_for(adapter: &dyn Adapter, resolution: &Resolution) -> BTreeMap<String, Desired> {
    let harness = adapter.harness();
    resolution
        .servers
        .iter()
        .filter(|server| match &server.harnesses {
            HarnessSet::All => true,
            HarnessSet::Only(set) => set.contains(&harness),
        })
        .map(|server| {
            let desired = match (&server.status, &server.definition) {
                (Status::Resolved, Some(definition)) => {
                    let mut rendered = adapter.render(definition);
                    rendered
                        .gaps
                        .extend(adapter::foreign_security_gaps(adapter, definition));
                    if adapter.reserved_names().contains(&server.name.as_str()) {
                        rendered.gaps.push(Gap {
                            field: "name".to_owned(),
                            severity: GapSeverity::Important,
                            reason: format!("the name is reserved by {}", harness.as_str()),
                            unless_present: None,
                        });
                    }
                    Desired::Render {
                        rendered,
                        definition_hash: definition.definition_hash(),
                    }
                }
                (Status::Hold, _) => Desired::Held(match server.reason {
                    Some(HoldReason::SourceUnavailable) => HeldReason::SourceUnavailable,
                    _ => HeldReason::InvalidWinner,
                }),
                _ => Desired::Held(HeldReason::NotOffered),
            };
            (server.name.clone(), desired)
        })
        .collect()
}

/// Settles the `pending` records of a harness location against what was
/// read. A record for another file (after a crash and a changed
/// `CODEX_HOME`) is left pending: it is reported as a location change and
/// settled once its file is read again. Returns notices for what was
/// settled.
fn settle_pending(
    state: &mut State,
    harness: Harness,
    scope: Scope,
    read: &HarnessRead,
) -> Vec<String> {
    let pending: Vec<RecordKey> = state
        .records
        .iter()
        .filter(|(key, record)| {
            key.harness == harness
                && key.scope == scope
                && record.file == read.location.file
                && matches!(record.state, RecordState::Pending { .. })
        })
        .map(|(key, _)| key.clone())
        .collect();
    let mut notices = Vec::new();
    for key in pending {
        let actual = read.entries.get(&key.name).map(|entry| entry.hash.as_str());
        let (record, settled) = state::settle(&state.records[&key], actual);
        let verb = match settled {
            Settled::Promoted => "completed",
            Settled::Reverted => "reverted",
            Settled::Dropped => "settled",
        };
        notices.push(format!("an interrupted write of `{}` was {verb}", key.name));
        match record {
            Some(record) => {
                state.records.insert(key, record);
            }
            None => {
                state.records.remove(&key);
            }
        }
    }
    notices
}

struct Context<'a> {
    paths: &'a StorePaths,
    env: &'a Environment,
    lock: Option<&'a StoreLock>,
    options: &'a SyncOptions<'a>,
    resolution: &'a Resolution,
}

fn sync_harness(
    context: &Context<'_>,
    adapter: &dyn Adapter,
    scope: Scope,
    state: &mut State,
    warnings: &mut Vec<String>,
) -> Result<HarnessReport> {
    let harness = adapter.harness();
    let mut report = HarnessReport::new(harness, scope);
    let location = match adapter.locate(context.env, scope) {
        Ok(location) => location,
        Err(error) => return Ok(report.failed(error)),
    };
    report.file = Some(location.file.clone());
    if !location.present() {
        report.status = HarnessStatus::NotPresent;
        report.notices.push("harness not present".to_owned());
        return Ok(report);
    }
    let read = match adapter.read(&location) {
        Ok(read) => read,
        Err(error) => return Ok(report.failed(error)),
    };
    let before = state.clone();
    report
        .notices
        .extend(settle_pending(state, harness, scope, &read));
    let records: BTreeMap<String, Record> = state
        .records
        .iter()
        .filter(|(key, _)| key.harness == harness && key.scope == scope)
        .map(|(key, record)| (key.name.clone(), record.clone()))
        .collect();
    let desired = desired_for(adapter, context.resolution);
    let planned = plan_harness(&PlanInput {
        adapter,
        read: &read,
        records: &records,
        desired: &desired,
        take_over: &context.options.take_over,
        relocate: context.options.relocate,
    });
    match planned.status {
        PlanStatus::Ready => {}
        PlanStatus::LocationChanged => report.status = HarnessStatus::LocationChanged,
        PlanStatus::UnsupportedRoot => {
            report.status = HarnessStatus::Unsupported;
            report.notices.push(
                "`mcp_servers` is an inline table or uses dotted keys; d-mcp does not edit it"
                    .to_owned(),
            );
        }
    }
    for old in &planned.relocated_from {
        report.notices.push(format!(
            "relocated from {}: that file still contains the formerly managed tables, and d-mcp no longer owns them",
            old.to_string_lossy()
        ));
    }
    let dry_run = context.options.dry_run;
    let key = |name: &str| RecordKey {
        harness,
        scope,
        name: name.to_owned(),
    };
    for entry in &planned.entries {
        match &entry.record {
            RecordChange::Keep => {}
            RecordChange::Set(record) => {
                state.records.insert(key(&entry.name), (**record).clone());
            }
            RecordChange::Drop => {
                state.records.remove(&key(&entry.name));
            }
        }
    }
    let ops: Vec<WriteOp> = planned
        .entries
        .iter()
        .filter_map(|entry| {
            Some(WriteOp {
                name: entry.name.clone(),
                kind: entry.write?,
                rendered: entry.rendered.clone(),
            })
        })
        .collect();
    let mut write_outcome = if dry_run {
        Outcome::Planned
    } else {
        Outcome::Applied
    };
    let mut readbacks: BTreeMap<String, Option<String>> = BTreeMap::new();
    // The journal persists `pending` records, so after a write attempt the
    // final state is saved even when it equals the state before the run.
    let attempted = context.lock.is_some() && !ops.is_empty();
    if let (Some(lock), false) = (context.lock, ops.is_empty()) {
        let mut journal = StateJournal {
            state,
            paths: context.paths,
            lock,
            harness,
            scope,
            file: &location.file,
            planned: &planned.entries,
            warnings,
        };
        let applied = adapter.apply(
            &read,
            &HarnessPlan { ops },
            &mut journal,
            &context.options.hooks.adapter,
        );
        match applied {
            Ok(result) => {
                if let Some(hook) = context.options.hooks.before_promote {
                    hook()?;
                }
                report.created = result.created;
                warnings.extend(result.warnings.iter().map(ToString::to_string));
                for entry in result.entries {
                    let planned_entry = planned.entries.iter().find(|p| p.name == entry.name);
                    match (&entry.readback, planned_entry) {
                        (Some(readback), Some(planned_entry)) => {
                            let hashes = Hashes {
                                sent: planned_entry
                                    .rendered
                                    .as_ref()
                                    .map(|rendered| rendered.sent_hash.clone())
                                    .unwrap_or_default(),
                                readback: readback.clone(),
                                definition_hash: planned_entry
                                    .definition_hash
                                    .clone()
                                    .unwrap_or_default(),
                            };
                            state.records.insert(
                                key(&entry.name),
                                Record::owned(location.file.clone(), hashes),
                            );
                        }
                        _ => {
                            state.records.remove(&key(&entry.name));
                        }
                    }
                    readbacks.insert(entry.name, entry.readback);
                }
            }
            Err(error) => {
                write_outcome = Outcome::Failed;
                // Settle the intents against what is on disk now: a failed
                // persist or a restored file reverts to the previous owned
                // records; an unreadable file keeps them pending.
                if let Ok(reread) = adapter.read(&location) {
                    settle_pending(state, harness, scope, &reread);
                }
                report = report.failed(error);
            }
        }
    }
    if let (true, Some(lock)) = (attempted || *state != before, context.lock) {
        let warning = state::save(context.paths, lock, state)?;
        warnings.extend(warning.map(|warning| warning.to_string()));
    }
    for entry in planned.entries {
        let outcome = if entry.write.is_some() {
            write_outcome
        } else if entry.operation.blocks() {
            Outcome::Blocked
        } else if entry.operation == Operation::Held {
            Outcome::Kept
        } else if entry.record != RecordChange::Keep {
            if dry_run {
                Outcome::Planned
            } else {
                Outcome::Recorded
            }
        } else {
            Outcome::Unchanged
        };
        let hash = match readbacks.get(&entry.name) {
            Some(readback) => readback.clone(),
            None if outcome == Outcome::Failed => entry.owned_hash.clone(),
            None => match &entry.record {
                RecordChange::Set(record) => record.hashes().map(|h| h.readback.clone()),
                RecordChange::Drop => None,
                RecordChange::Keep if entry.write.is_some() => None,
                RecordChange::Keep => entry.owned_hash.clone(),
            },
        };
        report.entries.push(report::EntryReport {
            notice: report::notice(
                entry.operation,
                &entry.name,
                harness,
                entry.taken_over,
                entry.desired,
            ),
            name: entry.name,
            operation: entry.operation,
            outcome,
            gaps: entry.gaps,
            reason: entry.reason,
            hash,
        });
    }
    Ok(report)
}

/// Runs `sync`. A global error (store, lock, config, state, unknown
/// take-over) aborts before any harness is touched and is returned as `Err`.
/// Otherwise the report carries every harness, and the error, if any, is
/// the aggregate one: the highest-priority harness code, in the order 3, 4,
/// 1, 0.
///
/// # Errors
///
/// The global errors above.
pub fn sync(
    paths: &StorePaths,
    env: &Environment,
    options: &SyncOptions<'_>,
    warnings: &mut Vec<String>,
) -> Result<(Report, Option<Error>)> {
    paths.require_initialized()?;
    let lock = if options.dry_run {
        None
    } else {
        Some(StoreLock::acquire(paths)?)
    };
    let config = paths.load_config()?;
    let mut state = state::load(paths)?;
    for name in &options.take_over {
        let known =
            config.selection(name).is_some() || state.records.keys().any(|key| &key.name == name);
        if !known {
            return Err(Error::UnknownTakeOver { name: name.clone() });
        }
    }
    let mut snapshots: Vec<_> = source_specs(paths, &config).iter().map(scan).collect();
    validate(&mut snapshots, adapter::registry());
    let resolution = resolve(&snapshots, &config.selections);
    let context = Context {
        paths,
        env,
        lock: lock.as_ref(),
        options,
        resolution: &resolution,
    };
    let mut reports = Vec::new();
    for adapter in adapter::registry() {
        if !options.harnesses.is_empty() && !options.harnesses.contains(&adapter.harness()) {
            continue;
        }
        for scope in adapter.write_scopes() {
            reports.push(sync_harness(
                &context, *adapter, *scope, &mut state, warnings,
            )?);
        }
    }
    drop(lock);
    let error = report::aggregate_error(&reports);
    let (data, text) = report::render(&reports, &paths.root.to_string_lossy(), options.dry_run);
    Ok((
        Report {
            data,
            problems: resolution.problems,
            text,
        },
        error,
    ))
}
