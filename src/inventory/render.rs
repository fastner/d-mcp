//! The JSON document and the text rendering of `inventory`. Entry content
//! appears only as the redacted [`crate::redact::EntryView`].

use serde_json::{Value, json};

use super::{EntryInventory, Inventory, ScopeInventory};

fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn entry_json(entry: &EntryInventory) -> Value {
    json!({
        "name": entry.name,
        "class": entry.class,
        "hash": entry.hash,
        "display_hash": entry.display_hash,
        "shadowed_in": entry.shadowed_in.iter().map(|shadow| json!({
            "scope": shadow.scope,
            "path": display(&shadow.path),
        })).collect::<Vec<_>>(),
        "approval": entry.approval,
        "view": entry.view,
        "import": entry.import,
    })
}

fn scope_json(scope: &ScopeInventory) -> Value {
    json!({
        "scope": scope.scope.as_str(),
        "path": scope.scope.path().map(display),
        "file": display(&scope.file),
        "present": scope.present,
        "trust": scope.trust,
        "orphaned": scope.orphaned,
        "error": scope.error.as_ref().map(|error| json!({
            "code": error.code(),
            "hint": error.hint(),
        })),
        "entries": scope.entries.iter().map(entry_json).collect::<Vec<_>>(),
    })
}

/// The `data` document.
#[must_use]
pub fn json(inventory: &Inventory) -> Value {
    json!({
        "store": display(&inventory.store),
        "initialized": inventory.initialized,
        "repository": display(&inventory.repository),
        "display_hashes": inventory.display_hashes,
        "harnesses": inventory.harnesses.iter().map(|harness| json!({
            "harness": harness.harness.as_str(),
            "notices": harness.notices,
            "scopes": harness.scopes.iter().map(scope_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "same_name": inventory.same_name.iter().map(|group| json!({
            "name": group.name,
            "verdict": group.verdict,
            "places": group.places.iter().map(|place| json!({
                "harness": place.harness.as_str(),
                "scope": place.scope,
                "file": display(&place.file),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "notices": inventory.notices,
    })
}

fn scope_heading(harness: &str, scope: &ScopeInventory) -> String {
    let mut heading = format!("{harness} {}", scope.scope.as_str());
    match scope.scope.path() {
        Some(path) if scope.file.parent() != Some(path) => {
            heading.push_str(&format!(" {}", display(path)));
        }
        _ => {}
    }
    if !scope.file.as_os_str().is_empty() {
        heading.push_str(&format!(" {}", display(&scope.file)));
    }
    if let Some(trust) = scope.trust {
        heading.push_str(&format!(" (trust: {})", trust.as_str()));
    }
    if scope.orphaned {
        heading.push_str(" (orphaned: the project no longer exists)");
    }
    if !scope.present && scope.error.is_none() {
        heading.push_str(" (not present)");
    }
    heading
}

fn entry_line(entry: &EntryInventory) -> String {
    let mut line = format!("  {}\t{}", entry.name, entry.class.as_str());
    if let Some(hash) = &entry.hash {
        line.push_str(&format!("\thash={hash}"));
    }
    if let Some(hash) = &entry.display_hash {
        line.push_str(&format!("\tdisplay_hash={hash}"));
    }
    if let Some(approval) = entry.approval {
        line.push_str(&format!("\tapproval={}", approval.as_str()));
    }
    if let Some(view) = &entry.view {
        line.push_str(&format!("\t{}", view.text()));
    }
    if let Some(import) = &entry.import
        && !import.ok
    {
        let gaps: Vec<String> = import
            .gaps
            .iter()
            .map(|gap| {
                if gap.field.is_empty() {
                    gap.reason.as_str().to_owned()
                } else {
                    format!("{} ({})", gap.field, gap.reason.as_str())
                }
            })
            .collect();
        line.push_str(&format!("\tnot importable: {}", gaps.join(", ")));
    }
    for shadow in &entry.shadowed_in {
        line.push_str(&format!(
            "\tshadowed by {} {}",
            shadow.scope,
            display(&shadow.path)
        ));
    }
    line
}

/// The text rendering.
#[must_use]
pub fn text(inventory: &Inventory) -> String {
    let mut lines = vec![format!("Repository: {}", display(&inventory.repository))];
    for harness in &inventory.harnesses {
        for scope in &harness.scopes {
            lines.push(scope_heading(harness.harness.as_str(), scope));
            if let Some(error) = &scope.error {
                lines.push(format!("  error [{}]: {}", error.code(), error.hint()));
            }
            if scope.entries.is_empty() && scope.error.is_none() && scope.present {
                lines.push("  (no entries)".to_owned());
            }
            lines.extend(scope.entries.iter().map(entry_line));
        }
        lines.extend(
            harness
                .notices
                .iter()
                .map(|notice| format!("  note: {notice}")),
        );
    }
    if !inventory.same_name.is_empty() {
        lines.push("Same name in several scopes:".to_owned());
        for group in &inventory.same_name {
            let places: Vec<String> = group
                .places
                .iter()
                .map(|place| format!("{}:{}", place.harness.as_str(), place.scope))
                .collect();
            lines.push(format!(
                "  {}\t{}\t{}",
                group.name,
                group.verdict.as_str(),
                places.join(", ")
            ));
        }
    }
    lines.extend(
        inventory
            .notices
            .iter()
            .map(|notice| format!("note: {notice}")),
    );
    lines.join("\n")
}
