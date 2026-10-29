//! The sync executor: store lock, write state, adapter apply, and promotion
//! or revert. The report model and its rendering live in `report`.
//!
//! [`sync`] takes the store lock itself; [`sync_under_lock`] runs under a
//! lock its caller holds (adoption writes the definition and the selection
//! first, under the same lock). [`SyncOptions::only`] limits a run to some
//! servers, and [`SyncOptions::bound`] binds take-overs to expected entry
//! hashes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::report::{self, HarnessReport, HarnessStatus, Outcome};
use super::{
    BoundTakeOver, Desired, HeldReason, Operation, PlanInput, PlanStatus, PlannedEntry,
    RecordChange, plan_harness,
};
use crate::adapter::{
    self, Adapter, ApplyContext, ApplyMode, EntryOutcome, EntryResult, Gap, GapSeverity,
    HarnessPlan, HarnessRead, Hooks, Intent, Journal, Scope, VarLookup, WriteOp,
};
use crate::config::{Harness, HarnessSet};
use crate::error::{Error, Result};
use crate::hashkey::{self, HashKey};
use crate::output::Report;
use crate::process::ProcessRunner;
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
    /// Names whose actual entries are accepted as owned, whatever their
    /// content (`sync --take-over`).
    pub take_over: BTreeSet<String>,
    /// The servers to plan, write and report; empty means every server.
    /// Records of other servers are not settled or changed.
    pub only: BTreeSet<String>,
    /// Take-overs bound to the entry hash the caller saw, per harness and
    /// name, with the adoption provenance to record. An entry whose actual
    /// hash differs fails its harness with `adopt-entry-changed` (exit 3)
    /// before anything is journaled.
    pub bound: BTreeMap<Harness, BTreeMap<String, BoundTakeOver>>,
    /// Rewrite recorded paths to the newly resolved location.
    pub relocate: bool,
    /// Looks up a variable of d-mcp's environment (for `client_secret_env`);
    /// `None` reads the process environment.
    pub var: Option<VarLookup<'a>>,
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
                    written: true,
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
                meta: planned.map(|entry| entry.meta.clone()).unwrap_or_default(),
            };
            self.state.records.insert(key, record);
        }
        let warning = state::save(self.paths, self.lock, self.state)?;
        self.warnings
            .extend(warning.map(|warning| warning.to_string()));
        Ok(())
    }
}

/// What the resolver wants for each server selected for a harness. Also the
/// input of the read-only inventory classification.
pub(crate) fn desired_for(
    adapter: &dyn Adapter,
    resolution: &Resolution,
) -> BTreeMap<String, Desired> {
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
/// read, for the servers in `only` (every server when it is empty). A
/// record for another file (after a crash and a changed `CODEX_HOME`) is
/// left pending: it is reported as a location change and settled once its
/// file is read again. Returns notices for what was settled.
pub(super) fn settle_pending(
    state: &mut State,
    harness: Harness,
    scope: Scope,
    read: &HarnessRead,
    only: &BTreeSet<String>,
) -> Vec<String> {
    let pending: Vec<RecordKey> = state
        .records
        .iter()
        .filter(|(key, record)| {
            key.harness == harness
                && key.scope == scope
                && (only.is_empty() || only.contains(&key.name))
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
    /// The display-hash key, for hashes of entries d-mcp did not write.
    key: Option<&'a HashKey>,
}

impl Context<'_> {
    fn in_scope(&self, name: &str) -> bool {
        self.options.only.is_empty() || self.options.only.contains(name)
    }
}

/// The first bound take-over whose entry changed since the caller saw it.
fn changed_entry(
    bound: Option<&BTreeMap<String, BoundTakeOver>>,
    read: &HarnessRead,
) -> Option<String> {
    bound?.iter().find_map(|(name, take)| {
        let expected = take.expected.as_ref()?;
        let unchanged = read
            .entries
            .get(name)
            .is_some_and(|actual| expected.matches(&actual.hash));
        (!unchanged).then(|| name.clone())
    })
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
    let bound = context.options.bound.get(&harness);
    if let Some(name) = changed_entry(bound, &read) {
        return Ok(report.failed(Error::AdoptEntryChanged {
            entry: format!("{}:{}:{name}", harness.as_str(), scope.as_str()),
        }));
    }
    let only = &context.options.only;
    let before = state.clone();
    report
        .notices
        .extend(settle_pending(state, harness, scope, &read, only));
    let records: BTreeMap<String, Record> = state
        .records
        .iter()
        .filter(|(key, _)| {
            key.harness == harness && key.scope == scope && context.in_scope(&key.name)
        })
        .map(|(key, record)| (key.name.clone(), record.clone()))
        .collect();
    let desired: BTreeMap<String, Desired> = desired_for(adapter, context.resolution)
        .into_iter()
        .filter(|(name, _)| context.in_scope(name))
        .collect();
    let mut take_over = context.options.take_over.clone();
    take_over.extend(
        bound
            .into_iter()
            .flatten()
            .filter(|(_, take)| take.expected.is_some())
            .map(|(name, _)| name.clone()),
    );
    let mut planned = plan_harness(&PlanInput {
        adapter,
        read: &read,
        records: &records,
        desired: &desired,
        take_over: &take_over,
        relocate: context.options.relocate,
    });
    // An adopted entry's record (and any record its write creates) carries
    // the adoption provenance.
    for entry in &mut planned.entries {
        if let Some(take) = bound.and_then(|bound| bound.get(&entry.name)) {
            entry.meta.adopted_from = Some(take.adopted_from.clone());
            entry.meta.replaced_fields.clone_from(&take.replaced_fields);
            entry.meta.replaced_fields.sort();
            if let RecordChange::Set(record) = &mut entry.record {
                record.meta.clone_from(&entry.meta);
            }
        }
    }
    match planned.status {
        PlanStatus::Ready => {}
        PlanStatus::LocationChanged => report.status = HarnessStatus::LocationChanged,
        PlanStatus::UnsupportedRoot => {
            report.status = HarnessStatus::Unsupported;
            report
                .notices
                .push(adapter.texts().unsupported_root.to_owned());
        }
    }
    for old in &planned.relocated_from {
        report.notices.push(format!(
            "relocated from {}: that file still contains {}, and d-mcp no longer owns them",
            old.to_string_lossy(),
            adapter.texts().relocated_leftover
        ));
    }
    for entry in &planned.entries {
        let managed = entry.desired || entry.owned_hash.is_some();
        if let (true, Some(projects)) = (managed, read.shadowed.get(&entry.name)) {
            for project in projects {
                report.notices.push(format!(
                    "`{}` is shadowed by a local-scope entry in the project {project}; the harness uses that entry there",
                    entry.name
                ));
            }
        }
    }
    let dry_run = context.lock.is_none();
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
                rollback: entry.previous.as_ref().is_some_and(|hashes| hashes.written),
            })
        })
        .collect();
    if adapter.apply_mode() == ApplyMode::HarnessCli && !location.binary_found {
        report.notices.push(
            "the harness CLI is not on PATH; d-mcp writes this harness only through it".to_owned(),
        );
    }
    let mut write_outcome = if dry_run {
        Outcome::Planned
    } else {
        Outcome::Applied
    };
    let mut results: BTreeMap<String, EntryResult> = BTreeMap::new();
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
        let process_env = |name: &str| std::env::var_os(name);
        let ctx = ApplyContext {
            env: context.env,
            runner: &ProcessRunner::default(),
            neutral_dir: &context.paths.root,
            var: context.options.var.unwrap_or(&process_env),
        };
        let applied = adapter.apply(
            &ctx,
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
                let mut unsettled = false;
                for entry in result.entries {
                    match entry.outcome {
                        EntryOutcome::Applied => {
                            promote(state, key(&entry.name), &location.file, &planned, &entry);
                        }
                        // Nothing was journaled for a skipped entry.
                        EntryOutcome::Drift => {}
                        _ => unsettled = true,
                    }
                    results.insert(entry.name.clone(), entry);
                }
                if unsettled {
                    // A harness-CLI write that failed, was rolled back or
                    // lost its entry: settle its intent against the file.
                    // An unreadable file keeps it pending.
                    if let Ok(reread) = adapter.read(&location) {
                        settle_pending(state, harness, scope, &reread, only);
                    }
                }
            }
            Err(error) => {
                write_outcome = Outcome::Failed;
                // Settle the intents against what is on disk now: a failed
                // persist or a restored file reverts to the previous owned
                // records; an unreadable file keeps them pending.
                if let Ok(reread) = adapter.read(&location) {
                    settle_pending(state, harness, scope, &reread, only);
                }
                report = report.failed(error);
            }
        }
    }
    if let (true, Some(lock)) = (attempted || *state != before, context.lock) {
        let warning = state::save(context.paths, lock, state)?;
        warnings.extend(warning.map(|warning| warning.to_string()));
    }
    let texts = adapter.texts();
    let mut worst: Option<Error> = None;
    for entry in planned.entries {
        let result = results.remove(&entry.name);
        let mut operation = entry.operation;
        let mut error = None;
        let mut gaps = entry.gaps;
        let mut extra_notice = None;
        // The verified read-back of an applied write (`None` after a Remove),
        // and whether d-mcp wrote it.
        let mut readback: Option<(Option<String>, bool)> = None;
        let outcome = match result {
            Some(result) => {
                gaps.extend(result.gaps);
                match result.outcome {
                    EntryOutcome::Applied => {
                        readback = Some((result.readback, true));
                        Outcome::Applied
                    }
                    EntryOutcome::Drift => {
                        operation = Operation::Drift;
                        readback = Some((entry.owned_hash.clone(), entry.owned_written));
                        Outcome::Blocked
                    }
                    EntryOutcome::Failed(failure) => {
                        error = Some(failure);
                        Outcome::Failed
                    }
                    EntryOutcome::RolledBack(failure) => {
                        error = Some(failure);
                        extra_notice = Some(
                            "the new entry could not be added; the previous entry was restored"
                                .to_owned(),
                        );
                        Outcome::RolledBack
                    }
                    EntryOutcome::Lost {
                        error: failure,
                        fields,
                    } => {
                        error = Some(failure);
                        extra_notice = Some(format!(
                            "lost: the update removed the entry and could not add it again (fields: {}); the next sync re-adds it",
                            fields.join(", ")
                        ));
                        Outcome::Lost
                    }
                }
            }
            None if entry.write.is_some() => write_outcome,
            None if entry.operation.blocks() => Outcome::Blocked,
            None if entry.operation == Operation::Held => Outcome::Kept,
            None if entry.record != RecordChange::Keep => {
                if dry_run {
                    Outcome::Planned
                } else {
                    Outcome::Recorded
                }
            }
            None => Outcome::Unchanged,
        };
        let (hash, written) = match (readback, outcome) {
            (Some(readback), _) => readback,
            (None, Outcome::Failed | Outcome::RolledBack | Outcome::Lost) => {
                (entry.owned_hash.clone(), entry.owned_written)
            }
            (None, _) => match &entry.record {
                RecordChange::Set(record) => (
                    record.hashes().map(|h| h.readback.clone()),
                    record.hashes().is_some_and(|h| h.written),
                ),
                RecordChange::Drop => (None, false),
                RecordChange::Keep if entry.write.is_some() => (None, false),
                RecordChange::Keep => (entry.owned_hash.clone(), entry.owned_written),
            },
        };
        // Only a hash d-mcp wrote itself is shown plain; a taken-over
        // record holds a foreign entry's hash, shown keyed.
        let (hash, display_hash) = if written {
            (hash, None)
        } else {
            let keyed = hash.and_then(|hash| context.key.map(|key| key.display_hash(&hash)));
            (None, keyed)
        };
        if let Some(failure) = &error
            && worst.as_ref().is_none_or(|current| {
                report::rank(failure.exit_code()) > report::rank(current.exit_code())
            })
        {
            worst = Some(failure.clone());
        }
        let notice = report::notice(
            operation,
            &entry.name,
            harness,
            texts,
            entry.taken_over,
            entry.desired,
        );
        let notice = match (notice, extra_notice) {
            (Some(notice), Some(extra)) => Some(format!("{notice}; {extra}")),
            (notice, extra) => notice.or(extra),
        };
        report.entries.push(report::EntryReport {
            notice,
            name: entry.name,
            operation,
            outcome,
            gaps,
            reason: entry.reason,
            hash,
            display_hash,
            error,
        });
    }
    if let (None, Some(worst)) = (&report.error, worst) {
        report = report.failed(worst);
    }
    Ok(report)
}

/// Promotes the record of an applied write: owned with the planned hashes
/// and the verified read-back hash, written by d-mcp, keeping the planned
/// provenance and history (a `vanished` re-add counts in it); or dropped
/// after a Remove.
fn promote(
    state: &mut State,
    key: RecordKey,
    file: &Path,
    planned: &super::Planned,
    entry: &EntryResult,
) {
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
                definition_hash: planned_entry.definition_hash.clone().unwrap_or_default(),
                written: true,
            };
            let mut meta = planned_entry.meta.clone();
            if planned_entry.operation == Operation::Vanished {
                meta.count_vanish();
            }
            state
                .records
                .insert(key, Record::owned_with(file.to_path_buf(), hashes, meta));
        }
        _ => {
            state.records.remove(&key);
        }
    }
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
    run(paths, env, lock.as_ref(), options, warnings)
}

/// Runs `sync` under a store lock the caller holds, so a command can write
/// the store and sync in one locked run (a second lock in the same process
/// would fail with `store-locked`). `options.dry_run` must be `false`; the
/// run writes. Errors and the report are those of [`sync`].
///
/// # Errors
///
/// See [`sync`].
pub fn sync_under_lock(
    paths: &StorePaths,
    env: &Environment,
    lock: &StoreLock,
    options: &SyncOptions<'_>,
    warnings: &mut Vec<String>,
) -> Result<(Report, Option<Error>)> {
    debug_assert!(!options.dry_run, "a locked sync writes");
    run(paths, env, Some(lock), options, warnings)
}

fn run(
    paths: &StorePaths,
    env: &Environment,
    lock: Option<&StoreLock>,
    options: &SyncOptions<'_>,
    warnings: &mut Vec<String>,
) -> Result<(Report, Option<Error>)> {
    paths.require_initialized()?;
    let config = paths.load_config()?;
    let mut state = state::load(paths)?;
    // Every mutating command creates a missing display-hash key; a dry run
    // only reads it.
    let key = match lock {
        Some(lock) => {
            let (key, _, warning) = hashkey::ensure(paths, lock)?;
            warnings.extend(warning.map(|warning| warning.to_string()));
            Some(key)
        }
        None => match hashkey::load(paths) {
            Ok(key) => key,
            Err(error) => {
                warnings.push(format!("no display hashes: {}", error.hint()));
                None
            }
        },
    };
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
        lock,
        options,
        resolution: &resolution,
        key: key.as_ref(),
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
    let dry_run = lock.is_none();
    let error = report::aggregate_error(&reports);
    let (data, text) = report::render(&reports, &paths.root.to_string_lossy(), dry_run);
    let problems = if options.only.is_empty() {
        resolution.problems
    } else {
        // A scoped run reports only the problems of its servers.
        resolution
            .problems
            .into_iter()
            .filter(|problem| {
                problem
                    .server
                    .as_ref()
                    .is_none_or(|server| options.only.contains(server))
            })
            .collect()
    };
    Ok((
        Report {
            data,
            problems,
            text,
        },
        error,
    ))
}
