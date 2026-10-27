//! The `unmanage` command: drops write-state records and selections, and
//! leaves the harness entries as they are.

use std::collections::BTreeSet;

use serde_json::json;

use crate::config::{Harness, HarnessSet, Selection};
use crate::error::{Error, Result};
use crate::output::Report;
use crate::state::{self, RecordKey};
use crate::store::{StoreLock, StorePaths};

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
    let before = config.selection(server).cloned();
    let converted = matches!(
        before,
        Some(Selection {
            harnesses: HarnessSet::All,
            ..
        })
    );
    let after = before.as_ref().map(|selection| {
        let current: BTreeSet<Harness> = match &selection.harnesses {
            HarnessSet::All => Harness::ALL.into_iter().collect(),
            HarnessSet::Only(set) => set.clone(),
        };
        current
            .difference(&removed)
            .copied()
            .collect::<BTreeSet<_>>()
    });
    let selection_removed = after.as_ref().is_some_and(BTreeSet::is_empty);
    match &after {
        Some(set) if set.is_empty() => {
            config.unselect(server);
        }
        Some(set) => {
            config.select(Selection {
                server: server.to_owned(),
                harnesses: HarnessSet::Only(set.clone()),
            });
        }
        None => {}
    }
    if let Some(lock) = &lock {
        // The state goes first: a crash in between leaves a selection without
        // a record (a Collision on the next sync), never a record without a
        // selection (which would plan a Remove).
        for key in &keys {
            state.records.remove(key);
        }
        warnings.extend(state::save(paths, lock, &state)?.map(|w| w.to_string()));
        if after.is_some() {
            warnings.extend(paths.save_config(lock, &config)?.map(|w| w.to_string()));
        }
    }
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
