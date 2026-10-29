//! The read-only plan of one harness location, as a dry-run sync makes it:
//! locate and read the harness, settle `pending` records in memory against
//! what was read, and run the planner. `status` and `uninstall` build on
//! it; neither takes a second classifier.

use std::collections::{BTreeMap, BTreeSet};

use super::executor::settle_pending;
use super::{Desired, PlanInput, Planned, plan_harness};
use crate::adapter::{Adapter, HarnessRead, Location, Scope};
use crate::error::Error;
use crate::state::{Record, State};
use crate::store::Environment;

/// What [`plan_location`] found at one harness location.
#[derive(Debug)]
pub(crate) enum LocationPlan {
    /// The location could not be resolved, or its file could not be read.
    Failed {
        /// The location, when it was resolved.
        location: Option<Location>,
        /// Why.
        error: Error,
    },
    /// Neither the harness file nor its binary exists.
    NotPresent {
        /// The location.
        location: Location,
    },
    /// The harness was read and planned.
    Planned {
        /// What was read.
        read: HarnessRead,
        /// The plan.
        planned: Planned,
        /// The settled records of this harness and scope, by name.
        records: BTreeMap<String, Record>,
        /// What the in-memory settlement of `pending` records did.
        notices: Vec<String>,
    },
}

/// Plans one harness location read-only. `pending` records of the servers
/// in `only` (every server when it is empty) are settled into `state`
/// against what was read; pass a copy to keep the caller's state unchanged.
/// `desired` is what the resolver wants for the harness (empty plans every
/// record as no longer desired, which is what `uninstall` needs).
pub(crate) fn plan_location(
    adapter: &dyn Adapter,
    env: &Environment,
    scope: Scope,
    state: &mut State,
    desired: &BTreeMap<String, Desired>,
    only: &BTreeSet<String>,
) -> LocationPlan {
    let harness = adapter.harness();
    let location = match adapter.locate(env, scope) {
        Ok(location) => location,
        Err(error) => {
            return LocationPlan::Failed {
                location: None,
                error,
            };
        }
    };
    if !location.present() {
        return LocationPlan::NotPresent { location };
    }
    let read = match adapter.read(&location) {
        Ok(read) => read,
        Err(error) => {
            return LocationPlan::Failed {
                location: Some(location),
                error,
            };
        }
    };
    let in_scope = |name: &str| only.is_empty() || only.contains(name);
    let notices = settle_pending(state, harness, scope, &read, only);
    let records: BTreeMap<String, Record> = state
        .records
        .iter()
        .filter(|(key, _)| key.harness == harness && key.scope == scope && in_scope(&key.name))
        .map(|(key, record)| (key.name.clone(), record.clone()))
        .collect();
    let desired: BTreeMap<String, Desired> = desired
        .iter()
        .filter(|(name, _)| in_scope(name))
        .map(|(name, desired)| (name.clone(), desired.clone()))
        .collect();
    let planned = plan_harness(&PlanInput {
        adapter,
        read: &read,
        records: &records,
        desired: &desired,
        take_over: &BTreeSet::new(),
        relocate: false,
    });
    LocationPlan::Planned {
        read,
        planned,
        records,
        notices,
    }
}
