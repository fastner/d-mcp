//! `d-mcp uninstall`: stop managing every harness entry, remove the ones
//! d-mcp created, and with `--purge` delete the store.
//!
//! Without `--apply` it is a read-only preview (no lock, no writes);
//! `--apply --dry-run` equals the preview. Every record is planned the way
//! a sync plans an entry that is no longer desired (see
//! `sync::plan_location`), after `pending` records are settled like sync
//! settles them, and gets one action:
//!
//! | Action | When | Effect |
//! |---|---|---|
//! | `remove` | d-mcp created the entry (`written`, not adopted) and it is unchanged | removed through a scoped sync (Remove) under the lock |
//! | `release` | the entry was adopted, or taken over without a write | ownership dropped as by `unmanage`; nothing is written |
//! | `forget` | the entry is gone | the record is dropped |
//! | `drifted` | changed outside d-mcp | left; the record is kept unless `--purge` |
//! | `location-changed` | recorded for another file | left; blocks `--purge` |
//! | `unsupported` | written in a style d-mcp does not edit | left; the record is kept unless `--purge` |
//!
//! Selections lose the harnesses of removed, released and forgotten
//! entries; selections of entries left behind are kept. A Claude Code
//! harness with removals and no `claude` on `PATH` fails with
//! `harness-cli-not-found` (exit 4), removes nothing and blocks `--purge`.
//!
//! `--purge` (only when nothing blocks it) deletes the known store entries
//! ([`crate::store::KNOWN_ENTRIES`], `.hash-key` included) inside the resolved store root
//! and then the root itself when it is empty. It never follows a symlink
//! inside the store, and registered sources live outside the store. A store
//! root that is a symlink is purged only with `--follow-store-link`, after
//! a warning naming the link target; the link and its target directory are
//! left. The error precedence is 3, 4, 1, 0, as for `sync`.

mod purge;
mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::adapter::{self, Adapter, ApplyMode, Scope};
use crate::config::Harness;
use crate::error::{Error, Result};
use crate::hashkey;
use crate::output::Report;
use crate::state::{self, Record, RecordKey, ReplacedField, State};
use crate::store::{Environment, StoreLock, StorePaths};
use crate::sync::{
    LocationPlan, Operation, PlanStatus, SyncOptions, plan_location, rank,
    release_selections_under_lock, release_under_lock, sync_under_lock,
};
use purge::PurgePlan;

/// What uninstall does with an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Remove the entry d-mcp created.
    Remove,
    /// Drop ownership; the entry stays.
    Release,
    /// The entry is gone; drop the record.
    Forget,
    /// Changed outside d-mcp; left.
    Drifted,
    /// Recorded for another file; left.
    LocationChanged,
    /// Written in a style d-mcp does not edit; left.
    Unsupported,
}

impl Action {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Remove => "remove",
            Action::Release => "release",
            Action::Forget => "forget",
            Action::Drifted => "drifted",
            Action::LocationChanged => "location-changed",
            Action::Unsupported => "unsupported",
        }
    }

    /// Whether the entry is left in the harness with its record.
    #[must_use]
    pub fn leaves(self) -> bool {
        matches!(
            self,
            Action::Drifted | Action::LocationChanged | Action::Unsupported
        )
    }
}

/// What happened to an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// A preview: it would happen.
    Planned,
    /// It happened.
    Applied,
    /// The removal failed (see the harness error and the `sync` document),
    /// or the step that removes or releases the entry stopped at an error
    /// (see the apply error): it may be partly done.
    Failed,
    /// The entry and its record are left.
    Left,
}

impl Outcome {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Planned => "planned",
            Outcome::Applied => "applied",
            Outcome::Failed => "failed",
            Outcome::Left => "left",
        }
    }
}

/// One recorded entry.
#[derive(Debug, Clone)]
pub struct EntryUninstall {
    /// The entry name.
    pub name: String,
    /// The action.
    pub action: Action,
    /// The outcome.
    pub outcome: Outcome,
    /// `<harness>:<scope>` of an adopted entry.
    pub adopted_from: Option<String>,
    /// The fields whose literals adoption replaced: the variables a
    /// released entry still reads.
    pub replaced_fields: Vec<ReplacedField>,
    /// What it means for the user.
    pub notice: String,
}

/// The status of a harness location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationState {
    /// Read and planned.
    Ready,
    /// Neither the harness file nor its binary exists.
    NotPresent,
    /// The location could not be resolved or read, or its CLI is missing.
    Failed,
    /// The recorded file differs from the resolved one.
    LocationChanged,
    /// The entry root cannot be edited.
    Unsupported,
}

impl LocationState {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LocationState::Ready => "ready",
            LocationState::NotPresent => "not-present",
            LocationState::Failed => "failed",
            LocationState::LocationChanged => "location-changed",
            LocationState::Unsupported => "unsupported",
        }
    }
}

/// One harness location.
#[derive(Debug, Clone)]
pub struct HarnessUninstall {
    /// The harness.
    pub harness: Harness,
    /// The scope.
    pub scope: Scope,
    /// The harness file, when resolved.
    pub file: Option<PathBuf>,
    /// The status.
    pub state: LocationState,
    /// The harness error.
    pub error: Option<Error>,
    /// Location-level notices.
    pub notices: Vec<String>,
    /// The recorded entries, sorted by name.
    pub entries: Vec<EntryUninstall>,
}

/// Why `--purge` is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// A record is for another harness file.
    LocationChanged,
    /// A harness CLI that removals need is missing.
    HarnessCliNotFound,
    /// A harness failed, or a removal failed.
    HarnessFailed,
    /// The store root is a symlink and `--follow-store-link` is missing.
    StoreRootSymlink,
}

impl BlockReason {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::LocationChanged => "location-changed",
            BlockReason::HarnessCliNotFound => "harness-cli-not-found",
            BlockReason::HarnessFailed => "harness-failed",
            BlockReason::StoreRootSymlink => "store-root-symlink",
        }
    }
}

/// The whole uninstall.
#[derive(Debug, Clone)]
pub struct Uninstall {
    /// The store root.
    pub store: PathBuf,
    /// Whether `--apply` was given.
    pub apply: bool,
    /// Whether nothing was written (a preview).
    pub dry_run: bool,
    /// The harness locations, in adapter order.
    pub harnesses: Vec<HarnessUninstall>,
    /// The purge.
    pub purge: PurgePlan,
    /// The merged `sync` document of the scoped removals, after an apply
    /// with removals.
    pub sync: Option<Value>,
    /// Command-level notices.
    pub notices: Vec<String>,
}

/// The options of `uninstall`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UninstallOptions {
    /// Act; without it, preview.
    pub apply: bool,
    /// With `--apply`: preview anyway.
    pub dry_run: bool,
    /// Delete the store afterwards.
    pub purge: bool,
    /// Purge the target of a symlinked store root.
    pub follow_store_link: bool,
}

fn entry(name: &str, action: Action, record: &Record, harness: Harness) -> EntryUninstall {
    let notice = match action {
        Action::Remove => "d-mcp created it; it is removed".to_owned(),
        Action::Release => {
            let mut notice =
                "d-mcp drops its ownership and writes nothing; the entry stays as it is".to_owned();
            let vars: Vec<String> = record
                .meta
                .replaced_fields
                .iter()
                .map(|field| format!("{} ({})", crate::redact::safe_key(&field.var), field.field))
                .collect();
            if !vars.is_empty() {
                notice.push_str(&format!(
                    "; it reads these variables instead of the literals adoption replaced: {}",
                    vars.join(", ")
                ));
            }
            notice
        }
        Action::Forget => "the entry is gone; its record is dropped".to_owned(),
        Action::Drifted => format!(
            "changed outside d-mcp, so it is left and its record kept (unless --purge); run `d-mcp sync --take-over {name}` and uninstall again to remove it, or `d-mcp unmanage {name} --harness {}` to keep it",
            harness.as_str()
        ),
        Action::LocationChanged => {
            "recorded for another file, so it is left; run `d-mcp sync --relocate` first".to_owned()
        }
        Action::Unsupported => {
            "written in a style d-mcp does not edit, so it is left and its record kept (unless --purge)"
                .to_owned()
        }
    };
    EntryUninstall {
        name: name.to_owned(),
        action,
        outcome: if action.leaves() {
            Outcome::Left
        } else {
            Outcome::Planned
        },
        adopted_from: record.meta.adopted_from.as_ref().map(|from| from.label()),
        replaced_fields: record.meta.replaced_fields.clone(),
        notice,
    }
}

/// Plans one harness location; `state` gets the settled `pending` records
/// (reported unless this is a preview, which settles in memory only).
fn plan_harness(
    adapter: &dyn Adapter,
    env: &Environment,
    scope: Scope,
    state: &mut State,
    preview: bool,
) -> HarnessUninstall {
    let harness = adapter.harness();
    let plan = plan_location(
        adapter,
        env,
        scope,
        state,
        &BTreeMap::new(),
        &BTreeSet::new(),
    );
    let mut report = HarnessUninstall {
        harness,
        scope,
        file: None,
        state: LocationState::Ready,
        error: None,
        notices: Vec::new(),
        entries: Vec::new(),
    };
    let recorded: Vec<(String, Record)> = state
        .records
        .iter()
        .filter(|(key, _)| key.harness == harness && key.scope == scope)
        .map(|(key, record)| (key.name.clone(), record.clone()))
        .collect();
    match plan {
        LocationPlan::Failed { location, error } => {
            report.file = location.map(|location| location.file);
            report.state = LocationState::Failed;
            report.error = Some(error);
            if !recorded.is_empty() {
                report.notices.push(format!(
                    "{} record(s) are kept until the harness can be read",
                    recorded.len()
                ));
            }
        }
        LocationPlan::NotPresent { location } => {
            report.file = Some(location.file);
            report.state = LocationState::NotPresent;
            report.notices.push("harness not present".to_owned());
            report.entries = recorded
                .iter()
                .map(|(name, record)| entry(name, Action::Forget, record, harness))
                .collect();
        }
        LocationPlan::Planned {
            read,
            planned,
            records,
            notices,
        } => {
            report.file = Some(read.location.file.clone());
            if !preview {
                report.notices.extend(notices);
            }
            match planned.status {
                PlanStatus::Ready => {}
                PlanStatus::LocationChanged => report.state = LocationState::LocationChanged,
                PlanStatus::UnsupportedRoot => {
                    report.state = LocationState::Unsupported;
                    report
                        .notices
                        .push(adapter.texts().unsupported_root.to_owned());
                    report.entries = records
                        .iter()
                        .map(|(name, record)| entry(name, Action::Unsupported, record, harness))
                        .collect();
                }
            }
            for planned in &planned.entries {
                let Some(record) = records.get(&planned.name) else {
                    continue;
                };
                // d-mcp did not create an adopted entry or one taken over
                // without a write, so it only lets go of it.
                let foreign = record.meta.adopted_from.is_some()
                    || !record.hashes().is_some_and(|hashes| hashes.written);
                let action = match planned.operation {
                    Operation::LocationChanged => Action::LocationChanged,
                    Operation::Orphaned => Action::Forget,
                    _ if foreign => Action::Release,
                    Operation::Remove => Action::Remove,
                    Operation::Drift => Action::Drifted,
                    Operation::Unsupported => Action::Unsupported,
                    // Nothing else is planned for an entry that is no
                    // longer desired; letting go never writes.
                    _ => Action::Release,
                };
                report
                    .entries
                    .push(entry(&planned.name, action, record, harness));
            }
            let removes = report
                .entries
                .iter()
                .any(|entry| entry.action == Action::Remove);
            if removes
                && adapter.apply_mode() == ApplyMode::HarnessCli
                && !read.location.binary_found
            {
                report.state = LocationState::Failed;
                report.error = Some(Error::HarnessCliNotFound {
                    harness: harness.as_str(),
                    program: adapter.program(),
                });
                for entry in &mut report.entries {
                    if entry.action == Action::Remove {
                        entry.outcome = Outcome::Left;
                    }
                }
            }
        }
    }
    report
}

fn blocked_by(harnesses: &[HarnessUninstall]) -> Vec<(BlockReason, Option<Harness>)> {
    let mut blocked = Vec::new();
    for harness in harnesses {
        if harness
            .entries
            .iter()
            .any(|entry| entry.action == Action::LocationChanged)
        {
            blocked.push((BlockReason::LocationChanged, Some(harness.harness)));
        }
        match &harness.error {
            Some(Error::HarnessCliNotFound { .. }) => {
                blocked.push((BlockReason::HarnessCliNotFound, Some(harness.harness)));
            }
            Some(_) => blocked.push((BlockReason::HarnessFailed, Some(harness.harness))),
            None => {}
        }
    }
    blocked
}

/// The aggregate error: the highest-priority code, in the order 3, 4, 1.
/// `apply_error` is an error that stopped the apply after planning (a
/// write or a reload of the store).
fn aggregate(
    uninstall: &Uninstall,
    sync_error: Option<Error>,
    purge_error: Option<Error>,
    apply_error: Option<Error>,
) -> Option<Error> {
    let left = uninstall
        .harnesses
        .iter()
        .flat_map(|harness| harness.entries.iter())
        .filter(|entry| entry.action.leaves())
        .count();
    let incomplete = (left > 0).then_some(Error::UninstallIncomplete { count: left });
    let purge = &uninstall.purge;
    let blocked = (purge.requested && !purge.blocked.is_empty()).then_some(Error::PurgeBlocked {
        count: purge.blocked.len(),
    });
    uninstall
        .harnesses
        .iter()
        .filter_map(|harness| harness.error.clone())
        .chain(sync_error)
        .chain(purge_error)
        .chain(apply_error)
        .chain(incomplete)
        .chain(blocked)
        .max_by_key(|error| rank(error.exit_code()))
}

/// Merges the `sync` documents of several scoped syncs into one: the first
/// is kept, the harnesses of every later one are appended to it.
fn merge_sync(merged: &mut Option<Value>, document: Value) {
    let Some(all) = merged else {
        *merged = Some(document);
        return;
    };
    if let (Some(all), Some(more)) = (
        all["harnesses"].as_array_mut(),
        document["harnesses"].as_array(),
    ) {
        all.extend(more.iter().cloned());
    }
}

/// Sets the outcome of every planned entry that `step` selects, by its
/// harness and action.
fn mark(
    uninstall: &mut Uninstall,
    step: impl Fn(Harness, Action) -> bool,
    outcome: impl Fn(Harness, Scope, &str) -> Outcome,
) {
    for harness in &mut uninstall.harnesses {
        let (name_of, scope) = (harness.harness, harness.scope);
        for entry in &mut harness.entries {
            if entry.outcome == Outcome::Planned && step(name_of, entry.action) {
                entry.outcome = outcome(name_of, scope, &entry.name);
            }
        }
    }
}

/// Whether `action` is one the release step carries out.
fn is_release(action: Action) -> bool {
    matches!(action, Action::Release | Action::Forget)
}

/// Applies the plan under the lock: releases, then the scoped removals,
/// then the purge.
fn apply(
    paths: &StorePaths,
    env: &Environment,
    lock: &StoreLock,
    uninstall: &mut Uninstall,
    loaded: &State,
    state: &mut State,
    warnings: &mut Vec<String>,
) -> Result<(Option<Error>, Option<Error>)> {
    let mut config = paths.load_config()?;
    let key = |harness: &HarnessUninstall, name: &str| RecordKey {
        harness: harness.harness,
        scope: harness.scope,
        name: name.to_owned(),
    };
    let released: Vec<RecordKey> = uninstall
        .harnesses
        .iter()
        .flat_map(|harness| {
            harness
                .entries
                .iter()
                .filter(|entry| matches!(entry.action, Action::Release | Action::Forget))
                .map(move |entry| key(harness, &entry.name))
        })
        .collect();
    // Each step sets the outcomes of its entries as soon as it ends, so an
    // error in a later step reports what already happened: an entry still
    // `planned` was not changed, a `failed` one was in the step that
    // stopped.
    // A failed state write changed nothing; after it the records are
    // released, so a failed config write leaves them `failed`.
    release_under_lock(paths, lock, state, &released, warnings)?;
    if let Err(error) = release_selections_under_lock(paths, lock, &mut config, &released, warnings)
    {
        mark(
            uninstall,
            |_, action| is_release(action),
            |_, _, _| Outcome::Failed,
        );
        return Err(error);
    }
    mark(
        uninstall,
        |_, action| is_release(action),
        |_, _, _| Outcome::Applied,
    );
    // The settlement of `pending` records is persisted even without a
    // release.
    if released.is_empty() && state != loaded {
        warnings.extend(state::save(paths, lock, state)?.map(|w| w.to_string()));
    }
    let mut removals: BTreeMap<Harness, BTreeSet<String>> = BTreeMap::new();
    for harness in &uninstall.harnesses {
        for entry in &harness.entries {
            if entry.action == Action::Remove && entry.outcome == Outcome::Planned {
                removals
                    .entry(harness.harness)
                    .or_default()
                    .insert(entry.name.clone());
            }
        }
    }
    // A removed entry's harness leaves its selection first, so the scoped
    // sync plans a Remove; a crash in between leaves records without a
    // selection, which the next sync removes as well.
    let mut changed = false;
    for (harness, names) in &removals {
        for name in names {
            changed |=
                crate::sync::drop_from_selection(&mut config, name, &BTreeSet::from([*harness]))
                    .changed;
        }
    }
    if changed {
        warnings.extend(paths.save_config(lock, &config)?.map(|w| w.to_string()));
    }
    let mut sync_error: Option<Error> = None;
    for (harness, names) in &removals {
        let options = SyncOptions {
            harnesses: BTreeSet::from([*harness]),
            only: names.clone(),
            ..SyncOptions::default()
        };
        let removes = |of: Harness, action: Action| of == *harness && action == Action::Remove;
        // The removals of this harness, and the state they left: a removed
        // entry has no record any more.
        let after = match sync_under_lock(paths, env, lock, &options, warnings) {
            Ok((report, error)) => {
                merge_sync(&mut uninstall.sync, report.data);
                if let Some(error) = error
                    && sync_error
                        .as_ref()
                        .is_none_or(|current| rank(error.exit_code()) > rank(current.exit_code()))
                {
                    sync_error = Some(error);
                }
                state::load(paths)
            }
            Err(error) => Err(error),
        };
        let after = match after {
            Ok(after) => after,
            Err(error) => {
                mark(uninstall, removes, |_, _, _| Outcome::Failed);
                return Err(error);
            }
        };
        mark(uninstall, removes, |of, scope, name| {
            let key = RecordKey {
                harness: of,
                scope,
                name: name.to_owned(),
            };
            if after.records.contains_key(&key) {
                Outcome::Failed
            } else {
                Outcome::Applied
            }
        });
    }
    let failed_removal = uninstall
        .harnesses
        .iter()
        .flat_map(|harness| harness.entries.iter())
        .any(|entry| entry.action == Action::Remove && entry.outcome == Outcome::Failed);
    if failed_removal || sync_error.is_some() {
        uninstall.purge.block(BlockReason::HarnessFailed, None);
    }
    let mut purge_error = None;
    if uninstall.purge.requested && uninstall.purge.blocked.is_empty() {
        match uninstall.purge.run() {
            Ok(()) => {}
            Err(error) => purge_error = Some(error),
        }
    }
    Ok((sync_error, purge_error))
}

/// Runs `uninstall`.
///
/// # Errors
///
/// Global errors (an uninitialized store, the lock, a corrupt
/// `config.toml` or `state.toml`) abort without `data`. Everything else is
/// the aggregate error next to the report: harness errors, `sync` errors
/// of the scoped removals, an error that stopped the apply after planning
/// (such as a store write that failed), `uninstall-incomplete` and
/// `purge-blocked`, in the order 3, 4, 1.
pub fn uninstall(
    paths: &StorePaths,
    env: &Environment,
    options: &UninstallOptions,
    warnings: &mut Vec<String>,
) -> Result<(Report, Option<Error>)> {
    paths.require_initialized()?;
    let preview = !options.apply || options.dry_run;
    let lock = if preview {
        None
    } else {
        Some(StoreLock::acquire(paths)?)
    };
    let loaded = state::load(paths)?;
    paths.load_config()?;
    if let Some(lock) = &lock {
        warnings.extend(hashkey::ensure_under_lock(paths, lock)?.map(|w| w.to_string()));
    }
    let mut state = loaded.clone();
    let mut harnesses = Vec::new();
    for adapter in adapter::registry() {
        for scope in adapter.write_scopes() {
            harnesses.push(plan_harness(*adapter, env, *scope, &mut state, preview));
        }
    }
    let mut purge = PurgePlan::new(paths, options.purge, options.follow_store_link)?;
    for (reason, harness) in blocked_by(&harnesses) {
        purge.block(reason, harness);
    }
    if let Some(warning) = purge.symlink_warning(preview) {
        warnings.push(warning);
    }
    let mut uninstall = Uninstall {
        store: paths.root.clone(),
        apply: options.apply,
        dry_run: preview,
        harnesses,
        purge,
        sync: None,
        notices: Vec::new(),
    };
    let (sync_error, purge_error, apply_error) = match &lock {
        Some(lock) => match apply(
            paths,
            env,
            lock,
            &mut uninstall,
            &loaded,
            &mut state,
            warnings,
        ) {
            Ok((sync_error, purge_error)) => (sync_error, purge_error, None),
            // The plan is reported with the error: entries whose outcome is
            // still `planned` were not changed, entries `failed` were in the
            // step that stopped, and the purge did not run.
            Err(error) => {
                uninstall.notices.push(
                    "the apply stopped at an error; entries still `planned` were not changed, entries `failed` may be partly done, and the purge did not run".to_owned(),
                );
                (None, None, Some(error))
            }
        },
        None => (None, None, None),
    };
    drop(lock);
    if preview {
        uninstall.notices.push(if options.apply {
            "Dry run: nothing was written.".to_owned()
        } else {
            "Preview: nothing was written; run `d-mcp uninstall --apply` to uninstall.".to_owned()
        });
    }
    let error = aggregate(&uninstall, sync_error, purge_error, apply_error);
    Ok((
        Report::new(render::json(&uninstall), render::text(&uninstall)),
        error,
    ))
}
