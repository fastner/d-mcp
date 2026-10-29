//! The JSON document and the text rendering of `status`. Entries carry
//! names, states, hashes (see [`crate::hashkey`]) and variable names only,
//! never a value.

use serde_json::{Value, json};

use super::{EntryStatus, HarnessStatus, Status};

fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn entry_json(entry: &EntryStatus) -> Value {
    json!({
        "name": entry.name,
        "desired": entry.desired.as_str(),
        "reason": entry.reason.map(|reason| reason.as_str()),
        "source": entry.source,
        "recorded": entry.recorded.as_str(),
        "adopted_from": entry.adopted_from,
        "vanish_count": entry.vanish_count,
        "actual": entry.actual.as_str(),
        "hash": entry.hash,
        "display_hash": entry.display_hash,
        "operation": entry.operation.map(|operation| operation.as_str()),
        "missing_variables": entry.missing_variables,
        "login": entry.login.map(|login| login.as_str()),
    })
}

fn harness_json(harness: &HarnessStatus) -> Value {
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
pub fn json(status: &Status) -> Value {
    json!({
        "store": display(&status.store),
        "initialized": status.initialized,
        "display_hashes": status.display_hashes,
        "auth": status.auth,
        "missing_variables": status.missing_variables,
        "notices": status.notices,
        "harnesses": status.harnesses.iter().map(harness_json).collect::<Vec<_>>(),
    })
}

fn entry_line(entry: &EntryStatus) -> String {
    let mut line = format!("  {}\tdesired={}", entry.name, entry.desired.as_str());
    if let Some(reason) = entry.reason {
        line.push_str(&format!(" ({})", reason.as_str()));
    }
    line.push_str(&format!(
        "\trecorded={}\tactual={}",
        entry.recorded.as_str(),
        entry.actual.as_str()
    ));
    if let Some(operation) = entry.operation {
        line.push_str(&format!("\tnext sync: {}", operation.as_str()));
    }
    if let Some(hash) = &entry.hash {
        line.push_str(&format!("\thash={hash}"));
    }
    if let Some(hash) = &entry.display_hash {
        line.push_str(&format!("\tdisplay_hash={hash}"));
    }
    if let Some(origin) = &entry.adopted_from {
        line.push_str(&format!("\tadopted from {origin}"));
    }
    if entry.vanish_count > 0 {
        line.push_str(&format!("\tvanished {} times", entry.vanish_count));
    }
    if !entry.missing_variables.is_empty() {
        line.push_str(&format!(
            "\tmissing: {}",
            entry.missing_variables.join(", ")
        ));
    }
    if let Some(login) = entry.login {
        line.push_str(&format!("\tlogin={}", login.as_str()));
    }
    line
}

/// The text rendering.
#[must_use]
pub fn text(status: &Status) -> String {
    let mut lines = Vec::new();
    for harness in &status.harnesses {
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
            lines.push(format!("  error [{}]: {}", error.code(), error.hint()));
        }
        lines.extend(
            harness
                .notices
                .iter()
                .map(|notice| format!("  notice: {notice}")),
        );
        if harness.entries.is_empty() {
            lines.push("  (no selected or managed servers)".to_owned());
        }
        lines.extend(harness.entries.iter().map(entry_line));
    }
    if !status.missing_variables.is_empty() {
        lines.push(format!(
            "Missing variables: {}",
            status.missing_variables.join(", ")
        ));
    }
    lines.extend(
        status
            .notices
            .iter()
            .map(|notice| format!("note: {notice}")),
    );
    lines.join("\n")
}
