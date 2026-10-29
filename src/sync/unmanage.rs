//! The `unmanage` command: drops write-state records and selections, and
//! leaves the harness entries as they are.

use std::collections::BTreeSet;

use serde_json::json;

use crate::config::{Config, Harness, HarnessSet, Selection};
use crate::error::{Error, Result};
use crate::hashkey;
use crate::output::Report;
use crate::state::{self, RecordKey, State};
use crate::store::{StoreLock, StorePaths};

/// How dropping harnesses changed a server's selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SelectionChange {
    /// The selection changed (and `config.toml` must be written).
    pub(crate) changed: bool,
    /// The selection was `all` and became an explicit list (or was
    /// removed).
    pub(crate) converted: bool,
    /// The selection was removed, because no harness is left.
    pub(crate) removed: bool,
}

/// Drops `harnesses` from the selection of `server`, in memory. A selection
/// of `all` becomes an explicit list of the known harnesses minus the
/// dropped ones; an empty result removes the selection.
pub(crate) fn drop_from_selection(
    config: &mut Config,
    server: &str,
    harnesses: &BTreeSet<Harness>,
) -> SelectionChange {
    let Some(before) = config.selection(server).cloned() else {
        return SelectionChange::default();
    };
    let current: BTreeSet<Harness> = match &before.harnesses {
        HarnessSet::All => Harness::ALL.into_iter().collect(),
        HarnessSet::Only(set) => set.clone(),
    };
    if current.is_disjoint(harnesses) {
        return SelectionChange::default();
    }
    let after: BTreeSet<Harness> = current.difference(harnesses).copied().collect();
    let converted = before.harnesses == HarnessSet::All;
    if after.is_empty() {
        config.unselect(server);
    } else {
        config.select(Selection {
            server: server.to_owned(),
            harnesses: HarnessSet::Only(after.clone()),
        });
    }
    SelectionChange {
        changed: true,
        converted,
        removed: after.is_empty(),
    }
}

/// Releases records under a store lock the caller holds, together with
/// [`release_selections_under_lock`]: removes them from `state`, writes
/// `state.toml`, then drops their harnesses from the
/// selections in `config` and writes `config.toml` when a selection
/// changed. The harness entries are left as they are; this is `unmanage`
/// for a command that already holds the lock (`uninstall`). The state goes
/// first, as in `unmanage`.
///
/// The two writes are two calls, [`release_under_lock`] for the state and
/// then [`release_selections_under_lock`], so the caller knows which one an
/// error stopped: after a failed state write nothing changed; after a
/// failed config write the records are already released.
///
/// # Errors
///
/// The store write errors.
pub(crate) fn release_under_lock(
    paths: &StorePaths,
    lock: &StoreLock,
    state: &mut State,
    keys: &[RecordKey],
    warnings: &mut Vec<String>,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    for key in keys {
        state.records.remove(key);
    }
    warnings.extend(state::save(paths, lock, state)?.map(|w| w.to_string()));
    Ok(())
}

/// The second write of a release (see [`release_under_lock`]): drops the
/// harnesses of the released `keys` from the selections in `config` and
/// writes `config.toml` when a selection changed.
///
/// # Errors
///
/// The store write errors.
pub(crate) fn release_selections_under_lock(
    paths: &StorePaths,
    lock: &StoreLock,
    config: &mut Config,
    keys: &[RecordKey],
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut changed = false;
    let servers: BTreeSet<&str> = keys.iter().map(|key| key.name.as_str()).collect();
    for server in servers {
        let harnesses: BTreeSet<Harness> = keys
            .iter()
            .filter(|key| key.name == server)
            .map(|key| key.harness)
            .collect();
        changed |= drop_from_selection(config, server, &harnesses).changed;
    }
    if changed {
        warnings.extend(paths.save_config(lock, config)?.map(|w| w.to_string()));
    }
    Ok(())
}

/// Runs `unmanage`: removes the write-state records of a server (for the
/// given harnesses, or every harness with a record) and drops those
/// harnesses from its selection. The harness entries are left as they are.
///
/// # Errors
///
/// [`Error::NotManaged`] when no record matches, plus the store errors.
pub fn unmanage(
    paths: &StorePaths,
    server: &str,
    harnesses: &BTreeSet<Harness>,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<Report> {
    paths.require_initialized()?;
    let lock = if dry_run {
        None
    } else {
        Some(StoreLock::acquire(paths)?)
    };
    let mut config = paths.load_config()?;
    let mut state = state::load(paths)?;
    // Every mutating command creates a missing display-hash key.
    if let Some(lock) = &lock {
        warnings
            .extend(hashkey::ensure_under_lock(paths, lock)?.map(|warning| warning.to_string()));
    }
    let keys: Vec<RecordKey> = state
        .records
        .keys()
        .filter(|key| {
            key.name == server && (harnesses.is_empty() || harnesses.contains(&key.harness))
        })
        .cloned()
        .collect();
    if keys.is_empty() {
        return Err(Error::NotManaged {
            server: server.to_owned(),
        });
    }
    let removed: BTreeSet<Harness> = keys.iter().map(|key| key.harness).collect();
    let change = drop_from_selection(&mut config, server, &removed);
    if let Some(lock) = &lock {
        // The state goes first: a crash in between leaves a selection without
        // a record (a Collision on the next sync), never a record without a
        // selection (which would plan a Remove).
        for key in &keys {
            state.records.remove(key);
        }
        warnings.extend(state::save(paths, lock, &state)?.map(|w| w.to_string()));
        if change.changed {
            warnings.extend(paths.save_config(lock, &config)?.map(|w| w.to_string()));
        }
    }
    let SelectionChange {
        converted,
        removed: selection_removed,
        ..
    } = change;
    let names: Vec<&str> = removed.iter().map(|harness| harness.as_str()).collect();
    let selection: Option<Vec<&str>> = config.selection(server).map(|s| s.harnesses.names());
    let mut text = format!(
        "{}{} `{server}` for {}; the entries are left as they are",
        if dry_run { "Dry run: would have " } else { "" },
        if dry_run { "unmanaged" } else { "Unmanaged" },
        names.join(", ")
    );
    if converted {
        text.push_str(&format!(
            "\nThe selection `all` became an explicit list: {}",
            selection
                .as_ref()
                .map_or_else(|| "(none)".to_owned(), |s| s.join(", "))
        ));
    }
    if selection_removed {
        text.push_str(&format!("\nThe selection of `{server}` was removed"));
    }
    Ok(Report::new(
        json!({
            "server": server,
            "harnesses": names,
            "selection": selection,
            "converted_from_all": converted,
            "selection_removed": selection_removed,
            "dry_run": dry_run,
        }),
        text,
    ))
}
