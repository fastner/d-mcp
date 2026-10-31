//! `source add-git` and `source refresh`: rendering of
//! [`git_source::add_git`] and [`refresh::refresh`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::config::LOCAL_SOURCE_ID;
use crate::definition::Definition;
use crate::error::{Error, Result};
use crate::output::{Report, Ui};
use crate::redact::approval_view;
use crate::resolver::{FileEntry, SourceSnapshot};
use crate::source::git_source::{self, AddGitOptions};
use crate::source::offered_names;
use crate::source::refresh::{self, RefreshOptions};
use crate::store::{Environment, StorePaths};

use super::{display, prefix, source_json, source_json_with, user_spec};

/// Runs `source add-git` and renders its report: the source object with
/// the pinned commit, and the names the snapshot offers.
pub(super) fn add_git(
    paths: &StorePaths,
    env: &Environment,
    ui: &mut Ui<'_>,
    options: &AddGitOptions<'_>,
) -> Result<Report> {
    let outcome = git_source::add_git(paths, env, options)?;
    for warning in &outcome.warnings {
        ui.warn(&warning.to_string());
    }
    let id = &outcome.entry.id;
    if !outcome.snapshot.available {
        ui.warn(&format!(
            "source `{id}` has no servers/ directory at commit {}; it is unavailable until one exists",
            outcome.commit.short()
        ));
    }
    let mut offered = offered_names(&outcome.snapshot);
    offered.sort();
    let dry_run = options.dry_run;
    let mut text = format!(
        "{}{} Git source `{id}` (priority {}) at {}: {}",
        prefix(dry_run),
        if dry_run { "added" } else { "Added" },
        outcome.entry.priority,
        outcome.commit.short(),
        options.url,
    );
    text.push_str(&format!(
        "\n  location: {}",
        display(outcome.spec.location())
    ));
    if offered.is_empty() {
        text.push_str("\n  offers no servers");
    } else {
        text.push_str(&format!("\n  offers: {}", offered.join(", ")));
    }
    Ok(Report::new(
        json!({
            "source": source_json_with(&outcome.spec, outcome.snapshot.available),
            "offered": offered,
            "dry_run": dry_run,
        }),
        text,
    ))
}

/// How a definition changed between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Added,
    Changed,
    Removed,
}

impl Change {
    fn as_str(self) -> &'static str {
        match self {
            Change::Added => "added",
            Change::Changed => "changed",
            Change::Removed => "removed",
        }
    }
}

/// The files of a snapshot by conflict key.
fn by_key(snapshot: &SourceSnapshot) -> BTreeMap<&str, &FileEntry> {
    snapshot
        .files
        .iter()
        .filter_map(|file| file.key.as_deref().map(|key| (key, file)))
        .collect()
}

/// What identifies a file's content for the change set: the
/// `definition_hash` of a valid definition, or the broken rule. A
/// formatting-only change keeps it.
fn identity(file: &FileEntry) -> (Option<String>, Option<String>) {
    match &file.outcome {
        Ok(definition) => (Some(definition.definition_hash()), None),
        Err(problem) => (None, Some(problem.rule.to_string())),
    }
}

/// One entry of the change set, through the approval view.
fn change_json(
    server: &str,
    change: Change,
    old: Option<&FileEntry>,
    new: Option<&FileEntry>,
) -> Value {
    let shown = new.or(old);
    let definition: Option<&Definition> = new.and_then(|file| file.outcome.as_ref().ok());
    let problem = new.and_then(|file| file.outcome.as_ref().err());
    json!({
        "server": server,
        "change": change.as_str(),
        "file": shown.map(|file| file.file.as_str()),
        "definition_hash": definition.map(Definition::definition_hash),
        "previous_hash": old
            .and_then(|file| file.outcome.as_ref().ok())
            .map(Definition::definition_hash),
        "problem": problem.map(|problem| json!({
            "rule": problem.rule,
            "field": problem.field,
            "line": problem.line,
            "column": problem.column,
        })),
        "findings": new.map(|file| &file.findings),
        "view": definition.map(approval_view),
        "approval": new.and_then(|file| file.gate),
    })
}

/// Runs `source refresh` and renders the change set between the old and
/// the new pin.
pub(super) fn refresh(
    paths: &StorePaths,
    env: &Environment,
    ui: &mut Ui<'_>,
    options: &RefreshOptions<'_>,
) -> Result<Report> {
    paths.require_initialized()?;
    let config = paths.load_config()?;
    let id = options.id;
    match config.source(id) {
        None if id == LOCAL_SOURCE_ID => {
            return directory_refresh(paths, ui, id, &config, options.dry_run);
        }
        None => return Err(Error::SourceNotFound { id: id.to_owned() }),
        Some(entry) if entry.git().is_none() => {
            return directory_refresh(paths, ui, id, &config, options.dry_run);
        }
        Some(_) => {}
    }
    let outcome = refresh::refresh(paths, env, options)?;
    for warning in &outcome.warnings {
        ui.warn(&warning.to_string());
    }
    if !outcome.new.available {
        ui.warn(&format!(
            "source `{id}` has no servers/ directory at commit {}; it is unavailable until one exists",
            outcome.commit.short()
        ));
    }
    let old = by_key(&outcome.old);
    let new = by_key(&outcome.new);
    let names: BTreeSet<&str> = old.keys().chain(new.keys()).copied().collect();
    let mut changes = Vec::new();
    let mut text_lines = Vec::new();
    for name in names {
        let (before, after) = (old.get(name).copied(), new.get(name).copied());
        let change = match (before, after) {
            (None, Some(_)) => Change::Added,
            (Some(_), None) => Change::Removed,
            (Some(before), Some(after)) if identity(before) != identity(after) => Change::Changed,
            _ => continue,
        };
        let entry = change_json(name, change, before, after);
        let mut line = format!("\n  {} {name}", change.as_str());
        if let Some(hash) = entry["definition_hash"].as_str() {
            line.push_str(&format!(": definition_hash {hash}"));
        }
        if let Some(rule) = entry["problem"]["rule"].as_str() {
            line.push_str(&format!(": invalid (rule `{rule}`)"));
        }
        if let Some(definition) = after.and_then(|file| file.outcome.as_ref().ok()) {
            line.push_str(&format!("\n    view: {}", approval_view(definition).text()));
        }
        for finding in after.map_or(&[][..], |file| file.findings.as_slice()) {
            line.push_str(&format!(
                "\n    finding: {} ({}) {}",
                finding.rule.as_str(),
                finding.launcher,
                serde_json::to_string(&finding.args).unwrap_or_default()
            ));
        }
        text_lines.push(line);
        changes.push(entry);
    }
    let pins = crate::lockfile::LockFile::default();
    let mut spec = user_spec(paths, &outcome.entry, &pins);
    if let Some(git) = &mut spec.git {
        git.pin = Some(outcome.commit.clone());
    }
    let dry_run = options.dry_run;
    let moved = outcome.previous.as_ref() != Some(&outcome.commit);
    let mut text = if moved {
        format!(
            "{}{} Git source `{id}` from {} to {}{}",
            prefix(dry_run),
            if dry_run { "refreshed" } else { "Refreshed" },
            outcome.previous.as_ref().map_or("-", |pin| pin.short()),
            outcome.commit.short(),
            if outcome.rewrite {
                " (rewrite accepted)"
            } else {
                ""
            },
        )
    } else {
        format!(
            "Git source `{id}` is up to date at {}",
            outcome.commit.short()
        )
    };
    if changes.is_empty() {
        text.push_str("\n  no definition changed");
    }
    for line in text_lines {
        text.push_str(&line);
    }
    if !changes.is_empty() {
        text.push_str(
            "\n  a moved pin approves nothing: changed servers stay held until `d-mcp approve`",
        );
    }
    Ok(Report::new(
        json!({
            "source": source_json_with(&spec, outcome.new.available),
            "previous_commit": outcome.previous.as_ref().map(|pin| pin.as_str()),
            "commit": outcome.commit.as_str(),
            "rewrite": outcome.rewrite,
            "recloned": outcome.recloned,
            "changes": changes,
            "dry_run": dry_run,
        }),
        text,
    ))
}

/// `source refresh` of a directory source: nothing to fetch.
fn directory_refresh(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    id: &str,
    config: &crate::config::Config,
    dry_run: bool,
) -> Result<Report> {
    ui.warn(&format!(
        "source `{id}` is a directory source; it is read as it is and has nothing to refresh"
    ));
    let pins = crate::lockfile::load(paths)?;
    let spec = crate::source::source_specs(paths, config, &pins)
        .into_iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| Error::SourceNotFound { id: id.to_owned() })?;
    Ok(Report::new(
        json!({
            "source": source_json(&spec),
            "previous_commit": null,
            "commit": null,
            "rewrite": false,
            "recloned": false,
            "changes": [],
            "dry_run": dry_run,
        }),
        format!("Source `{id}` is a directory source; nothing to refresh"),
    ))
}
