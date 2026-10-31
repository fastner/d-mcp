//! Pure, deterministic resolution of selections against source snapshots.
//!
//! [`resolve`] does no I/O. It sorts its inputs itself, so the result does not
//! depend on the order of sources, files or selections. For every selected
//! server it returns exactly one status:
//!
//! - `resolved`: the winning definition from the highest-priority source that
//!   offers the name;
//! - `hold` with reason `source-unavailable`: an unavailable source ranks
//!   above the current winner, or no winner exists while a source is
//!   unavailable;
//! - `hold` with reason `invalid-winner`: the highest-priority file for the
//!   name is invalid (fail closed);
//! - `hold` with reason `pending-approval`: the winner comes from a Git
//!   source and its `definition_hash` is not approved for that source;
//! - `hold` with reason `risk-not-accepted`: the winner is approved, but has
//!   audit findings without an acceptance of the current findings hash;
//! - `revoked`: the winner's (source, name) carries a revoked marker, also
//!   when the winner is invalid; or a revoked marker sits on an unavailable
//!   source that ranks above the winner; or no source offers the name and a
//!   source carries a revoked marker for it, also a source id that is no
//!   longer a registered Git source (a *detached* marker, see
//!   [`resolve_with_detached`]). The revoked definition may be
//!   the one sync installed, so it is removed even when it is now invalid,
//!   gone upstream or unreadable;
//! - `not-offered`: no source offers the name.
//!
//! WP2 turns `hold` into its *Held* operation (keep the existing managed entry
//! unchanged); `revoked` is left out of the desired map, so sync removes an
//! owned entry. The approval and audit verdicts are inputs: the gate in
//! [`crate::source::load_resolution`] sets [`FileEntry::gate`] and
//! [`FileEntry::findings`] before resolution. A held or revoked winner
//! shadows lower definitions like any winner; nothing falls through.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::approval::{ApprovalKey, Orphan, OrphanKind};
use crate::audit::Finding;
use crate::config::{HarnessSet, Selection};
use crate::definition::{Definition, DefinitionError, OverrideSites};
use crate::rule::Rule;

/// A scanned source, the resolver input.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSnapshot {
    /// The source id.
    pub id: String,
    /// The priority; higher wins. The built-in `local` source has 0.
    pub priority: u32,
    /// Whether the source root and its `servers/` directory are readable.
    pub available: bool,
    /// The `servers/*.toml` files.
    pub files: Vec<FileEntry>,
    /// Ignored entries in `servers/`.
    pub notices: Vec<Notice>,
    /// The server names that carry a revoked marker for this source (set by
    /// the gate, Git sources only), whether or not the source offers them.
    pub revoked: BTreeSet<String>,
}

/// One `servers/*.toml` file of a source.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// The path relative to the source root, such as `servers/github.toml`.
    pub file: String,
    /// The conflict key: the file stem, when it passes the name rule. A file
    /// without a key shadows nothing.
    pub key: Option<String>,
    /// The parsed definition, or the per-file problem.
    pub outcome: Result<Definition, FileProblem>,
    /// The TOML kind and position of every top-level override key of a
    /// parsed definition (empty for a problem), for the `validate` step.
    pub override_sites: OverrideSites,
    /// The launcher audit findings of a valid definition (set by the gate).
    pub findings: Vec<Finding>,
    /// The approval verdict of a valid definition from a Git source (set by
    /// the gate); `None` for a directory source, whose findings only warn.
    pub gate: Option<GateVerdict>,
}

/// The approval and audit verdict on a Git-source definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateVerdict {
    /// Approved, and without findings or with an accepted risk that matches
    /// the current findings hash.
    Approved,
    /// The `definition_hash` is not approved for the source.
    PendingApproval,
    /// Approved, but the findings are not accepted (or changed since the
    /// acceptance).
    RiskNotAccepted,
    /// The (source, name) carries a revoked marker.
    Revoked,
}

/// Why a file is invalid. It names the rule and position only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileProblem {
    /// The broken rule.
    pub rule: Rule,
    /// The field path, when the problem is inside the file.
    pub field: Option<String>,
    /// 1-based line, when known.
    pub line: Option<usize>,
    /// 1-based column, when known.
    pub column: Option<usize>,
}

impl FileProblem {
    /// A file-level problem without a position.
    #[must_use]
    pub fn file(rule: Rule) -> Self {
        Self {
            rule,
            field: None,
            line: None,
            column: None,
        }
    }
}

impl From<DefinitionError> for FileProblem {
    fn from(error: DefinitionError) -> Self {
        Self {
            rule: error.rule,
            field: (!error.field.is_empty()).then_some(error.field),
            line: Some(error.line),
            column: Some(error.column),
        }
    }
}

/// An entry of `servers/` that the scan ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The path relative to the source root.
    pub file: String,
    /// `hidden-file`, `non-toml-file` or `subdirectory`.
    pub rule: Rule,
}

/// The status of a selected server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// A valid winner exists.
    Resolved,
    /// The server is held; see [`HoldReason`].
    Hold,
    /// No source offers the name.
    NotOffered,
    /// The winner's approval was revoked; sync removes an owned entry.
    Revoked,
}

/// Why a server is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldReason {
    /// The highest-priority definition is invalid.
    InvalidWinner,
    /// An unavailable source could change the result.
    SourceUnavailable,
    /// The winner from a Git source is not approved.
    PendingApproval,
    /// The winner is approved, but its audit findings are not accepted.
    RiskNotAccepted,
}

/// The resolution of one selected server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerResolution {
    /// The server name.
    pub name: String,
    /// The status.
    pub status: Status,
    /// The hold reason, for `hold`.
    pub reason: Option<HoldReason>,
    /// For `resolved`, the origin source; for `hold`, the source that causes
    /// the hold.
    pub source: Option<String>,
    /// The file in `source`, when there is one.
    pub file: Option<String>,
    /// The selected harnesses.
    pub harnesses: HarnessSet,
    /// The winning definition, for `resolved`.
    pub definition: Option<Definition>,
    /// For a valid winner from a Git source: its gate verdict, hash and
    /// findings, also when it is held or revoked.
    pub gated: Option<Gated>,
}

/// The gate data of a Git-source winner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gated {
    /// The verdict.
    pub verdict: GateVerdict,
    /// The winner's `definition_hash`.
    pub definition_hash: String,
    /// The winner's audit findings.
    pub findings: Vec<Finding>,
}

/// The closed list of problem codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProblemCode {
    /// A file in an available source is invalid.
    InvalidDefinition,
    /// A selected server is held because its winner is invalid.
    InvalidWinner,
    /// A selected server is not offered by any source.
    NotOffered,
    /// A source is unavailable, or a selected server is held because of one.
    SourceUnavailable,
    /// A definition is shadowed by a higher-priority one.
    Shadowed,
    /// An entry of `servers/` was ignored.
    Notice,
    /// A selected server is held because its Git-source winner is not
    /// approved.
    PendingApproval,
    /// A selected server is held because its audit findings are not
    /// accepted.
    RiskNotAccepted,
    /// A selected server's winner has a launcher audit finding (a warning
    /// for a directory source).
    AuditFinding,
    /// An approval, accepted risk or revoked marker in `approvals.toml`
    /// belongs to a source that is no longer a registered Git source with
    /// the recorded URL.
    OrphanedApproval,
}

/// The message of an orphaned entry of `approvals.toml`, built from names
/// only.
#[must_use]
pub fn orphan_message(orphan: &Orphan) -> String {
    let ApprovalKey { source, server } = &orphan.key;
    match orphan.kind {
        OrphanKind::Approval => format!(
            "the approval of `{server}` from source `{source}` is orphaned: the source is no longer a registered Git source with the approved URL"
        ),
        OrphanKind::Revoked => format!(
            "the revoked marker of `{server}` from source `{source}` is orphaned: the source is no longer a registered Git source with the revoked URL; it still blocks `{server}` from source `{source}`"
        ),
    }
}

/// A problem reported next to the resolution. Problems never carry values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// The problem code.
    pub code: ProblemCode,
    /// The server name, when the problem concerns one.
    pub server: Option<String>,
    /// The source id, when the problem concerns one.
    pub source: Option<String>,
    /// The file relative to the source root.
    pub file: Option<String>,
    /// The rule code, for `invalid-definition`, `invalid-winner` and `notice`.
    pub rule: Option<Rule>,
    /// The field path inside the file.
    pub field: Option<String>,
    /// 1-based line.
    pub line: Option<usize>,
    /// 1-based column.
    pub column: Option<usize>,
    /// A human-readable summary built from codes and names only.
    pub message: String,
}

impl Problem {
    fn new(code: ProblemCode, message: String) -> Self {
        Self {
            code,
            server: None,
            source: None,
            file: None,
            rule: None,
            field: None,
            line: None,
            column: None,
            message,
        }
    }

    fn with_file_problem(mut self, problem: &FileProblem) -> Self {
        self.rule = Some(problem.rule);
        self.field.clone_from(&problem.field);
        self.line = problem.line;
        self.column = problem.column;
        self
    }

    /// The `orphaned-approval` problem of an orphaned entry of
    /// `approvals.toml`.
    #[must_use]
    pub fn orphaned_approval(orphan: &Orphan) -> Self {
        let ApprovalKey { source, server } = &orphan.key;
        Self {
            server: Some(server.clone()),
            source: Some(source.clone()),
            ..Self::new(ProblemCode::OrphanedApproval, orphan_message(orphan))
        }
    }

    fn sort_key(&self) -> impl Ord + '_ {
        (
            self.code,
            self.server.as_deref(),
            self.source.as_deref(),
            self.file.as_deref(),
            self.line,
            self.column,
        )
    }
}

/// The resolver result.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// One entry per selected server, sorted by name.
    pub servers: Vec<ServerResolution>,
    /// Problems, sorted by code, server, source and file.
    pub problems: Vec<Problem>,
}

impl Resolution {
    /// Adds problems found outside the resolver (such as orphaned
    /// approvals) and keeps the problems sorted.
    pub fn add_problems(&mut self, problems: impl IntoIterator<Item = Problem>) {
        self.problems.extend(problems);
        self.problems
            .sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    }
}

/// A candidate definition for a key.
struct Candidate<'a> {
    source: &'a SourceSnapshot,
    file: &'a FileEntry,
}

/// Resolves the selections against the source snapshots.
#[must_use]
pub fn resolve(sources: &[SourceSnapshot], selections: &[Selection]) -> Resolution {
    resolve_with_detached(sources, selections, &BTreeSet::new())
}

/// Resolves like [`resolve`], with the *detached* revoked markers: those
/// whose source id is not a registered Git source (removed, or now a
/// directory source), so no snapshot carries them. A detached marker makes
/// its server `revoked` only when no source offers the name, so a server
/// revoked before its source was removed is still removed by sync instead
/// of being kept as `not-offered`.
#[must_use]
pub fn resolve_with_detached(
    sources: &[SourceSnapshot],
    selections: &[Selection],
    detached: &BTreeSet<ApprovalKey>,
) -> Resolution {
    let mut ordered: Vec<&SourceSnapshot> = sources.iter().collect();
    ordered.sort_by(|a, b| {
        (Reverse(a.priority), a.id.as_str()).cmp(&(Reverse(b.priority), b.id.as_str()))
    });

    let mut problems = Vec::new();
    let mut candidates: BTreeMap<&str, Vec<Candidate<'_>>> = BTreeMap::new();
    for source in &ordered {
        if !source.available {
            let mut problem = Problem::new(
                ProblemCode::SourceUnavailable,
                format!(
                    "source `{}` is unavailable: its root or servers/ directory is missing or unreadable",
                    source.id
                ),
            );
            problem.source = Some(source.id.clone());
            problems.push(problem);
            continue;
        }
        let mut files: Vec<&FileEntry> = source.files.iter().collect();
        files.sort_by(|a, b| a.file.cmp(&b.file));
        for file in files {
            if let Err(file_problem) = &file.outcome {
                let mut problem = Problem::new(
                    ProblemCode::InvalidDefinition,
                    format!(
                        "{} in source `{}` breaks rule `{}`",
                        file.file, source.id, file_problem.rule
                    ),
                )
                .with_file_problem(file_problem);
                problem.server.clone_from(&file.key);
                problem.source = Some(source.id.clone());
                problem.file = Some(file.file.clone());
                problems.push(problem);
            }
            if let Some(key) = &file.key {
                candidates
                    .entry(key.as_str())
                    .or_default()
                    .push(Candidate { source, file });
            }
        }
        let mut notices: Vec<&Notice> = source.notices.iter().collect();
        notices.sort_by(|a, b| a.file.cmp(&b.file));
        for notice in notices {
            let mut problem = Problem::new(
                ProblemCode::Notice,
                format!(
                    "{} in source `{}` was ignored ({})",
                    notice.file, source.id, notice.rule
                ),
            );
            problem.source = Some(source.id.clone());
            problem.file = Some(notice.file.clone());
            problem.rule = Some(notice.rule);
            problems.push(problem);
        }
    }

    for (key, list) in &candidates {
        let winner = &list[0];
        for shadowed in &list[1..] {
            let mut problem = Problem::new(
                ProblemCode::Shadowed,
                format!(
                    "{} in source `{}` is shadowed by {} in source `{}`",
                    shadowed.file.file, shadowed.source.id, winner.file.file, winner.source.id
                ),
            );
            problem.server = Some((*key).to_owned());
            problem.source = Some(shadowed.source.id.clone());
            problem.file = Some(shadowed.file.file.clone());
            problems.push(problem);
        }
    }

    // `config.toml` keeps selections unique, so duplicates only reach here
    // from other callers. Sorting by server and then by harness names makes
    // the kept duplicate independent of the input order.
    let mut selected: Vec<&Selection> = selections.iter().collect();
    selected.sort_by(|a, b| {
        (a.server.as_str(), a.harnesses.names()).cmp(&(b.server.as_str(), b.harnesses.names()))
    });
    selected.dedup_by(|a, b| a.server == b.server);
    let servers = selected
        .into_iter()
        .map(|selection| {
            let winner = candidates
                .get(selection.server.as_str())
                .and_then(|list| list.first());
            resolve_one(selection, winner, &ordered, detached, &mut problems)
        })
        .collect();

    problems.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    Resolution { servers, problems }
}

fn resolve_one(
    selection: &Selection,
    winner: Option<&Candidate<'_>>,
    ordered: &[&SourceSnapshot],
    detached: &BTreeSet<ApprovalKey>,
    problems: &mut Vec<Problem>,
) -> ServerResolution {
    let name = selection.server.clone();
    let mut resolution = ServerResolution {
        name: name.clone(),
        status: Status::Resolved,
        reason: None,
        source: None,
        file: None,
        harnesses: selection.harnesses.clone(),
        definition: None,
        gated: None,
    };
    let winner_priority = winner.map(|candidate| candidate.source.priority);
    // A revoked marker wins over every hold: on the winner's source (valid
    // or not), on an unavailable source above the winner, or on any source
    // when nothing offers the name; a detached marker (its source id is no
    // longer a registered Git source) only in the last case.
    let revoked_by = ordered
        .iter()
        .find(|source| {
            source.revoked.contains(&name)
                && winner.is_none_or(|winner| {
                    source.id == winner.source.id
                        || (!source.available && source.priority > winner.source.priority)
                })
        })
        .map(|source| source.id.as_str())
        .or_else(|| {
            detached
                .iter()
                .find(|key| winner.is_none() && key.server == name)
                .map(|key| key.source.as_str())
        });
    if let Some(revoked_by) = revoked_by {
        resolution.status = Status::Revoked;
        resolution.source = Some(revoked_by.to_owned());
        if let Some(winner) = winner.filter(|winner| winner.source.id == revoked_by) {
            resolution.file = Some(winner.file.file.clone());
            if let Ok(definition) = &winner.file.outcome {
                report_findings(&name, winner, problems);
                resolution.gated = winner.file.gate.map(|_| Gated {
                    verdict: GateVerdict::Revoked,
                    definition_hash: definition.definition_hash(),
                    findings: winner.file.findings.clone(),
                });
            }
        }
        return resolution;
    }
    // `ordered` is sorted by descending priority, so the first unavailable
    // source above the winner is the highest-ranked blocker.
    let blocker = ordered.iter().find(|source| {
        !source.available && winner_priority.is_none_or(|priority| source.priority > priority)
    });
    if let Some(blocker) = blocker {
        let mut problem = Problem::new(
            ProblemCode::SourceUnavailable,
            format!(
                "`{name}` is held: unavailable source `{}` could change its resolution",
                blocker.id
            ),
        );
        problem.server = Some(name);
        problem.source = Some(blocker.id.clone());
        problems.push(problem);
        resolution.status = Status::Hold;
        resolution.reason = Some(HoldReason::SourceUnavailable);
        resolution.source = Some(blocker.id.clone());
        return resolution;
    }
    let Some(winner) = winner else {
        let mut problem = Problem::new(
            ProblemCode::NotOffered,
            format!("`{name}` is selected but no source offers it"),
        );
        problem.server = Some(name);
        problems.push(problem);
        resolution.status = Status::NotOffered;
        return resolution;
    };
    resolution.source = Some(winner.source.id.clone());
    resolution.file = Some(winner.file.file.clone());
    match &winner.file.outcome {
        Ok(definition) => {
            apply_verdict(&mut resolution, winner, definition, problems);
        }
        Err(file_problem) => {
            let problem = Problem {
                server: Some(name.clone()),
                source: Some(winner.source.id.clone()),
                file: Some(winner.file.file.clone()),
                ..Problem::new(
                    ProblemCode::InvalidWinner,
                    format!(
                        "`{name}` is held: its highest-priority definition {} in source `{}` is invalid",
                        winner.file.file, winner.source.id
                    ),
                )
            }
            .with_file_problem(file_problem);
            problems.push(problem);
            resolution.status = Status::Hold;
            resolution.reason = Some(HoldReason::InvalidWinner);
        }
    }
    resolution
}

/// Reports the audit findings of a valid winner as `audit-finding`
/// problems.
fn report_findings(name: &str, winner: &Candidate<'_>, problems: &mut Vec<Problem>) {
    let source = &winner.source.id;
    for finding in &winner.file.findings {
        let effect = if winner.file.gate.is_some() {
            "needs risk acceptance"
        } else {
            "warning only, directory source"
        };
        problems.push(Problem {
            server: Some(name.to_owned()),
            source: Some(source.clone()),
            file: Some(winner.file.file.clone()),
            ..Problem::new(
                ProblemCode::AuditFinding,
                format!(
                    "`{name}` in source `{source}`: launcher `{}` breaks audit rule `{}` ({effect})",
                    finding.launcher,
                    finding.rule.as_str()
                ),
            )
        });
    }
}

/// Resolves a valid winner through its gate verdict and reports its audit
/// findings.
fn apply_verdict(
    resolution: &mut ServerResolution,
    winner: &Candidate<'_>,
    definition: &Definition,
    problems: &mut Vec<Problem>,
) {
    let name = resolution.name.clone();
    let source = winner.source.id.clone();
    let file = winner.file.file.clone();
    let located = |code: ProblemCode, message: String| Problem {
        server: Some(name.clone()),
        source: Some(source.clone()),
        file: Some(file.clone()),
        ..Problem::new(code, message)
    };
    report_findings(&name, winner, problems);
    let Some(verdict) = winner.file.gate else {
        resolution.definition = Some(definition.clone());
        return;
    };
    resolution.gated = Some(Gated {
        verdict,
        definition_hash: definition.definition_hash(),
        findings: winner.file.findings.clone(),
    });
    match verdict {
        GateVerdict::Approved => resolution.definition = Some(definition.clone()),
        GateVerdict::PendingApproval => {
            problems.push(located(
                ProblemCode::PendingApproval,
                format!(
                    "`{name}` is held: its definition {file} in Git source `{source}` is not approved"
                ),
            ));
            resolution.status = Status::Hold;
            resolution.reason = Some(HoldReason::PendingApproval);
        }
        GateVerdict::RiskNotAccepted => {
            problems.push(located(
                ProblemCode::RiskNotAccepted,
                format!(
                    "`{name}` is held: the audit findings of {file} in Git source `{source}` are not accepted"
                ),
            ));
            resolution.status = Status::Hold;
            resolution.reason = Some(HoldReason::RiskNotAccepted);
        }
        GateVerdict::Revoked => resolution.status = Status::Revoked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::parse_definition;

    fn definition(name: &str, command: &str) -> Definition {
        parse_definition(
            &format!("name = \"{name}\"\n[transport]\ntype = \"stdio\"\ncommand = \"{command}\"\n"),
            name,
        )
        .unwrap()
    }

    fn valid(name: &str, command: &str) -> FileEntry {
        FileEntry {
            file: format!("servers/{name}.toml"),
            key: Some(name.to_owned()),
            outcome: Ok(definition(name, command)),
            override_sites: OverrideSites::default(),
            findings: Vec::new(),
            gate: None,
        }
    }

    fn invalid(name: &str) -> FileEntry {
        FileEntry {
            file: format!("servers/{name}.toml"),
            key: Some(name.to_owned()),
            outcome: Err(FileProblem::file(Rule::TomlSyntax)),
            override_sites: OverrideSites::default(),
            findings: Vec::new(),
            gate: None,
        }
    }

    fn source(id: &str, priority: u32, files: Vec<FileEntry>) -> SourceSnapshot {
        SourceSnapshot {
            id: id.to_owned(),
            priority,
            available: true,
            files,
            notices: Vec::new(),
            revoked: BTreeSet::new(),
        }
    }

    fn unavailable(id: &str, priority: u32) -> SourceSnapshot {
        SourceSnapshot {
            available: false,
            ..source(id, priority, Vec::new())
        }
    }

    fn select(name: &str) -> Selection {
        Selection {
            server: name.to_owned(),
            harnesses: HarnessSet::All,
        }
    }

    fn codes(resolution: &Resolution) -> Vec<ProblemCode> {
        resolution.problems.iter().map(|p| p.code).collect()
    }

    #[test]
    fn resolved_with_shadowing() {
        let sources = [
            source("local", 0, vec![valid("github", "low")]),
            source("team", 10, vec![valid("github", "high")]),
        ];
        let resolution = resolve(&sources, &[select("github")]);
        let server = &resolution.servers[0];
        assert_eq!(server.status, Status::Resolved);
        assert_eq!(server.source.as_deref(), Some("team"));
        assert_eq!(server.file.as_deref(), Some("servers/github.toml"));
        assert_eq!(
            server.definition.as_ref().unwrap().transport,
            definition("github", "high").transport
        );
        assert_eq!(codes(&resolution), [ProblemCode::Shadowed]);
        assert_eq!(resolution.problems[0].source.as_deref(), Some("local"));
    }

    #[test]
    fn invalid_winner_is_held_even_with_a_valid_lower_copy() {
        let sources = [
            source("local", 0, vec![valid("github", "low")]),
            source("team", 10, vec![invalid("github")]),
        ];
        let resolution = resolve(&sources, &[select("github")]);
        let server = &resolution.servers[0];
        assert_eq!(server.status, Status::Hold);
        assert_eq!(server.reason, Some(HoldReason::InvalidWinner));
        assert_eq!(server.source.as_deref(), Some("team"));
        assert!(server.definition.is_none());
        assert_eq!(
            codes(&resolution),
            [
                ProblemCode::InvalidDefinition,
                ProblemCode::InvalidWinner,
                ProblemCode::Shadowed
            ]
        );
    }

    #[test]
    fn invalid_stem_shadows_nothing() {
        let bad_stem = FileEntry {
            file: "servers/GitHub.toml".to_owned(),
            key: None,
            outcome: Err(FileProblem::file(Rule::InvalidStem)),
            override_sites: OverrideSites::default(),
            findings: Vec::new(),
            gate: None,
        };
        let sources = [
            source("local", 0, vec![valid("github", "low")]),
            source("team", 10, vec![bad_stem]),
        ];
        let resolution = resolve(&sources, &[select("github")]);
        assert_eq!(resolution.servers[0].status, Status::Resolved);
        assert_eq!(resolution.servers[0].source.as_deref(), Some("local"));
        assert_eq!(codes(&resolution), [ProblemCode::InvalidDefinition]);
        assert_eq!(resolution.problems[0].server, None);
    }

    #[test]
    fn unavailable_higher_source_holds() {
        let sources = [
            source("local", 0, vec![valid("github", "low")]),
            unavailable("team", 10),
            source("low", 5, vec![valid("other", "x")]),
        ];
        let resolution = resolve(&sources, &[select("github"), select("other")]);
        for server in &resolution.servers {
            assert_eq!(server.status, Status::Hold);
            assert_eq!(server.reason, Some(HoldReason::SourceUnavailable));
            assert_eq!(server.source.as_deref(), Some("team"));
        }
        assert_eq!(
            codes(&resolution),
            [
                ProblemCode::SourceUnavailable,
                ProblemCode::SourceUnavailable,
                ProblemCode::SourceUnavailable
            ]
        );
    }

    #[test]
    fn unavailable_lower_source_does_not_hold_a_winner() {
        let sources = [
            unavailable("old", 1),
            source("team", 10, vec![valid("github", "x")]),
        ];
        let resolution = resolve(&sources, &[select("github"), select("missing")]);
        assert_eq!(resolution.servers[0].status, Status::Resolved);
        // No winner while a source is unavailable: held, not "not offered".
        assert_eq!(resolution.servers[1].status, Status::Hold);
        assert_eq!(
            resolution.servers[1].reason,
            Some(HoldReason::SourceUnavailable)
        );
    }

    #[test]
    fn not_offered() {
        let resolution = resolve(&[source("local", 0, Vec::new())], &[select("github")]);
        assert_eq!(resolution.servers[0].status, Status::NotOffered);
        assert_eq!(resolution.servers[0].source, None);
        assert_eq!(codes(&resolution), [ProblemCode::NotOffered]);
    }

    #[test]
    fn notices_are_reported() {
        let mut local = source("local", 0, Vec::new());
        local.notices = vec![
            Notice {
                file: "servers/notes.md".to_owned(),
                rule: Rule::NonTomlFile,
            },
            Notice {
                file: "servers/.hidden.toml".to_owned(),
                rule: Rule::HiddenFile,
            },
        ];
        let resolution = resolve(&[local], &[]);
        assert!(resolution.servers.is_empty());
        assert_eq!(
            codes(&resolution),
            [ProblemCode::Notice, ProblemCode::Notice]
        );
        assert_eq!(
            resolution.problems[0].file.as_deref(),
            Some("servers/.hidden.toml")
        );
    }

    #[test]
    fn duplicate_selections_resolve_independently_of_input_order() {
        use crate::config::Harness;
        let sources = [source("local", 0, vec![valid("github", "x")])];
        let codex = Selection {
            server: "github".to_owned(),
            harnesses: HarnessSet::Only([Harness::Codex].into()),
        };
        let all = select("github");
        let forward = resolve(&sources, &[codex.clone(), all.clone()]);
        let backward = resolve(&sources, &[all, codex]);
        assert_eq!(forward, backward);
        assert_eq!(forward.servers.len(), 1);
        assert_eq!(forward.servers[0].harnesses, HarnessSet::All);
    }

    fn gated(name: &str, command: &str, gate: GateVerdict, findings: usize) -> FileEntry {
        let finding = crate::audit::Finding {
            rule: crate::audit::AuditRule::UnpinnedNpmPackage,
            launcher: "npx".to_owned(),
            args: vec!["pkg".to_owned()],
        };
        FileEntry {
            gate: Some(gate),
            findings: vec![finding; findings],
            ..valid(name, command)
        }
    }

    #[test]
    fn the_gate_holds_or_revokes_without_fall_through() {
        for (gate, status, reason, code) in [
            (
                GateVerdict::PendingApproval,
                Status::Hold,
                Some(HoldReason::PendingApproval),
                Some(ProblemCode::PendingApproval),
            ),
            (
                GateVerdict::RiskNotAccepted,
                Status::Hold,
                Some(HoldReason::RiskNotAccepted),
                Some(ProblemCode::RiskNotAccepted),
            ),
            (GateVerdict::Revoked, Status::Revoked, None, None),
        ] {
            let sources = [
                source("local", 0, vec![valid("github", "low")]),
                source("personal", 10, vec![gated("github", "high", gate, 1)]),
            ];
            let resolution = resolve(&sources, &[select("github")]);
            let server = &resolution.servers[0];
            assert_eq!(server.status, status, "{gate:?}");
            assert_eq!(server.reason, reason);
            assert_eq!(server.source.as_deref(), Some("personal"));
            assert!(server.definition.is_none());
            let gated = server.gated.as_ref().unwrap();
            assert_eq!(gated.verdict, gate);
            assert_eq!(
                gated.definition_hash,
                definition("github", "high").definition_hash()
            );
            assert_eq!(gated.findings.len(), 1);
            let mut expected = vec![ProblemCode::Shadowed, ProblemCode::AuditFinding];
            expected.extend(code);
            expected.sort();
            assert_eq!(codes(&resolution), expected);
        }
    }

    fn revoked_for(mut snapshot: SourceSnapshot, name: &str) -> SourceSnapshot {
        snapshot.revoked.insert(name.to_owned());
        snapshot
    }

    /// A revoked marker removes what was revoked even when the definition is
    /// now invalid, gone upstream or its source is unavailable; a revoked
    /// marker on a source below a valid winner changes nothing.
    #[test]
    fn a_revoked_marker_wins_over_every_hold() {
        let revoked = |sources: &[SourceSnapshot]| {
            let resolution = resolve(sources, &[select("github")]);
            let server = resolution.servers[0].clone();
            (server.status, server.source, server.file, resolution)
        };
        // An invalid winner.
        let (status, origin, file, resolution) = revoked(&[
            source("local", 0, vec![valid("github", "low")]),
            revoked_for(source("personal", 10, vec![invalid("github")]), "github"),
        ]);
        assert_eq!(status, Status::Revoked);
        assert_eq!(origin.as_deref(), Some("personal"));
        assert_eq!(file.as_deref(), Some("servers/github.toml"));
        assert!(resolution.servers[0].gated.is_none());
        assert!(!codes(&resolution).contains(&ProblemCode::InvalidWinner));
        // Removed upstream, nothing else offers it.
        let (status, origin, file, resolution) = revoked(&[
            source("local", 0, Vec::new()),
            revoked_for(source("personal", 10, Vec::new()), "github"),
        ]);
        assert_eq!(status, Status::Revoked);
        assert_eq!(origin.as_deref(), Some("personal"));
        assert!(file.is_none());
        assert!(codes(&resolution).is_empty());
        // The revoked source is unavailable and ranks above the winner.
        let (status, origin, _, _) = revoked(&[
            source("local", 0, vec![valid("github", "low")]),
            revoked_for(unavailable("personal", 10), "github"),
        ]);
        assert_eq!(status, Status::Revoked);
        assert_eq!(origin.as_deref(), Some("personal"));
        // A revoked winner below an unavailable source is revoked, not held.
        let (status, origin, _, _) = revoked(&[
            revoked_for(
                source(
                    "personal",
                    10,
                    vec![gated("github", "x", GateVerdict::Revoked, 0)],
                ),
                "github",
            ),
            unavailable("team", 20),
        ]);
        assert_eq!(status, Status::Revoked);
        assert_eq!(origin.as_deref(), Some("personal"));
        // Removed upstream, but a lower source offers it: that one wins.
        let (status, origin, _, _) = revoked(&[
            source("local", 0, vec![valid("github", "low")]),
            revoked_for(source("personal", 10, Vec::new()), "github"),
        ]);
        assert_eq!(status, Status::Resolved);
        assert_eq!(origin.as_deref(), Some("local"));
        // A marker below the winner changes nothing.
        let (status, origin, _, _) = revoked(&[
            revoked_for(source("low", 0, vec![valid("github", "low")]), "github"),
            source("high", 10, vec![valid("github", "high")]),
        ]);
        assert_eq!(status, Status::Resolved);
        assert_eq!(origin.as_deref(), Some("high"));
        // A marker for another name changes nothing.
        let (status, _, _, _) = revoked(&[revoked_for(source("local", 0, Vec::new()), "linear")]);
        assert_eq!(status, Status::NotOffered);
    }

    /// A detached marker (its source was removed) revokes a server no source
    /// offers, also over an unavailable source; an offered name resolves.
    #[test]
    fn a_detached_revoked_marker_revokes_only_what_nothing_offers() {
        let detached = BTreeSet::from([
            ApprovalKey::new("gone", "github"),
            ApprovalKey::new("old", "github"),
        ]);
        let resolve = |sources: &[SourceSnapshot]| {
            let resolution = resolve_with_detached(sources, &[select("github")], &detached);
            resolution.servers[0].clone()
        };
        let server = resolve(&[source("local", 0, Vec::new())]);
        assert_eq!(server.status, Status::Revoked);
        // The first marker in (source, server) order names the source.
        assert_eq!(server.source.as_deref(), Some("gone"));
        assert!(server.file.is_none() && server.gated.is_none());
        let server = resolve(&[source("local", 0, Vec::new()), unavailable("team", 10)]);
        assert_eq!(server.status, Status::Revoked);
        let server = resolve(&[source("local", 0, vec![valid("github", "low")])]);
        assert_eq!(server.status, Status::Resolved);
        assert_eq!(server.source.as_deref(), Some("local"));
        let server = resolve_with_detached(
            &[source("local", 0, Vec::new())],
            &[select("github")],
            &BTreeSet::from([ApprovalKey::new("gone", "linear")]),
        );
        assert_eq!(server.servers[0].status, Status::NotOffered);
    }

    #[test]
    fn approved_and_directory_winners_resolve_with_audit_warnings() {
        let sources = [source(
            "personal",
            10,
            vec![gated("github", "x", GateVerdict::Approved, 2)],
        )];
        let resolution = resolve(&sources, &[select("github")]);
        let server = &resolution.servers[0];
        assert_eq!(server.status, Status::Resolved);
        assert!(server.definition.is_some());
        assert_eq!(
            server.gated.as_ref().unwrap().verdict,
            GateVerdict::Approved
        );
        assert_eq!(
            codes(&resolution),
            [ProblemCode::AuditFinding, ProblemCode::AuditFinding]
        );
        assert!(
            resolution.problems[0]
                .message
                .contains("needs risk acceptance")
        );
        let mut directory = valid("github", "x");
        directory.findings = gated("github", "x", GateVerdict::Approved, 1).findings;
        let resolution = resolve(&[source("local", 0, vec![directory])], &[select("github")]);
        assert_eq!(resolution.servers[0].status, Status::Resolved);
        assert!(resolution.servers[0].gated.is_none());
        assert_eq!(codes(&resolution), [ProblemCode::AuditFinding]);
        assert!(resolution.problems[0].message.contains("warning only"));
        assert_eq!(resolution.problems[0].source.as_deref(), Some("local"));
    }

    #[test]
    fn problems_serialize_with_every_key() {
        let resolution = resolve(&[source("local", 0, Vec::new())], &[select("a")]);
        let json = serde_json::to_value(&resolution.problems[0]).unwrap();
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "code", "column", "field", "file", "line", "message", "rule", "server", "source"
            ]
        );
        assert_eq!(json["code"], "not-offered");
    }
}
