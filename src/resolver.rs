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
//! - `not-offered`: no source offers the name.
//!
//! WP2 turns `hold` into its *Held* operation (keep the existing managed entry
//! unchanged), and WP5 adds the reason `pending-approval`.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::Serialize;

use crate::config::{HarnessSet, Selection};
use crate::definition::{Definition, DefinitionError};
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
}

/// Why a server is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldReason {
    /// The highest-priority definition is invalid.
    InvalidWinner,
    /// An unavailable source could change the result.
    SourceUnavailable,
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

/// A candidate definition for a key.
struct Candidate<'a> {
    source: &'a SourceSnapshot,
    file: &'a FileEntry,
}

/// Resolves the selections against the source snapshots.
#[must_use]
pub fn resolve(sources: &[SourceSnapshot], selections: &[Selection]) -> Resolution {
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
            resolve_one(selection, winner, &ordered, &mut problems)
        })
        .collect();

    problems.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    Resolution { servers, problems }
}

fn resolve_one(
    selection: &Selection,
    winner: Option<&Candidate<'_>>,
    ordered: &[&SourceSnapshot],
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
    };
    let winner_priority = winner.map(|candidate| candidate.source.priority);
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
            resolution.definition = Some(definition.clone());
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
        }
    }

    fn invalid(name: &str) -> FileEntry {
        FileEntry {
            file: format!("servers/{name}.toml"),
            key: Some(name.to_owned()),
            outcome: Err(FileProblem::file(Rule::TomlSyntax)),
        }
    }

    fn source(id: &str, priority: u32, files: Vec<FileEntry>) -> SourceSnapshot {
        SourceSnapshot {
            id: id.to_owned(),
            priority,
            available: true,
            files,
            notices: Vec::new(),
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
