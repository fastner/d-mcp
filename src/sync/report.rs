//! The sync report: its model, the entry notices and the text and JSON
//! rendering.
//!
//! Output rules until WP4: reports name only harness, scope, file path, entry
//! name, operation, outcome, gaps, hold reasons and positions, plus hashes of
//! owned entries. They never contain field values, hashes of foreign
//! entries, child process output or `toml_edit` error text.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::{HeldReason, Operation};
use crate::adapter::{Gap, GapSeverity, Scope};
use crate::config::Harness;
use crate::error::{EXIT_ENVIRONMENT, EXIT_FAILURE, EXIT_UNSAFE, Error};

/// The outcome of one entry in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Applied,
    Planned,
    Failed,
    Recorded,
    Unchanged,
    Blocked,
    Kept,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::Planned => "planned",
            Outcome::Failed => "failed",
            Outcome::Recorded => "recorded",
            Outcome::Unchanged => "unchanged",
            Outcome::Blocked => "blocked",
            Outcome::Kept => "kept",
        }
    }
}

/// The status of one harness location in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HarnessStatus {
    /// Planned and, unless a dry run, applied.
    Synced,
    /// Neither the harness file nor its binary exists.
    NotPresent,
    /// The harness failed with an error.
    Failed,
    /// The recorded file differs; nothing was planned.
    LocationChanged,
    /// The entry root cannot be edited; nothing was planned.
    Unsupported,
}

impl HarnessStatus {
    fn as_str(self) -> &'static str {
        match self {
            HarnessStatus::Synced => "synced",
            HarnessStatus::NotPresent => "not-present",
            HarnessStatus::Failed => "failed",
            HarnessStatus::LocationChanged => "location-changed",
            HarnessStatus::Unsupported => "unsupported",
        }
    }
}

pub(super) struct EntryReport {
    pub(super) name: String,
    pub(super) operation: Operation,
    pub(super) outcome: Outcome,
    pub(super) gaps: Vec<Gap>,
    pub(super) reason: Option<HeldReason>,
    pub(super) notice: Option<String>,
    pub(super) hash: Option<String>,
}

pub(super) struct HarnessReport {
    pub(super) harness: Harness,
    pub(super) scope: Scope,
    pub(super) file: Option<PathBuf>,
    pub(super) status: HarnessStatus,
    pub(super) created: bool,
    pub(super) error: Option<Error>,
    pub(super) notices: Vec<String>,
    pub(super) entries: Vec<EntryReport>,
}

impl HarnessReport {
    pub(super) fn new(harness: Harness, scope: Scope) -> Self {
        Self {
            harness,
            scope,
            file: None,
            status: HarnessStatus::Synced,
            created: false,
            error: None,
            notices: Vec::new(),
            entries: Vec::new(),
        }
    }

    pub(super) fn failed(mut self, error: Error) -> Self {
        self.status = HarnessStatus::Failed;
        self.error = Some(error);
        self
    }

    pub(super) fn blocked(&self) -> usize {
        let root = usize::from(self.status == HarnessStatus::Unsupported);
        root + self
            .entries
            .iter()
            .filter(|entry| entry.operation.blocks())
            .count()
    }

    pub(super) fn exit_code(&self) -> u8 {
        match &self.error {
            Some(error) => error.exit_code(),
            None if self.blocked() > 0 => EXIT_FAILURE,
            None => 0,
        }
    }

    fn json(&self) -> Value {
        json!({
            "harness": self.harness.as_str(),
            "scope": self.scope.as_str(),
            "file": self.file.as_ref().map(|file| file.to_string_lossy().into_owned()),
            "status": self.status.as_str(),
            "created": self.created,
            "error": self.error.as_ref().map(|error| json!({ "code": error.code(), "hint": error.hint() })),
            "notices": self.notices,
            "entries": self.entries.iter().map(|entry| json!({
                "name": entry.name,
                "operation": entry.operation.as_str(),
                "outcome": entry.outcome.as_str(),
                "reason": entry.reason.map(HeldReason::as_str),
                "notice": entry.notice,
                "hash": entry.hash,
                "gaps": entry.gaps,
            })).collect::<Vec<_>>(),
        })
    }

    fn text(&self, out: &mut String) {
        let file = self.file.as_ref().map_or_else(
            || "-".to_owned(),
            |file| file.to_string_lossy().into_owned(),
        );
        out.push_str(&format!(
            "{} ({}): {file}: {}",
            self.harness.as_str(),
            self.scope.as_str(),
            self.status.as_str()
        ));
        if self.created {
            out.push_str(" (file created)");
        }
        if let Some(error) = &self.error {
            out.push_str(&format!("\n  error: {} [{}]", error.hint(), error.code()));
        }
        for notice in &self.notices {
            out.push_str(&format!("\n  notice: {notice}"));
        }
        for entry in &self.entries {
            out.push_str(&format!(
                "\n  {}\t{}\t{}",
                entry.operation.as_str(),
                entry.name,
                entry.outcome.as_str()
            ));
            if let Some(reason) = entry.reason {
                out.push_str(&format!(" ({})", reason.as_str()));
            }
            if let Some(notice) = &entry.notice {
                out.push_str(&format!("\n    {notice}"));
            }
            for gap in &entry.gaps {
                let severity = match gap.severity {
                    GapSeverity::Note => "note",
                    GapSeverity::Important => "important",
                };
                out.push_str(&format!(
                    "\n    {severity} gap `{}`: {}",
                    gap.field, gap.reason
                ));
            }
        }
    }
}

/// The entry notice for an operation. `desired` tells whether the server is
/// still selected and resolved for the harness.
pub(super) fn notice(
    operation: Operation,
    name: &str,
    harness: Harness,
    taken_over: bool,
    desired: bool,
) -> Option<String> {
    let harness = harness.as_str();
    let text = match operation {
        // A take-over of an entry that is no longer desired would remove
        // it, so it is not advised.
        Operation::Drift if !desired => format!(
            "changed outside d-mcp and no longer selected; run `d-mcp unmanage {name} --harness {harness}` to keep the entry as it is, or select it again"
        ),
        Operation::Drift => format!(
            "changed outside d-mcp; run `d-mcp sync --take-over {name}` to accept the change"
        ),
        Operation::Collision => format!(
            "an entry with this name exists and is not managed by d-mcp; run `d-mcp sync --take-over {name}` to manage it"
        ),
        Operation::IdenticalCollision => format!(
            "an identical entry exists and is not managed by d-mcp; run `d-mcp sync --take-over {name}` to manage it without a write"
        ),
        Operation::Vanished => format!(
            "removed outside d-mcp and re-added; run `d-mcp unmanage {name} --harness {harness}` to keep it removed"
        ),
        Operation::Held => "held; the managed entry is left unchanged".to_owned(),
        Operation::Orphaned => {
            "the managed entry is gone and no longer selected; the record is dropped".to_owned()
        }
        Operation::LocationChanged => {
            "recorded for another file; run `d-mcp sync --relocate` to manage it at the new location"
                .to_owned()
        }
        Operation::Unsupported => format!(
            "written in an inline or dotted style d-mcp does not edit; restore the standard table or run `d-mcp unmanage {name} --harness {harness}`"
        ),
        Operation::Blocked => "an important gap blocks this entry; it is not written".to_owned(),
        Operation::Add | Operation::Update | Operation::Remove | Operation::Unchanged => {
            return taken_over.then(|| "taken over".to_owned());
        }
    };
    Some(if taken_over {
        format!("taken over; {text}")
    } else {
        text
    })
}

/// The aggregate error of a sync: the highest-priority harness code, in the
/// order 3, 4, 1, 0.
pub(super) fn aggregate_error(reports: &[HarnessReport]) -> Option<Error> {
    let rank = |code: u8| match code {
        EXIT_UNSAFE => 4,
        EXIT_ENVIRONMENT => 3,
        EXIT_FAILURE => 2,
        _ => 1,
    };
    let worst = reports
        .iter()
        .filter(|report| report.exit_code() != 0)
        .max_by_key(|report| rank(report.exit_code()));
    worst.map(|report| match &report.error {
        Some(error) => error.clone(),
        None => Error::SyncBlocked {
            count: reports.iter().map(HarnessReport::blocked).sum(),
        },
    })
}

/// Renders the harness reports as text and as the `sync` JSON document.
pub(super) fn render(reports: &[HarnessReport], store: &str, dry_run: bool) -> (Value, String) {
    let mut text = String::new();
    for (index, report) in reports.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        report.text(&mut text);
    }
    if reports.is_empty() {
        text.push_str("No harness to sync.");
    }
    if dry_run {
        text.push_str("\nDry run: nothing was written.");
    }
    let data = json!({
        "store": store,
        "dry_run": dry_run,
        "harnesses": reports.iter().map(HarnessReport::json).collect::<Vec<_>>(),
    });
    (data, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_of_an_unselected_entry_never_advises_a_take_over() {
        let selected = notice(Operation::Drift, "a", Harness::Codex, false, true).unwrap();
        assert!(selected.contains("--take-over a"), "{selected}");
        let unselected = notice(Operation::Drift, "a", Harness::Codex, false, false).unwrap();
        assert!(!unselected.contains("take-over"), "{unselected}");
        assert!(unselected.contains("d-mcp unmanage a --harness codex"));
        assert_eq!(
            notice(Operation::Update, "a", Harness::Codex, true, true).as_deref(),
            Some("taken over")
        );
        assert_eq!(
            notice(Operation::Add, "a", Harness::Codex, false, true),
            None
        );
    }

    #[test]
    fn statuses_and_the_aggregate_error() {
        assert_eq!(HarnessStatus::NotPresent.as_str(), "not-present");
        assert_eq!(HarnessStatus::LocationChanged.as_str(), "location-changed");
        let mut unsupported = HarnessReport::new(Harness::Codex, Scope::User);
        unsupported.status = HarnessStatus::Unsupported;
        assert_eq!(unsupported.blocked(), 1);
        assert_eq!(unsupported.exit_code(), EXIT_FAILURE);
        let failed = HarnessReport::new(Harness::Codex, Scope::User).failed(Error::HomeUnset);
        assert_eq!(failed.status, HarnessStatus::Failed);
        let error = aggregate_error(&[unsupported, failed]).unwrap();
        assert_eq!(error.code(), "home-unset");
        assert!(aggregate_error(&[HarnessReport::new(Harness::Codex, Scope::User)]).is_none());
    }
}
