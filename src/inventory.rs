//! `d-mcp inventory`: every discoverable scope of every harness, classified
//! against the write state. Read-only: no lock, no writes, no harness CLI.
//!
//! - **Classes** of user-scope entries come from a read-only
//!   [`plan_harness`] run (the sync planner, no second classifier):
//!   `managed`, `drifted`, `unmanaged`, `vanished`, `pending` (a write in
//!   flight) and `location-changed`. Project-scope entries are `unmanaged`
//!   (d-mcp writes user scope only); Claude Code's local-scope entries are
//!   `local`, by name only, and never hashed, compared or adopted. The
//!   class is about ownership and content: the owned, unchanged entry of a
//!   held (`pending-approval`, `risk-not-accepted`, ...) or revoked server
//!   stays `managed`; `status` shows why and what the next sync does.
//! - **`shadowed_in`** lists the narrower scopes that hold the same name and
//!   win in their project: Claude Code's local scope, an approved or pending
//!   `.mcp.json` entry, and a Codex project layer unless the project is
//!   untrusted.
//! - **Same name in several scopes**: a *duplicate* when the imported
//!   candidates have an equal `core_hash` and the same replaced literals, a
//!   *conflict* when they differ, *undetermined* when one cannot be imported.
//!   Neither the hash nor a literal is ever output, only the verdict.
//! - **Hashes**: `hash` is the plain owned hash, only for a record d-mcp
//!   wrote itself (`written`); every other hash is a keyed `display_hash`
//!   (see [`crate::hashkey`]), shown only when the store has a key.
//! - **Errors** of a scope are data; the command exits 0.
//!
//! Entry content is rendered only through [`crate::redact`].

mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;

use crate::adapter::{
    self, Adapter, Approval, DiscoveredScope, ImportGap, Imported, Normalization, Scope, ScopeRead,
    Trust, repository_root,
};
use crate::config::{Config, Harness};
use crate::error::{Error, Result};
use crate::hashkey::{self, HashKey};
use crate::output::Report;
use crate::redact::{EntryView, entry_view};
use crate::source::{ReadInputs, load_resolution};
use crate::state::{self, Record, RecordState, State};
use crate::store::{Environment, StorePaths};
use crate::sync::{Desired, Operation, PlanInput, PlanStatus, desired_for, plan_harness};

/// The class of an inventory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    /// Owned by d-mcp and unchanged since it was recorded.
    Managed,
    /// Owned, but changed outside d-mcp.
    Drifted,
    /// Not owned by d-mcp.
    Unmanaged,
    /// Owned, but no longer in the harness file.
    Vanished,
    /// A write of d-mcp was interrupted; the next sync settles it.
    Pending,
    /// Owned at another file (after `CODEX_HOME` or `CLAUDE_CONFIG_DIR`
    /// changed).
    LocationChanged,
    /// A Claude Code local-scope entry, known by name only.
    Local,
}

impl Class {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Managed => "managed",
            Class::Drifted => "drifted",
            Class::Unmanaged => "unmanaged",
            Class::Vanished => "vanished",
            Class::Pending => "pending",
            Class::LocationChanged => "location-changed",
            Class::Local => "local",
        }
    }
}

/// A narrower scope that shadows an entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Shadow {
    /// `local` or `project`.
    pub scope: &'static str,
    /// The project path or repository root.
    pub path: PathBuf,
}

/// What the import of an entry found, names only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportSummary {
    /// Whether the entry imports into a candidate definition.
    pub ok: bool,
    /// The fields it cannot express.
    pub gaps: Vec<ImportGap>,
    /// The literals the candidate replaces with references: field, kind and
    /// variable name.
    pub replaced: Vec<Replaced>,
    /// The declared normalizations.
    pub normalizations: Vec<Normalization>,
}

/// A replaced literal, without its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Replaced {
    /// The native field.
    pub field: String,
    /// `env`, `bearer-header` or `header`.
    pub kind: &'static str,
    /// The variable the candidate reads instead, as
    /// [`crate::redact::safe_key`] shows it.
    pub var: String,
}

impl ImportSummary {
    fn of(result: &std::result::Result<Imported, Vec<ImportGap>>) -> Self {
        match result {
            Ok(imported) => Self {
                ok: true,
                gaps: Vec::new(),
                replaced: imported
                    .replacements
                    .iter()
                    .map(|replacement| Replaced {
                        field: replacement.field.clone(),
                        kind: replacement.kind.as_str(),
                        var: crate::redact::safe_key(&replacement.var).into_owned(),
                    })
                    .collect(),
                normalizations: imported.normalizations.clone(),
            },
            Err(gaps) => Self {
                ok: false,
                gaps: gaps.clone(),
                replaced: Vec::new(),
                normalizations: Vec::new(),
            },
        }
    }
}

/// One entry of a scope.
#[derive(Debug, Clone)]
pub struct EntryInventory {
    /// The entry name.
    pub name: String,
    /// The class.
    pub class: Class,
    /// The plain owned hash, for a record d-mcp wrote itself.
    pub hash: Option<String>,
    /// The keyed display hash of every other hash.
    pub display_hash: Option<String>,
    /// The narrower scopes that shadow the entry.
    pub shadowed_in: BTreeSet<Shadow>,
    /// The approval of a Claude Code `.mcp.json` entry.
    pub approval: Option<Approval>,
    /// The redacted view of the entry content; `None` without content.
    pub view: Option<EntryView>,
    /// The import result; `None` for names-only and absent entries.
    pub import: Option<ImportSummary>,
}

impl EntryInventory {
    fn new(name: &str, class: Class) -> Self {
        Self {
            name: name.to_owned(),
            class,
            hash: None,
            display_hash: None,
            shadowed_in: BTreeSet::new(),
            approval: None,
            view: None,
            import: None,
        }
    }
}

/// One discovered scope.
#[derive(Debug, Clone)]
pub struct ScopeInventory {
    /// The scope.
    pub scope: DiscoveredScope,
    /// The file.
    pub file: PathBuf,
    /// Whether the scope exists.
    pub present: bool,
    /// The project trust, for a project scope.
    pub trust: Option<Trust>,
    /// Local scope: the project path no longer exists.
    pub orphaned: bool,
    /// Why the scope could not be read.
    pub error: Option<Error>,
    /// The entries, sorted by name.
    pub entries: Vec<EntryInventory>,
}

/// The scopes of one harness.
#[derive(Debug, Clone)]
pub struct HarnessInventory {
    /// The harness.
    pub harness: Harness,
    /// The scopes: user first.
    pub scopes: Vec<ScopeInventory>,
    /// Harness-level notices.
    pub notices: Vec<String>,
}

/// The verdict for a name found in several value-read scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The same server everywhere.
    Duplicate,
    /// Different servers under one name.
    Conflict,
    /// At least one entry cannot be imported, so d-mcp cannot tell.
    Undetermined,
}

impl Verdict {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Duplicate => "duplicate",
            Verdict::Conflict => "conflict",
            Verdict::Undetermined => "undetermined",
        }
    }
}

/// Where a same-name entry lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Place {
    /// The harness.
    pub harness: Harness,
    /// `user` or `project`.
    pub scope: &'static str,
    /// The file.
    pub file: PathBuf,
}

/// A name found in several value-read scopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SameName {
    /// The name.
    pub name: String,
    /// The verdict.
    pub verdict: Verdict,
    /// The places, in discovery order.
    pub places: Vec<Place>,
}

/// The whole inventory.
#[derive(Debug, Clone)]
pub struct Inventory {
    /// The store root.
    pub store: PathBuf,
    /// Whether the store is initialized; without it nothing is owned.
    pub initialized: bool,
    /// The current repository.
    pub repository: PathBuf,
    /// Whether display hashes are shown (the store has a key).
    pub display_hashes: bool,
    /// The harnesses, in registry order.
    pub harnesses: Vec<HarnessInventory>,
    /// Names found in several value-read scopes.
    pub same_name: Vec<SameName>,
    /// Command-level notices.
    pub notices: Vec<String>,
}

/// The options of `inventory`.
#[derive(Debug, Clone, Default)]
pub struct InventoryOptions {
    /// The harnesses to list; empty means every adapter.
    pub harnesses: BTreeSet<Harness>,
}

/// The store side of the classification: config, write state and key. An
/// uninitialized store reads as empty. Shared with `status` and `doctor`.
pub(crate) struct StoreView {
    /// Whether the store is initialized.
    pub(crate) initialized: bool,
    /// `config.toml`.
    pub(crate) config: Config,
    /// `state.toml`.
    pub(crate) state: State,
    /// The display-hash key, if the store has one.
    pub(crate) key: Option<HashKey>,
    /// `lock.toml` and `approvals.toml`, for the shared resolution.
    pub(crate) inputs: ReadInputs,
}

impl StoreView {
    /// Loads the store read-only; a missing or corrupt `.hash-key` only
    /// warns (no display hashes).
    pub(crate) fn load(paths: &StorePaths, warnings: &mut Vec<String>) -> Result<Self> {
        if !paths.is_initialized() {
            return Ok(Self {
                initialized: false,
                config: Config::default(),
                state: State::default(),
                key: None,
                inputs: ReadInputs::default(),
            });
        }
        let config = paths.load_config()?;
        let state = state::load(paths)?;
        let inputs = ReadInputs::load(paths)?;
        let key = match hashkey::load(paths) {
            Ok(key) => key,
            Err(error) => {
                warnings.push(format!("no display hashes: {}", error.hint()));
                None
            }
        };
        Ok(Self {
            initialized: true,
            config,
            state,
            key,
            inputs,
        })
    }

    /// The display hash of an entry hash, when the store has a key.
    pub(crate) fn display(&self, hash: &str) -> Option<String> {
        self.key.as_ref().map(|key| key.display_hash(hash))
    }
}

/// The class of a planned user-scope entry; `None` when the planner wants an
/// entry that does not exist (an Add).
fn class_of(operation: Operation, recorded: bool, present: bool) -> Option<Class> {
    Some(match operation {
        Operation::Drift => Class::Drifted,
        Operation::Collision | Operation::IdenticalCollision => Class::Unmanaged,
        Operation::Vanished | Operation::Orphaned => Class::Vanished,
        Operation::LocationChanged => Class::LocationChanged,
        Operation::Add => return None,
        Operation::Unchanged if !recorded => Class::Unmanaged,
        Operation::Update
        | Operation::Remove
        | Operation::Unsupported
        | Operation::Unchanged
        | Operation::Held => Class::Managed,
        Operation::Blocked if present => Class::Managed,
        Operation::Blocked if recorded => Class::Vanished,
        Operation::Blocked => return None,
    })
}

/// Classifies the user scope of one harness against its records.
fn classify_user(
    adapter: &dyn Adapter,
    read: &ScopeRead,
    store: &StoreView,
    desired: &BTreeMap<String, Desired>,
    notices: &mut Vec<String>,
) -> Vec<EntryInventory> {
    let harness = adapter.harness();
    let records: BTreeMap<String, Record> = store
        .state
        .records
        .iter()
        .filter(|(key, _)| key.harness == harness && key.scope == Scope::User)
        .map(|(key, record)| (key.name.clone(), record.clone()))
        .collect();
    let pending: BTreeSet<&String> = records
        .iter()
        .filter(|(_, record)| matches!(record.state, RecordState::Pending { .. }))
        .map(|(name, _)| name)
        .collect();
    let owned: BTreeMap<String, Record> = records
        .iter()
        .filter(|(name, _)| !pending.contains(name))
        .map(|(name, record)| (name.clone(), record.clone()))
        .collect();
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    if let Some(harness_read) = &read.read {
        let planned = plan_harness(&PlanInput {
            adapter,
            read: harness_read,
            records: &owned,
            desired,
            take_over: &BTreeSet::new(),
            relocate: false,
        });
        if planned.status == PlanStatus::UnsupportedRoot {
            notices.push(adapter.texts().unsupported_root.to_owned());
        }
        for entry in &planned.entries {
            let present = read.entries.contains_key(&entry.name);
            if let Some(class) = class_of(entry.operation, owned.contains_key(&entry.name), present)
            {
                classes.insert(entry.name.clone(), class);
            }
        }
    }
    for name in &pending {
        classes.insert((*name).clone(), Class::Pending);
    }
    let names: BTreeSet<&String> = read.entries.keys().chain(classes.keys()).collect();
    names
        .into_iter()
        .map(|name| {
            let class = classes.get(name).copied().unwrap_or(Class::Unmanaged);
            let mut entry = EntryInventory::new(name, class);
            let hashes = owned.get(name).and_then(Record::hashes);
            let actual = read
                .entries
                .get(name)
                .and_then(|entry| entry.hash.as_deref());
            match hashes {
                Some(hashes) if hashes.written => {
                    entry.hash = Some(hashes.readback.clone());
                    if let Some(actual) = actual.filter(|actual| *actual != hashes.readback) {
                        entry.display_hash = store.display(actual);
                    }
                }
                // A taken-over record holds the foreign entry's hash.
                Some(hashes) => {
                    entry.display_hash = store.display(actual.unwrap_or(&hashes.readback));
                }
                None if class != Class::Pending => {
                    entry.display_hash = actual.and_then(|actual| store.display(actual));
                }
                None => {}
            }
            entry
        })
        .collect()
}

/// The inventory of one scope: the user scope classified against the write
/// state, every other scope as `unmanaged` or `local`.
fn scope_inventory(
    adapter: &dyn Adapter,
    read: &ScopeRead,
    store: &StoreView,
    desired: &BTreeMap<String, Desired>,
    notices: &mut Vec<String>,
) -> ScopeInventory {
    let mut entries = match read.scope {
        DiscoveredScope::User => classify_user(adapter, read, store, desired, notices),
        DiscoveredScope::Local { .. } => read
            .entries
            .keys()
            .map(|name| EntryInventory::new(name, Class::Local))
            .collect(),
        DiscoveredScope::Project { .. } => read
            .entries
            .iter()
            .map(|(name, discovered)| {
                let mut entry = EntryInventory::new(name, Class::Unmanaged);
                entry.display_hash = discovered
                    .hash
                    .as_deref()
                    .and_then(|hash| store.display(hash));
                entry
            })
            .collect(),
    };
    for entry in &mut entries {
        let Some(discovered) = read.entries.get(&entry.name) else {
            continue;
        };
        entry.approval = discovered.approval;
        if let Some(raw) = &discovered.raw {
            let imported = adapter.import(read, &entry.name);
            entry.view = Some(entry_view(adapter.harness(), raw, imported.as_ref().ok()));
            entry.import = Some(ImportSummary::of(&imported));
        }
    }
    ScopeInventory {
        scope: read.scope.clone(),
        file: read.file.clone(),
        present: read.present,
        trust: read.trust,
        orphaned: read.orphaned,
        error: read.error.clone(),
        entries,
    }
}

/// Whether a narrower scope's entry wins over the user entry in its
/// project: not when the harness ignores it (an untrusted Codex project, a
/// disabled `.mcp.json` entry).
fn shadows(read: &ScopeRead, name: &str) -> Option<Shadow> {
    let entry = read.entries.get(name)?;
    let (scope, path) = match &read.scope {
        DiscoveredScope::User => return None,
        DiscoveredScope::Local { project } => ("local", project.clone()),
        DiscoveredScope::Project { root } => ("project", root.clone()),
    };
    let ignored = read.trust == Some(Trust::Untrusted) && entry.approval.is_none()
        || entry.approval == Some(Approval::Disabled);
    (!ignored).then_some(Shadow { scope, path })
}

/// The same-name groups over the value-read scopes of every harness.
fn same_name(reads: &[(&dyn Adapter, Vec<ScopeRead>)]) -> Vec<SameName> {
    type Found<'a> = (
        Place,
        std::result::Result<Imported, Vec<ImportGap>>,
        Option<&'a str>,
    );
    let mut by_name: BTreeMap<&str, Vec<Found<'_>>> = BTreeMap::new();
    for (adapter, scopes) in reads {
        for read in scopes {
            if matches!(read.scope, DiscoveredScope::Local { .. }) {
                continue;
            }
            for (name, discovered) in &read.entries {
                let place = Place {
                    harness: adapter.harness(),
                    scope: read.scope.as_str(),
                    file: read.file.clone(),
                };
                let found = (
                    place,
                    adapter.import(read, name),
                    discovered.hash.as_deref(),
                );
                by_name.entry(name).or_default().push(found);
            }
        }
    }
    by_name
        .into_iter()
        .filter(|(_, found)| found.len() > 1)
        .map(|(name, found)| {
            let imports: Option<Vec<&Imported>> = found
                .iter()
                .map(|(_, imported, _)| imported.as_ref().ok())
                .collect();
            let same_harness = found
                .iter()
                .all(|(place, _, _)| place.harness == found[0].0.harness);
            let verdict = match imports {
                Some(imports) if imports.iter().all(|other| imports[0].same_core(other)) => {
                    Verdict::Duplicate
                }
                Some(_) => Verdict::Conflict,
                // One harness: the semantic entry hashes still compare.
                None if same_harness
                    && found
                        .iter()
                        .all(|(_, _, hash)| hash.is_some() && *hash == found[0].2) =>
                {
                    Verdict::Duplicate
                }
                None => Verdict::Undetermined,
            };
            SameName {
                name: name.to_owned(),
                verdict,
                places: found.into_iter().map(|(place, _, _)| place).collect(),
            }
        })
        .collect()
}

/// Builds the inventory.
///
/// # Errors
///
/// Global errors only: an unknown current directory, a corrupt
/// `config.toml` or `state.toml`. A scope that cannot be read is data.
pub fn build(
    paths: &StorePaths,
    env: &Environment,
    options: &InventoryOptions,
    warnings: &mut Vec<String>,
) -> Result<Inventory> {
    let cwd = env.cwd.clone().ok_or(Error::CurrentDirUnavailable)?;
    let repository = repository_root(&cwd);
    let store = StoreView::load(paths, warnings)?;
    let resolution = load_resolution(paths, &store.config, &store.inputs);
    let mut notices = Vec::new();
    if !store.initialized {
        notices.push(format!(
            "the store at {} is not initialized: nothing is managed yet, and no display hashes are shown; run `d-mcp init`",
            paths.root.display()
        ));
    } else if store.key.is_none() {
        notices.push(
            "the store has no .hash-key, so no display hashes are shown; run `d-mcp init` to create it"
                .to_owned(),
        );
    }
    let reads: Vec<(&dyn Adapter, Vec<ScopeRead>)> = adapter::registry()
        .iter()
        .copied()
        .filter(|adapter| {
            options.harnesses.is_empty() || options.harnesses.contains(&adapter.harness())
        })
        .map(|adapter| (adapter, adapter.discover(env, &repository)))
        .collect();
    let mut harnesses = Vec::new();
    for (adapter, scopes) in &reads {
        // Held servers are left out, so their records classify by content.
        let desired: BTreeMap<String, Desired> = desired_for(*adapter, &resolution)
            .into_iter()
            .filter(|(_, desired)| matches!(desired, Desired::Render { .. }))
            .collect();
        let mut harness_notices = Vec::new();
        let mut inventories: Vec<ScopeInventory> = scopes
            .iter()
            .map(|read| scope_inventory(*adapter, read, &store, &desired, &mut harness_notices))
            .collect();
        for inventory in &mut inventories {
            if inventory.scope != DiscoveredScope::User {
                continue;
            }
            for entry in &mut inventory.entries {
                entry.shadowed_in = scopes
                    .iter()
                    .filter_map(|read| shadows(read, &entry.name))
                    .collect();
            }
        }
        harnesses.push(HarnessInventory {
            harness: adapter.harness(),
            scopes: inventories,
            notices: harness_notices,
        });
    }
    Ok(Inventory {
        store: paths.root.clone(),
        initialized: store.initialized,
        repository,
        display_hashes: store.key.is_some(),
        same_name: same_name(&reads),
        harnesses,
        notices,
    })
}

/// Runs `inventory` and renders it.
///
/// # Errors
///
/// See [`build`].
pub fn inventory(
    paths: &StorePaths,
    env: &Environment,
    options: &InventoryOptions,
    warnings: &mut Vec<String>,
) -> Result<Report> {
    let inventory = build(paths, env, options, warnings)?;
    Ok(Report::new(
        render::json(&inventory),
        render::text(&inventory),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_operations_map_to_classes() {
        assert_eq!(class_of(Operation::Drift, true, true), Some(Class::Drifted));
        assert_eq!(
            class_of(Operation::Collision, false, true),
            Some(Class::Unmanaged)
        );
        assert_eq!(
            class_of(Operation::IdenticalCollision, false, true),
            Some(Class::Unmanaged)
        );
        assert_eq!(
            class_of(Operation::Vanished, true, false),
            Some(Class::Vanished)
        );
        assert_eq!(
            class_of(Operation::Orphaned, true, false),
            Some(Class::Vanished)
        );
        assert_eq!(
            class_of(Operation::LocationChanged, true, false),
            Some(Class::LocationChanged)
        );
        assert_eq!(class_of(Operation::Add, false, false), None);
        assert_eq!(
            class_of(Operation::Unchanged, false, true),
            Some(Class::Unmanaged)
        );
        for operation in [
            Operation::Update,
            Operation::Remove,
            Operation::Unsupported,
            Operation::Unchanged,
            Operation::Held,
        ] {
            assert_eq!(class_of(operation, true, true), Some(Class::Managed));
        }
        assert_eq!(
            class_of(Operation::Blocked, true, true),
            Some(Class::Managed)
        );
        assert_eq!(
            class_of(Operation::Blocked, true, false),
            Some(Class::Vanished)
        );
        assert_eq!(class_of(Operation::Blocked, false, false), None);
        assert_eq!(Class::LocationChanged.as_str(), "location-changed");
        assert_eq!(Verdict::Undetermined.as_str(), "undetermined");
    }
}
