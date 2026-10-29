//! The `adopt` document and its text form. Entry content appears only as
//! redacted views; hashes only as display hashes.

use serde_json::{Value, json};

use super::origin::OriginPlan;
use super::{Adoption, Refusal, ServerPlan};
use crate::adapter::GapSeverity;
use crate::redact::safe_key;

fn path(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn origin_json(origin: &OriginPlan) -> Value {
    json!({
        "entry": origin.label(),
        "harness": origin.entry.place.harness.as_str(),
        "scope": origin.entry.place.scope.as_str(),
        "path": origin.path.as_deref().map(path),
        "file": origin.file.as_deref().map(path),
        "display_hash": origin.display_hash,
        "write": origin.write.map(super::origin::WriteMode::as_str),
        "before": origin.before,
        "after": origin.after,
        "changed": origin.changed,
        "gaps": origin.gaps,
        "replaced_fields": origin.replaced_fields.iter().map(|replaced| json!({
            "field": replaced.field,
            "var": safe_key(&replaced.var),
        })).collect::<Vec<_>>(),
        "outcome": origin.outcome,
        "error": origin.error,
    })
}

fn server_json(server: &ServerPlan) -> Value {
    json!({
        "name": server.name,
        "harnesses": server.harnesses.iter().map(|harness| harness.as_str()).collect::<Vec<_>>(),
        "verdict": server.verdict,
        "from": server.from.map(|place| place.to_string()),
        "file": path(&server.file),
        "definition": server.view,
        "entries": server.origins.iter().map(origin_json).collect::<Vec<_>>(),
        "replaced": server.replaced,
        "kept_literals": server.kept,
        "normalizations": server.normalizations,
        "variables": server.variables,
        "refusals": server.refusals,
        "notices": server.notices,
    })
}

/// The `adopt` document.
pub(super) fn json(adoption: &Adoption) -> Value {
    json!({
        "store": path(&adoption.store),
        "repository": path(&adoption.repository),
        "apply": adoption.apply,
        "display_hashes": adoption.display_hashes,
        "servers": adoption.servers.iter().map(server_json).collect::<Vec<_>>(),
        "refusals": adoption.refusals,
        "notices": adoption.notices,
        "written": adoption.written.as_ref().map(|written| json!({
            "definitions": written.definitions.iter().map(|file| path(file)).collect::<Vec<_>>(),
            "selections": written.selections,
        })),
        "sync": adoption.sync,
    })
}

fn refusal_line(refusal: &Refusal) -> String {
    let mut line = String::from("  refused");
    if let Some(entry) = &refusal.entry {
        line.push_str(&format!(" {entry}"));
    }
    if !refusal.field.is_empty() {
        line.push_str(&format!(" `{}`", refusal.field));
    }
    line.push_str(&format!(": {}", refusal.reason));
    line
}

fn origin_text(origin: &OriginPlan, out: &mut Vec<String>) {
    let mut line = format!("  {}", origin.label());
    if let Some(write) = origin.write {
        line.push_str(&format!("\twrite={}", write.as_str()));
    }
    if let Some(hash) = &origin.display_hash {
        line.push_str(&format!("\tdisplay_hash={hash}"));
    }
    if let Some(outcome) = &origin.outcome {
        line.push_str(&format!("\toutcome={outcome}"));
    }
    out.push(line);
    if let Some(error) = &origin.error {
        out.push(format!(
            "    error: {} [{}]",
            error["hint"].as_str().unwrap_or_default(),
            error["code"].as_str().unwrap_or_default()
        ));
    }
    if let Some(before) = &origin.before {
        out.push(format!("    before: {}", before.text()));
    }
    if let Some(after) = &origin.after {
        out.push(format!("    after:  {}", after.text()));
    }
    if !origin.changed.is_empty() {
        out.push(format!("    changed: {}", origin.changed.join(", ")));
    }
    for gap in &origin.gaps {
        let severity = match gap.severity {
            GapSeverity::Note => "note",
            GapSeverity::Important => "important",
        };
        out.push(format!(
            "    {severity} gap `{}`: {}",
            gap.field, gap.reason
        ));
    }
}

fn server_text(server: &ServerPlan, out: &mut Vec<String>) {
    let harnesses: Vec<&str> = server.harnesses.iter().map(|h| h.as_str()).collect();
    let mut head = format!("{}\t{}", server.name, path(&server.file));
    if let Some(verdict) = server.verdict {
        let verdict = serde_json::to_value(verdict).unwrap_or_default();
        head.push_str(&format!("\t{}", verdict.as_str().unwrap_or_default()));
    }
    if let Some(from) = server.from {
        head.push_str(&format!("\tfrom={from}"));
    }
    head.push_str(&format!("\tselection={}", harnesses.join(",")));
    out.push(head);
    if let Some(view) = &server.view {
        out.push(format!("  definition: {}", view.text()));
    }
    for origin in &server.origins {
        origin_text(origin, out);
    }
    for replaced in &server.replaced {
        out.push(format!(
            "  replaced {} `{}` ({}) -> ${}",
            replaced.origin, replaced.field, replaced.kind, replaced.var
        ));
    }
    for kept in &server.kept {
        out.push(format!("  kept literal `env.{kept}`"));
    }
    for normalization in &server.normalizations {
        out.push(format!(
            "  normalized {} `{}`: {}",
            normalization.origin,
            normalization.normalization.field,
            normalization.normalization.kind.as_str()
        ));
    }
    for check in &server.variables {
        let status = serde_json::to_value(check.status).unwrap_or_default();
        out.push(format!(
            "  variable {} for `{}`: {}{}",
            check.name,
            check.field,
            status.as_str().unwrap_or_default(),
            if check.accepted { " (accepted)" } else { "" }
        ));
    }
    out.extend(server.refusals.iter().map(refusal_line));
    for notice in &server.notices {
        out.push(format!("  notice: {notice}"));
    }
}

/// The text form.
pub(super) fn text(adoption: &Adoption) -> String {
    let mut out = Vec::new();
    for server in &adoption.servers {
        server_text(server, &mut out);
    }
    out.extend(adoption.refusals.iter().map(refusal_line));
    for notice in &adoption.notices {
        out.push(format!("notice: {notice}"));
    }
    if let Some(written) = &adoption.written {
        for file in &written.definitions {
            out.push(format!("wrote {}", path(file)));
        }
        for name in &written.selections {
            out.push(format!("selected `{name}`"));
        }
    }
    if !adoption.apply {
        let refused = adoption.refusal_count() > 0;
        let expects: Vec<String> = adoption
            .servers
            .iter()
            .flat_map(|server| &server.origins)
            .filter_map(|origin| {
                let hash = origin.display_hash.as_ref()?;
                Some(format!("--expect {}={hash}", origin.label()))
            })
            .collect();
        out.push(if refused {
            "Preview: nothing was written; the refusals above block --apply.".to_owned()
        } else if expects.is_empty() {
            "Preview: nothing was written.".to_owned()
        } else {
            format!(
                "Preview: nothing was written; to apply, run again with --apply {}",
                expects.join(" ")
            )
        });
    }
    out.join("\n")
}
