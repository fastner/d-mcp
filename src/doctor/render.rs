//! The JSON document and the text rendering of `doctor`.

use serde_json::{Value, json};

use super::{Check, Doctor, Severity};

fn check_json(check: &Check) -> Value {
    json!({
        "check": check.check.as_str(),
        "severity": check.severity.as_str(),
        "harness": check.harness.map(|harness| harness.as_str()),
        "server": check.server,
        "code": check.code,
        "message": check.message,
        "hint": check.hint,
    })
}

/// The `data` document.
#[must_use]
pub fn json(doctor: &Doctor) -> Value {
    json!({
        "store": doctor.store.to_string_lossy(),
        "repository": doctor.repository.as_ref().map(|path| path.to_string_lossy()),
        "auth": doctor.auth,
        "summary": {
            "ok": doctor.count(Severity::Ok),
            "warn": doctor.count(Severity::Warn),
            "error": doctor.count(Severity::Error),
        },
        "checks": doctor.checks.iter().map(check_json).collect::<Vec<_>>(),
    })
}

/// The text rendering: one line per check (severity, check, harness,
/// message, error code), its hint indented below, then a summary.
#[must_use]
pub fn text(doctor: &Doctor) -> String {
    let mut lines = Vec::new();
    for check in &doctor.checks {
        let mut line = format!("{}\t{}", check.severity.as_str(), check.check.as_str());
        if let Some(harness) = check.harness {
            line.push_str(&format!("\t{}", harness.as_str()));
        }
        line.push_str(&format!("\t{}", check.message));
        if let Some(code) = check.code {
            line.push_str(&format!(" [{code}]"));
        }
        lines.push(line);
        if let Some(hint) = &check.hint {
            lines.push(format!("  hint: {hint}"));
        }
    }
    lines.push(format!(
        "{} ok, {} warn, {} error",
        doctor.count(Severity::Ok),
        doctor.count(Severity::Warn),
        doctor.count(Severity::Error)
    ));
    lines.join("\n")
}
