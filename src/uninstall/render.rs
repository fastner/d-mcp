//! The JSON document and the text rendering of `uninstall`. Entries carry
//! names, actions, outcomes and the names of replaced fields and their
//! variables only, never a value or a hash.

use serde_json::{Value, json};

use super::{EntryUninstall, HarnessUninstall, Uninstall};
use crate::redact::safe_key;

fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn entry_json(entry: &EntryUninstall) -> Value {
    json!({
        "name": entry.name,
        "action": entry.action.as_str(),
        "outcome": entry.outcome.as_str(),
        "adopted_from": entry.adopted_from,
        "replaced_fields": entry.replaced_fields.iter().map(|field| json!({
            "field": field.field,
            "var": safe_key(&field.var),
        })).collect::<Vec<_>>(),
        "notice": entry.notice,
    })
}

fn harness_json(harness: &HarnessUninstall) -> Value {
    json!({
        "harness": harness.harness.as_str(),
        "scope": harness.scope.as_str(),
        "file": harness.file.as_deref().map(display),
        "status": harness.state.as_str(),
        "error": harness.error.as_ref().map(|error| json!({
            "code": error.code(),
            "hint": error.hint(),
        })),
        "notices": harness.notices,
        "entries": harness.entries.iter().map(entry_json).collect::<Vec<_>>(),
    })
}

/// The `data` document.
#[must_use]
pub fn json(uninstall: &Uninstall) -> Value {
    let purge = &uninstall.purge;
    json!({
        "store": display(&uninstall.store),
        "apply": uninstall.apply,
        "dry_run": uninstall.dry_run,
        "harnesses": uninstall.harnesses.iter().map(harness_json).collect::<Vec<_>>(),
        "purge": {
            "requested": purge.requested,
            "follow_store_link": purge.follow_store_link,
            "store_root_symlink": purge.root_is_link,
            "target": display(&purge.target),
            "entries": purge.entries.iter().map(|path| display(path)).collect::<Vec<_>>(),
            "blocked": purge.blocked.iter().map(|(reason, harness)| json!({
                "reason": reason.as_str(),
                "harness": harness.map(|harness| harness.as_str()),
            })).collect::<Vec<_>>(),
            "deleted": purge.deleted,
        },
        "sync": uninstall.sync,
        "notices": uninstall.notices,
    })
}

/// The text rendering: per harness a heading, its error and notices, and
/// one line per entry (action, name, outcome) with its notice; then the
/// purge and the notices.
#[must_use]
pub fn text(uninstall: &Uninstall) -> String {
    let mut lines = Vec::new();
    for harness in &uninstall.harnesses {
        let file = harness
            .file
            .as_deref()
            .map_or_else(|| "-".to_owned(), display);
        lines.push(format!(
            "{} ({}): {file}: {}",
            harness.harness.as_str(),
            harness.scope.as_str(),
            harness.state.as_str()
        ));
        if let Some(error) = &harness.error {
            lines.push(format!("  error: {} [{}]", error.hint(), error.code()));
        }
        lines.extend(
            harness
                .notices
                .iter()
                .map(|notice| format!("  notice: {notice}")),
        );
        if harness.entries.is_empty() {
            lines.push("  (no managed entries)".to_owned());
        }
        for entry in &harness.entries {
            let mut line = format!(
                "  {}\t{}\t{}",
                entry.action.as_str(),
                entry.name,
                entry.outcome.as_str()
            );
            if let Some(origin) = &entry.adopted_from {
                line.push_str(&format!("\tadopted from {origin}"));
            }
            lines.push(line);
            lines.push(format!("    {}", entry.notice));
        }
    }
    let purge = &uninstall.purge;
    if purge.requested {
        let entries: Vec<String> = purge.entries.iter().map(|path| display(path)).collect();
        if !purge.blocked.is_empty() {
            let reasons: Vec<String> = purge
                .blocked
                .iter()
                .map(|(reason, harness)| match harness {
                    Some(harness) => format!("{} ({})", reason.as_str(), harness.as_str()),
                    None => reason.as_str().to_owned(),
                })
                .collect();
            lines.push(format!("Purge blocked: {}", reasons.join(", ")));
        } else if purge.deleted {
            lines.push(format!("Purged: {}", entries.join(", ")));
        } else {
            lines.push(format!("Purge would delete: {}", entries.join(", ")));
        }
    }
    lines.extend(uninstall.notices.iter().cloned());
    lines.join("\n")
}
