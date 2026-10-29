//! `d-mcp status`: per harness and server, the desired, recorded and actual
//! state, the operation the next sync would run, the variables the
//! definition reads that the current shell lacks, and with `--auth` the
//! login state. Read-only: no lock, no writes, no `.hash-key`.
//!
//! - **Operation** comes from the read-only plan of `sync::plan_location`, the
//!   one a dry-run sync makes (pending records settled in memory), not from
//!   a second classifier.
//! - **Missing variables** come from [`Definition::forwarded_env`] (stdio
//!   forwards, `bearer_env`, `header_env` and `client_secret_env`). Only the
//!   current shell is checked; a set but empty variable counts as missing.
//!   The output always says that a harness launched from a GUI can see
//!   another environment.
//! - **Login state** (`--auth`) uses the adapters' login-state capability;
//!   for Claude Code that runs `claude mcp get`, which connects to the
//!   server, and the output warns about it.
//! - **Hashes** follow the inventory: `hash` only for a record d-mcp wrote
//!   itself, every other hash as a keyed `display_hash`.
//! - **Errors** of a harness location are data; the command exits 0.

mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::adapter::{self, Adapter, LoginState, Scope, VarLookup};
use crate::config::Harness;
use crate::definition::Definition;
use crate::error::Result;
use crate::inventory::StoreView;
use crate::output::Report;
use crate::process::ProcessRunner;
use crate::resolver::{Resolution, resolve};
use crate::source::{scan, source_specs};
use crate::state::{Record, RecordKey, RecordState};
use crate::store::{Environment, StorePaths};
use crate::sync::{
    Desired, HeldReason, LocationPlan, Operation, PlanStatus, PlannedEntry, desired_for,
    plan_location,
};
use crate::validate::validate;

/// The caveat every `status` output carries.
pub const ENVIRONMENT_CAVEAT: &str = "variables are checked in this shell only; a harness launched from a GUI (a desktop app, a dock or a launcher) can see a different environment";

/// The warning of `status --auth` and `doctor --auth`.
pub const AUTH_WARNING: &str = "--auth runs `claude mcp get` for every Claude Code entry that can use OAuth, which connects to that server";

/// What the resolver wants for a server in one harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    /// Selected for the harness and resolved to a definition.
    Resolved,
    /// Selected for the harness and held (see the reason).
    Held,
    /// Not selected for the harness (it has a record only).
    NotSelected,
}

impl DesiredState {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DesiredState::Resolved => "resolved",
            DesiredState::Held => "held",
            DesiredState::NotSelected => "not-selected",
        }
    }
}

/// What the write state records for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedState {
    /// d-mcp owns the entry.
    Owned,
    /// A write of d-mcp was interrupted; the next sync settles it.
    Pending,
    /// No record.
    None,
}

impl RecordedState {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RecordedState::Owned => "owned",
            RecordedState::Pending => "pending",
            RecordedState::None => "none",
        }
    }
}

/// Whether the entry exists in the harness file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActualState {
    /// The entry exists.
    Present,
    /// The entry does not exist.
    Absent,
    /// The harness could not be read (or is not present).
    Unknown,
}

impl ActualState {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ActualState::Present => "present",
            ActualState::Absent => "absent",
            ActualState::Unknown => "unknown",
        }
    }
}

/// The status of a harness location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationState {
    /// Read and planned.
    Read,
    /// Neither the harness file nor its binary exists.
    NotPresent,
    /// The location could not be resolved or read (see the error).
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
            LocationState::Read => "read",
            LocationState::NotPresent => "not-present",
            LocationState::Failed => "failed",
            LocationState::LocationChanged => "location-changed",
            LocationState::Unsupported => "unsupported",
        }
    }
}

/// The status of one server in one harness.
#[derive(Debug, Clone)]
pub struct EntryStatus {
    /// The server (entry) name.
    pub name: String,
    /// What the resolver wants.
    pub desired: DesiredState,
    /// The hold reason, for `held`.
    pub reason: Option<HeldReason>,
    /// The source of the winning (or holding) definition.
    pub source: Option<String>,
    /// What the write state records.
    pub recorded: RecordedState,
    /// `<harness>:<scope>` of an adopted entry.
    pub adopted_from: Option<String>,
    /// How often a `vanished` re-add was promoted.
    pub vanish_count: u64,
    /// Whether the entry exists.
    pub actual: ActualState,
    /// The plain owned hash, for a record d-mcp wrote itself.
    pub hash: Option<String>,
    /// The keyed display hash of every other hash.
    pub display_hash: Option<String>,
    /// The operation the next sync would run; `None` when the harness was
    /// not planned.
    pub operation: Option<Operation>,
    /// The variables the definition reads that are unset or empty here.
    pub missing_variables: Vec<String>,
    /// The login state, with `--auth`.
    pub login: Option<LoginState>,
}

/// The status of one harness location.
#[derive(Debug, Clone)]
pub struct HarnessStatus {
    /// The harness.
    pub harness: Harness,
    /// The scope.
    pub scope: Scope,
    /// The harness file, when it was resolved.
    pub file: Option<PathBuf>,
    /// The location status.
    pub state: LocationState,
    /// Why the location failed.
    pub error: Option<crate::error::Error>,
    /// Location-level notices.
    pub notices: Vec<String>,
    /// The entries, sorted by name.
    pub entries: Vec<EntryStatus>,
}

/// The whole status.
#[derive(Debug, Clone)]
pub struct Status {
    /// The store root.
    pub store: PathBuf,
    /// Whether the store is initialized.
    pub initialized: bool,
    /// Whether display hashes are shown.
    pub display_hashes: bool,
    /// Whether `--auth` was given.
    pub auth: bool,
    /// The harness locations, in adapter order.
    pub harnesses: Vec<HarnessStatus>,
    /// Every missing variable, sorted and deduplicated.
    pub missing_variables: Vec<String>,
    /// Command-level notices; the first is always [`ENVIRONMENT_CAVEAT`].
    pub notices: Vec<String>,
}

/// The options of `status`.
#[derive(Default)]
pub struct StatusOptions<'a> {
    /// The harnesses to show; empty means every adapter.
    pub harnesses: BTreeSet<Harness>,
    /// The servers to show; empty means every selected or recorded one.
    pub names: BTreeSet<String>,
    /// Query the login state.
    pub auth: bool,
    /// Looks up a variable of the current shell; `None` reads the process
    /// environment.
    pub var: Option<VarLookup<'a>>,
}

/// The variables of a definition that are unset or empty in the shell.
pub(crate) fn missing_variables(definition: &Definition, var: VarLookup<'_>) -> Vec<String> {
    definition
        .forwarded_env()
        .into_iter()
        .filter(|name| var(name).is_none_or(|value| value.is_empty()))
        .collect()
}

/// The plain and the keyed hash of an entry, as `inventory` shows them: the
/// owned read-back hash plain only when d-mcp wrote it, and the actual hash
/// keyed when it differs from that (or always, for a record d-mcp did not
/// write and for an unrecorded entry).
pub(crate) fn shown_hashes(
    store: &StoreView,
    record: Option<&Record>,
    actual: Option<&str>,
) -> (Option<String>, Option<String>) {
    match record.and_then(Record::hashes) {
        Some(hashes) if hashes.written => {
            let keyed = actual
                .filter(|actual| *actual != hashes.readback)
                .and_then(|actual| store.display(actual));
            (Some(hashes.readback.clone()), keyed)
        }
        Some(hashes) => (None, store.display(actual.unwrap_or(&hashes.readback))),
        None => (None, actual.and_then(|actual| store.display(actual))),
    }
}

struct Context<'a> {
    env: &'a Environment,
    store: &'a StoreView,
    resolution: &'a Resolution,
    options: &'a StatusOptions<'a>,
    var: VarLookup<'a>,
}

fn location_status(context: &Context<'_>, adapter: &dyn Adapter, scope: Scope) -> HarnessStatus {
    let harness = adapter.harness();
    let desired = desired_for(adapter, context.resolution);
    let mut settled = context.store.state.clone();
    let plan = plan_location(
        adapter,
        context.env,
        scope,
        &mut settled,
        &desired,
        &context.options.names,
    );
    let mut status = HarnessStatus {
        harness,
        scope,
        file: None,
        state: LocationState::Read,
        error: None,
        notices: Vec::new(),
        entries: Vec::new(),
    };
    let (read, planned) = match plan {
        LocationPlan::Failed { location, error } => {
            status.file = location.map(|location| location.file);
            status.state = LocationState::Failed;
            status.error = Some(error);
            (None, Vec::new())
        }
        LocationPlan::NotPresent { location } => {
            status.file = Some(location.file);
            status.state = LocationState::NotPresent;
            status.notices.push("harness not present".to_owned());
            (None, Vec::new())
        }
        LocationPlan::Planned { read, planned, .. } => {
            status.file = Some(read.location.file.clone());
            status.state = match planned.status {
                PlanStatus::Ready => LocationState::Read,
                PlanStatus::LocationChanged => LocationState::LocationChanged,
                PlanStatus::UnsupportedRoot => {
                    status
                        .notices
                        .push(adapter.texts().unsupported_root.to_owned());
                    LocationState::Unsupported
                }
            };
            (Some(read), planned.entries)
        }
    };
    let in_scope =
        |name: &str| context.options.names.is_empty() || context.options.names.contains(name);
    let key = |name: &str| RecordKey {
        harness,
        scope,
        name: name.to_owned(),
    };
    let recorded: BTreeSet<&str> = context
        .store
        .state
        .records
        .keys()
        .filter(|record| record.harness == harness && record.scope == scope)
        .map(|record| record.name.as_str())
        .collect();
    let names: BTreeSet<&str> = desired
        .keys()
        .map(String::as_str)
        .chain(recorded)
        .filter(|name| in_scope(name))
        .collect();
    let planned: BTreeMap<&str, &PlannedEntry> = planned
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();
    for name in names {
        let record = context.store.state.records.get(&key(name));
        let server = context
            .resolution
            .servers
            .iter()
            .find(|server| server.name == name);
        let (state, reason) = match desired.get(name) {
            Some(Desired::Render { .. }) => (DesiredState::Resolved, None),
            Some(Desired::Held(reason)) => (DesiredState::Held, Some(*reason)),
            None => (DesiredState::NotSelected, None),
        };
        let actual = read
            .as_ref()
            .map(|read| read.entries.get(name).map(|entry| entry.hash.as_str()));
        let (hash, display_hash) = shown_hashes(context.store, record, actual.flatten());
        let missing = match (state, server.and_then(|server| server.definition.as_ref())) {
            (DesiredState::Resolved, Some(definition)) => {
                missing_variables(definition, context.var)
            }
            _ => Vec::new(),
        };
        let operation = match (planned.get(name), state) {
            (Some(entry), _) => Some(entry.operation),
            // The planner skips a held server d-mcp does not own.
            (None, DesiredState::Held) if read.is_some() => Some(Operation::Held),
            (None, _) => None,
        };
        status.entries.push(EntryStatus {
            name: name.to_owned(),
            desired: state,
            reason,
            source: server
                .filter(|_| state != DesiredState::NotSelected)
                .and_then(|server| server.source.clone()),
            recorded: match record.map(|record| &record.state) {
                Some(RecordState::Owned(_)) => RecordedState::Owned,
                Some(RecordState::Pending { .. }) => RecordedState::Pending,
                None => RecordedState::None,
            },
            adopted_from: record
                .and_then(|record| record.meta.adopted_from.as_ref())
                .map(crate::state::AdoptedFrom::label),
            vanish_count: record.map_or(0, |record| record.meta.vanish_count),
            actual: match actual {
                Some(Some(_)) => ActualState::Present,
                Some(None) => ActualState::Absent,
                None => ActualState::Unknown,
            },
            hash,
            display_hash,
            operation,
            missing_variables: missing,
            login: None,
        });
    }
    if context.options.auth
        && let Some(read) = &read
    {
        let present: Vec<String> = status
            .entries
            .iter()
            .filter(|entry| read.entries.contains_key(&entry.name))
            .map(|entry| entry.name.clone())
            .collect();
        let states = login_states(adapter, context.env, &present);
        for entry in &mut status.entries {
            entry.login = Some(
                states
                    .get(&entry.name)
                    .copied()
                    .unwrap_or(LoginState::Unknown),
            );
        }
    }
    status
}

/// The login states of the named entries, through the adapter's
/// capability; no call without names.
pub(crate) fn login_states(
    adapter: &dyn Adapter,
    env: &Environment,
    names: &[String],
) -> BTreeMap<String, LoginState> {
    if names.is_empty() {
        return BTreeMap::new();
    }
    adapter.login_state(env, &ProcessRunner::default(), names)
}

/// Builds the status.
///
/// # Errors
///
/// Global errors only: a corrupt `config.toml` or `state.toml`. A harness
/// location that cannot be read is data.
pub fn build(
    paths: &StorePaths,
    env: &Environment,
    options: &StatusOptions<'_>,
    warnings: &mut Vec<String>,
) -> Result<Status> {
    let store = StoreView::load(paths, warnings)?;
    let mut snapshots: Vec<_> = source_specs(paths, &store.config)
        .iter()
        .map(scan)
        .collect();
    validate(&mut snapshots, adapter::registry());
    let resolution = resolve(&snapshots, &store.config.selections);
    let process_env = |name: &str| std::env::var_os(name);
    let var: VarLookup<'_> = options.var.unwrap_or(&process_env);
    let mut notices = vec![ENVIRONMENT_CAVEAT.to_owned()];
    if !store.initialized {
        notices.push(format!(
            "the store at {} is not initialized: nothing is selected or managed yet; run `d-mcp init`",
            paths.root.display()
        ));
    }
    if options.auth {
        warnings.push(AUTH_WARNING.to_owned());
        notices.push(AUTH_WARNING.to_owned());
    }
    let context = Context {
        env,
        store: &store,
        resolution: &resolution,
        options,
        var,
    };
    let mut harnesses = Vec::new();
    for adapter in adapter::registry() {
        if !options.harnesses.is_empty() && !options.harnesses.contains(&adapter.harness()) {
            continue;
        }
        for scope in adapter.write_scopes() {
            harnesses.push(location_status(&context, *adapter, *scope));
        }
    }
    let shown: BTreeSet<&str> = harnesses
        .iter()
        .flat_map(|harness| harness.entries.iter().map(|entry| entry.name.as_str()))
        .collect();
    for name in &options.names {
        if !shown.contains(name.as_str()) {
            notices.push(format!("`{name}` is neither selected nor managed"));
        }
    }
    let missing: BTreeSet<String> = harnesses
        .iter()
        .flat_map(|harness| harness.entries.iter())
        .flat_map(|entry| entry.missing_variables.iter().cloned())
        .collect();
    Ok(Status {
        store: paths.root.clone(),
        initialized: store.initialized,
        display_hashes: store.key.is_some(),
        auth: options.auth,
        harnesses,
        missing_variables: missing.into_iter().collect(),
        notices,
    })
}

/// Runs `status` and renders it.
///
/// # Errors
///
/// See [`build`].
pub fn status(
    paths: &StorePaths,
    env: &Environment,
    options: &StatusOptions<'_>,
    warnings: &mut Vec<String>,
) -> Result<Report> {
    let status = build(paths, env, options, warnings)?;
    Ok(Report::new(render::json(&status), render::text(&status)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::parse_definition;
    use std::ffi::OsString;

    #[test]
    fn set_but_empty_variables_are_missing() {
        let definition = parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
             [env]\nA = { forward = true }\nB = { forward = true }\nC = { forward = true }\n",
            "a",
        )
        .unwrap();
        let lookup = |name: &str| match name {
            "A" => Some(OsString::from("set")),
            "B" => Some(OsString::new()),
            _ => None,
        };
        assert_eq!(missing_variables(&definition, &lookup), ["B", "C"]);
    }

    #[test]
    fn state_names_are_stable() {
        assert_eq!(DesiredState::NotSelected.as_str(), "not-selected");
        assert_eq!(RecordedState::Pending.as_str(), "pending");
        assert_eq!(ActualState::Unknown.as_str(), "unknown");
        assert_eq!(LocationState::LocationChanged.as_str(), "location-changed");
        assert_eq!(LoginState::NeedsLogin.as_str(), "needs-login");
    }
}
