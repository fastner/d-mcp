//! The approval commands: `approve`, `approve --all-from`, `accept-risk`,
//! `revoke`, `source trust` and `select --all-from`.
//!
//! Each takes no prompt and supports `--dry-run` and `--json`. The preview
//! (a dry run, or the report of an applied run) shows a Git-source
//! definition through the approval view ([`crate::redact::approval_view`]),
//! with its `definition_hash` and audit findings. Applying binds to the
//! preview: `approve` and `accept-risk` need `--expect <definition_hash>`,
//! `approve --all-from` needs `--expect <pinned commit>`; under the store
//! lock everything is read again and a mismatch is `approval-changed`
//! (exit 3) with nothing written.

use std::cmp::Reverse;

use serde_json::{Value, json};

use crate::approval::{self, AcceptedRisk, ApprovalKey, Approvals, MAX_REASON_CHARS};
use crate::audit::findings_hash;
use crate::config::{Config, HarnessSet, LOCAL_SOURCE_ID, Selection, SourceOrigin};
use crate::credential::secret_value_heuristic;
use crate::definition::{Definition, is_valid_name};
use crate::error::{Error, Result};
use crate::hashkey;
use crate::output::{Report, Ui};
use crate::redact::approval_view;
use crate::resolver::{FileEntry, FileProblem, GateVerdict, SourceSnapshot};
use crate::source::{ReadInputs, load_snapshots, offered_names, prepare_snapshots};
use crate::state;
use crate::store::{StoreLock, StorePaths};

use super::{mutate, prefix, source_json, user_spec};

/// The store read for an approval command: config, lock and approvals, and
/// the validated and gated snapshots.
struct Loaded {
    config: Config,
    inputs: ReadInputs,
    snapshots: Vec<SourceSnapshot>,
}

impl Loaded {
    fn read(paths: &StorePaths) -> Result<Self> {
        let config = paths.load_config()?;
        let inputs = ReadInputs::load(paths)?;
        let mut snapshots = load_snapshots(paths, &config, &inputs);
        prepare_snapshots(&mut snapshots, &config, &inputs);
        Ok(Self {
            config,
            inputs,
            snapshots,
        })
    }

    /// The URL of a registered Git source.
    fn git_url(&self, id: &str) -> Option<&str> {
        self.config
            .source(id)
            .and_then(|source| source.git())
            .map(|git| git.url.as_str())
    }

    fn snapshot(&self, id: &str) -> Option<&SourceSnapshot> {
        self.snapshots.iter().find(|snapshot| snapshot.id == id)
    }
}

/// The (source, server) an approval command acts on, and its file.
struct Target<'a> {
    key: ApprovalKey,
    url: String,
    file: Option<&'a FileEntry>,
}

impl Target<'_> {
    fn definition(&self) -> Option<&Definition> {
        self.file.and_then(|file| file.outcome.as_ref().ok())
    }

    fn findings(&self) -> &[crate::audit::Finding] {
        self.file.map_or(&[], |file| file.findings.as_slice())
    }

    fn definition_hash(&self) -> Option<String> {
        self.definition().map(Definition::definition_hash)
    }

    fn problem(&self) -> Option<&FileProblem> {
        self.file.and_then(|file| file.outcome.as_ref().err())
    }
}

/// Checks the server name and source id arguments against the name rule
/// before anything is read, so an invalid name never reaches
/// `approvals.toml` or `config.toml`.
fn check_names(server: Option<&str>, source: Option<&str>) -> Result<()> {
    if server.is_some_and(|server| !is_valid_name(server)) {
        return Err(Error::InvalidName {
            what: "server name",
        });
    }
    if source.is_some_and(|source| !is_valid_name(source)) {
        return Err(Error::InvalidName { what: "source id" });
    }
    Ok(())
}

fn not_needed(server: &str, what: &'static str, why: &'static str) -> Error {
    Error::ApprovalNotNeeded {
        server: server.to_owned(),
        what,
        why,
    }
}

/// Finds the target: with `source`, that source's file for `server`;
/// otherwise the highest-priority available file for `server`, which must
/// come from a Git source. `require_file` refuses a (source, server) that
/// the source does not offer.
fn find_target<'a>(
    loaded: &'a Loaded,
    server: &str,
    source: Option<&str>,
    what: &'static str,
    require_file: bool,
) -> Result<Target<'a>> {
    let offered = |snapshot: &'a SourceSnapshot| {
        snapshot
            .files
            .iter()
            .find(|file| file.key.as_deref() == Some(server))
    };
    let (id, file) = match source {
        Some(id) => {
            if id != LOCAL_SOURCE_ID && loaded.config.source(id).is_none() {
                return Err(Error::SourceNotFound { id: id.to_owned() });
            }
            (id, loaded.snapshot(id).and_then(offered))
        }
        None => {
            let mut ordered: Vec<&SourceSnapshot> = loaded
                .snapshots
                .iter()
                .filter(|snapshot| snapshot.available)
                .collect();
            ordered.sort_by(|a, b| {
                (Reverse(a.priority), a.id.as_str()).cmp(&(Reverse(b.priority), b.id.as_str()))
            });
            let Some((snapshot, file)) = ordered
                .into_iter()
                .find_map(|snapshot| offered(snapshot).map(|file| (snapshot, file)))
            else {
                return Err(not_needed(server, what, "no source offers it"));
            };
            (snapshot.id.as_str(), Some(file))
        }
    };
    let Some(url) = loaded.git_url(id) else {
        return Err(not_needed(
            server,
            what,
            "its definition comes from a directory source",
        ));
    };
    if require_file && file.is_none() {
        return Err(not_needed(
            server,
            what,
            "the Git source does not offer it (or is unavailable)",
        ));
    }
    Ok(Target {
        key: ApprovalKey::new(id, server),
        url: url.to_owned(),
        file,
    })
}

/// The preview fields every single-server command shares.
fn target_json(target: &Target<'_>) -> Value {
    let findings = target.findings();
    json!({
        "server": target.key.server,
        "source": target.key.source,
        "url": target.url,
        "file": target.file.map(|file| file.file.as_str()),
        "definition_hash": target.definition_hash(),
        "view": target.definition().map(approval_view),
        "findings": findings,
        "findings_hash": (!findings.is_empty()).then(|| findings_hash(findings)),
        "problem": target.problem().map(problem_json),
    })
}

fn problem_json(problem: &FileProblem) -> Value {
    json!({
        "rule": problem.rule,
        "field": problem.field,
        "line": problem.line,
        "column": problem.column,
    })
}

/// The text lines of a target preview.
fn target_text(target: &Target<'_>) -> String {
    let mut text = String::new();
    if let Some(hash) = target.definition_hash() {
        text.push_str(&format!("\n  definition_hash: {hash}"));
    }
    if let Some(definition) = target.definition() {
        text.push_str(&format!("\n  view: {}", approval_view(definition).text()));
    }
    if let Some(problem) = target.problem() {
        text.push_str(&format!("\n  invalid: rule `{}`", problem.rule));
    }
    for finding in target.findings() {
        text.push_str(&format!(
            "\n  finding: {} ({}) {}",
            finding.rule.as_str(),
            finding.launcher,
            serde_json::to_string(&finding.args).unwrap_or_default()
        ));
    }
    text
}

/// Takes the lock, creates a missing `.hash-key`, and reports its warning.
fn lock(paths: &StorePaths, ui: &mut Ui<'_>) -> Result<StoreLock> {
    let lock = StoreLock::acquire(paths)?;
    if let Some(warning) = hashkey::ensure_under_lock(paths, &lock)? {
        ui.warn(&warning.to_string());
    }
    Ok(lock)
}

fn save(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    lock: &StoreLock,
    approvals: &Approvals,
) -> Result<()> {
    if let Some(warning) = approval::save(paths, lock, approvals)? {
        ui.warn(&warning.to_string());
    }
    Ok(())
}

/// Why a target cannot be approved: `invalid-definition` or
/// `risk-not-accepted`.
fn blocked_reason(target: &Target<'_>, approvals: &Approvals) -> Option<&'static str> {
    let Some(hash) = target.definition_hash() else {
        return Some("invalid-definition");
    };
    (!approvals.risk_accepted(&target.key, &target.url, &hash, target.findings()))
        .then_some("risk-not-accepted")
}

/// Checks `--expect` against the current hash.
fn check_expect(server: &str, expect: Option<&str>, current: Option<&str>) -> Result<()> {
    match expect {
        Some(expect) if Some(expect) != current => Err(Error::ApprovalChanged {
            server: server.to_owned(),
        }),
        _ => Ok(()),
    }
}

/// `approve <server> [--source <id>] --expect <definition_hash>`.
pub(super) fn approve(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    server: &str,
    source: Option<&str>,
    expect: Option<&str>,
    dry_run: bool,
) -> Result<(Report, Option<Error>)> {
    paths.require_initialized()?;
    check_names(Some(server), source)?;
    let loaded = Loaded::read(paths)?;
    let target = find_target(&loaded, server, source, "approval", true)?;
    let hash = target.definition_hash();
    if !dry_run && expect.is_none() {
        return Err(Error::ExpectRequired {
            command: "approve",
            value: "definition_hash",
        });
    }
    let blocked = blocked_reason(&target, &loaded.inputs.approvals);
    if blocked.is_none() {
        check_expect(server, expect, hash.as_deref())?;
    }
    let mut newly = !hash.as_deref().is_some_and(|hash| {
        loaded
            .inputs
            .approvals
            .is_approved(&target.key, &target.url, hash)
    });
    if blocked.is_none() && !dry_run {
        let lock = lock(paths, ui)?;
        let locked = Loaded::read(paths)?;
        let current = find_target(&locked, server, source, "approval", true)?;
        let current_hash = current.definition_hash();
        check_expect(server, expect, current_hash.as_deref())?;
        if current.key != target.key
            || current.url != target.url
            || blocked_reason(&current, &locked.inputs.approvals).is_some()
        {
            return Err(Error::ApprovalChanged {
                server: server.to_owned(),
            });
        }
        let mut approvals = locked.inputs.approvals.clone();
        let hash = current_hash.unwrap_or_default();
        newly = approvals.approve(&current.key, &current.url, &hash);
        // Also when only a revoked marker was cleared.
        if approvals != locked.inputs.approvals {
            save(paths, ui, &lock, &approvals)?;
        }
    }
    let mut data = target_json(&target);
    // After the command; in a dry run, whether it is approved already.
    data["approved"] = json!(blocked.is_none() && (!dry_run || !newly));
    data["newly_approved"] = json!(blocked.is_none() && newly);
    data["blocked"] = json!(blocked);
    data["dry_run"] = json!(dry_run);
    let mut text = match blocked {
        Some(reason) => format!(
            "`{server}` from source `{}` is blocked from approval ({reason})",
            target.key.source
        ),
        None if dry_run => format!(
            "Dry run: would have approved `{server}` from source `{}`; apply with --source {} --expect {}",
            target.key.source,
            target.key.source,
            hash.as_deref().unwrap_or_default()
        ),
        None if newly => format!("Approved `{server}` from source `{}`", target.key.source),
        None => format!(
            "`{server}` from source `{}` is already approved",
            target.key.source
        ),
    };
    text.push_str(&target_text(&target));
    if blocked == Some("risk-not-accepted") {
        text.push_str(&format!(
            "\n  accept the findings first: d-mcp accept-risk {server} --reason <text> --expect {}",
            hash.as_deref().unwrap_or_default()
        ));
    }
    let error = blocked.map(|_| Error::ApprovalBlocked {
        count: 1,
        untrusted: None,
    });
    Ok((Report::new(data, text), error))
}

/// One server of `approve --all-from`.
struct Planned {
    server: String,
    file: String,
    definition_hash: Option<String>,
    findings: Vec<crate::audit::Finding>,
    blocked: Option<&'static str>,
    newly: bool,
}

/// What `approve --all-from` would approve and block.
fn plan_all(loaded: &Loaded, id: &str, url: &str) -> Vec<Planned> {
    let Some(snapshot) = loaded.snapshot(id) else {
        return Vec::new();
    };
    let approvals = &loaded.inputs.approvals;
    let mut files: Vec<&FileEntry> = snapshot
        .files
        .iter()
        .filter(|file| file.key.is_some())
        .collect();
    files.sort_by(|a, b| a.key.cmp(&b.key));
    files
        .into_iter()
        .map(|file| {
            let server = file.key.clone().unwrap_or_default();
            let target = Target {
                key: ApprovalKey::new(id, &server),
                url: url.to_owned(),
                file: Some(file),
            };
            let hash = target.definition_hash();
            let blocked = if approvals.is_revoked(&target.key) {
                Some("revoked")
            } else {
                blocked_reason(&target, approvals)
            };
            let newly = !hash
                .as_deref()
                .is_some_and(|hash| approvals.is_approved(&target.key, url, hash));
            Planned {
                server,
                file: file.file.clone(),
                definition_hash: hash,
                findings: file.findings.clone(),
                blocked,
                newly,
            }
        })
        .collect()
}

/// `approve --all-from <source> --expect <pinned commit>`.
pub(super) fn approve_all(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    id: &str,
    expect: Option<&str>,
    dry_run: bool,
) -> Result<(Report, Option<Error>)> {
    paths.require_initialized()?;
    check_names(None, Some(id))?;
    let loaded = Loaded::read(paths)?;
    let Some(entry) = loaded.config.source(id) else {
        return Err(if id == LOCAL_SOURCE_ID {
            not_needed(id, "approval", "it is a directory source")
        } else {
            Error::SourceNotFound { id: id.to_owned() }
        });
    };
    let Some(git) = entry.git() else {
        return Err(not_needed(id, "approval", "it is a directory source"));
    };
    if !git.trusted {
        return Err(Error::ApprovalBlocked {
            count: 0,
            untrusted: Some(id.to_owned()),
        });
    }
    if !dry_run && expect.is_none() {
        return Err(Error::ExpectRequired {
            command: "approve --all-from",
            value: "pinned commit",
        });
    }
    let pin = loaded
        .inputs
        .lock
        .pin(id)
        .map(|pin| pin.as_str().to_owned());
    check_expect(id, expect, pin.as_deref())?;
    let mut planned = plan_all(&loaded, id, &git.url);
    if !dry_run {
        let lock = lock(paths, ui)?;
        let locked = Loaded::read(paths)?;
        let same_source = locked.config.source(id) == Some(entry);
        let locked_pin = locked
            .inputs
            .lock
            .pin(id)
            .map(|pin| pin.as_str().to_owned());
        if !same_source || locked_pin != pin {
            return Err(Error::ApprovalChanged {
                server: id.to_owned(),
            });
        }
        planned = plan_all(&locked, id, &git.url);
        let mut approvals = locked.inputs.approvals;
        let mut changed = false;
        for item in planned.iter_mut().filter(|item| item.blocked.is_none()) {
            let hash = item.definition_hash.clone().unwrap_or_default();
            item.newly = approvals.approve(&ApprovalKey::new(id, &item.server), &git.url, &hash);
            changed |= item.newly;
        }
        if changed {
            save(paths, ui, &lock, &approvals)?;
        }
    }
    let spec = user_spec(paths, entry, &loaded.inputs.lock);
    let approved: Vec<Value> = planned
        .iter()
        .filter(|item| item.blocked.is_none())
        .map(|item| {
            json!({
                "server": item.server,
                "file": item.file,
                "definition_hash": item.definition_hash,
                "newly_approved": item.newly,
            })
        })
        .collect();
    let blocked: Vec<Value> = planned
        .iter()
        .filter_map(|item| {
            item.blocked.map(|reason| {
                json!({
                    "server": item.server,
                    "file": item.file,
                    "reason": reason,
                    "definition_hash": item.definition_hash,
                    "findings": item.findings,
                })
            })
        })
        .collect();
    let mut text = format!(
        "{}{} {} {} from source `{id}` at {}",
        prefix(dry_run),
        if dry_run { "approved" } else { "Approved" },
        approved.len(),
        if approved.len() == 1 {
            "server"
        } else {
            "servers"
        },
        pin.as_deref().map_or("-", |pin| &pin[..pin.len().min(12)]),
    );
    for item in &planned {
        match (item.blocked, &item.definition_hash) {
            (None, Some(hash)) => text.push_str(&format!("\n  approved {}: {hash}", item.server)),
            (Some(reason), _) => {
                text.push_str(&format!("\n  blocked {}: {reason}", item.server));
            }
            (None, None) => {}
        }
    }
    let error = (!blocked.is_empty()).then_some(Error::ApprovalBlocked {
        count: blocked.len(),
        untrusted: None,
    });
    let data = json!({
        "source": source_json(&spec),
        "commit": pin,
        "approved": approved,
        "blocked": blocked,
        "dry_run": dry_run,
    });
    Ok((Report::new(data, text), error))
}

/// Checks a risk-acceptance reason without ever echoing it.
fn check_reason(reason: &str) -> Result<()> {
    let why = if reason.trim().is_empty() {
        "it is empty"
    } else if reason.chars().count() > MAX_REASON_CHARS {
        "it is longer than 500 characters"
    } else if reason.chars().any(char::is_control) {
        "it contains control characters (use one line)"
    } else if secret_value_heuristic(reason).is_some() {
        "it looks like it contains a secret"
    } else {
        return Ok(());
    };
    Err(Error::ReasonInvalid { why })
}

/// `accept-risk <server> --reason <text> --expect <definition_hash>`.
pub(super) fn accept_risk(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    server: &str,
    reason: &str,
    expect: Option<&str>,
    dry_run: bool,
) -> Result<Report> {
    paths.require_initialized()?;
    check_names(Some(server), None)?;
    check_reason(reason)?;
    let loaded = Loaded::read(paths)?;
    let target = find_target(&loaded, server, None, "risk acceptance", true)?;
    let Some(hash) = target.definition_hash() else {
        return Err(Error::ApprovalBlocked {
            count: 1,
            untrusted: None,
        });
    };
    if target.findings().is_empty() {
        return Err(not_needed(
            server,
            "risk acceptance",
            "it has no audit findings",
        ));
    }
    if !dry_run && expect.is_none() {
        return Err(Error::ExpectRequired {
            command: "accept-risk",
            value: "definition_hash",
        });
    }
    check_expect(server, expect, Some(&hash))?;
    let mut approvals = loaded.inputs.approvals.clone();
    if !dry_run {
        let lock = lock(paths, ui)?;
        let locked = Loaded::read(paths)?;
        let current = find_target(&locked, server, None, "risk acceptance", true)?;
        if current.key != target.key
            || current.url != target.url
            || current.definition_hash().as_deref() != Some(hash.as_str())
            || current.findings() != target.findings()
        {
            return Err(Error::ApprovalChanged {
                server: server.to_owned(),
            });
        }
        approvals = locked.inputs.approvals.clone();
        approvals.accept_risk(
            &current.key,
            AcceptedRisk {
                url: current.url.clone(),
                definition_hash: hash.clone(),
                findings_hash: findings_hash(current.findings()),
                reason: reason.to_owned(),
                accepted_at: state::now(),
            },
        );
        save(paths, ui, &lock, &approvals)?;
    }
    let verdict = approvals.verdict(&target.key, &target.url, &hash, target.findings());
    let mut data = target_json(&target);
    data["approved"] = json!(!dry_run && verdict == GateVerdict::Approved);
    data["dry_run"] = json!(dry_run);
    let mut text = format!(
        "{}{} the audit findings of `{server}` from source `{}`",
        prefix(dry_run),
        if dry_run { "accepted" } else { "Accepted" },
        target.key.source
    );
    text.push_str(&target_text(&target));
    if dry_run {
        text.push_str(&format!("\n  apply with --expect {hash}"));
    }
    if verdict != GateVerdict::Approved {
        text.push_str(&format!(
            "\n  this does not approve it: d-mcp approve {server} --source {} --expect {hash}",
            target.key.source
        ));
    }
    Ok(Report::new(data, text))
}

/// `revoke <server> [--source <id>]`.
pub(super) fn revoke(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    server: &str,
    source: Option<&str>,
    dry_run: bool,
) -> Result<Report> {
    paths.require_initialized()?;
    check_names(Some(server), source)?;
    let loaded = Loaded::read(paths)?;
    let target = find_target(&loaded, server, source, "approval", false)?;
    let mut had_approval = loaded.inputs.approvals.approval(&target.key).is_some();
    if !dry_run {
        let lock = lock(paths, ui)?;
        let locked = Loaded::read(paths)?;
        let current = find_target(&locked, server, source, "approval", false)?;
        if current.key != target.key || current.url != target.url {
            return Err(Error::ApprovalChanged {
                server: server.to_owned(),
            });
        }
        let mut approvals = locked.inputs.approvals.clone();
        had_approval = approvals.revoke(&current.key, &current.url);
        if approvals != locked.inputs.approvals {
            save(paths, ui, &lock, &approvals)?;
        }
    }
    let text = format!(
        "{}{} `{server}` from source `{}`{}",
        prefix(dry_run),
        if dry_run { "revoked" } else { "Revoked" },
        target.key.source,
        if had_approval {
            "; the next sync removes its managed entries"
        } else {
            " (it had no approval)"
        }
    );
    Ok(Report::new(
        json!({
            "server": server,
            "source": target.key.source,
            "url": target.url,
            "had_approval": had_approval,
            "dry_run": dry_run,
        }),
        text,
    ))
}

/// `source trust <id> --on|--off`. Trust only allows `approve --all-from`;
/// on a directory source it has no effect.
pub(super) fn trust(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    id: &str,
    on: bool,
    dry_run: bool,
) -> Result<Report> {
    paths.require_initialized()?;
    check_names(None, Some(id))?;
    let (entry, changed) = mutate(paths, ui, dry_run, |config| {
        let Some(entry) = config.sources.iter_mut().find(|source| source.id == id) else {
            return Err(if id == LOCAL_SOURCE_ID {
                Error::SourceIdReserved
            } else {
                Error::SourceNotFound { id: id.to_owned() }
            });
        };
        let changed = match &mut entry.origin {
            SourceOrigin::Git(git) => {
                let changed = git.trusted != on;
                git.trusted = on;
                changed
            }
            SourceOrigin::Directory { .. } => false,
        };
        Ok((entry.clone(), changed))
    })?;
    if entry.git().is_none() {
        ui.warn(&format!(
            "source `{id}` is a directory source; trust has no effect on it"
        ));
    }
    let pins = crate::lockfile::load(paths)?;
    let spec = user_spec(paths, &entry, &pins);
    let state = if on { "trusted" } else { "untrusted" };
    let text = if entry.git().is_none() {
        format!("Source `{id}` is a directory source; nothing changed")
    } else if changed {
        format!(
            "{}{} source `{id}` {state}",
            prefix(dry_run),
            if dry_run { "marked" } else { "Marked" }
        )
    } else {
        format!("Source `{id}` is already {state}")
    };
    Ok(Report::new(
        json!({ "source": source_json(&spec), "changed": changed, "dry_run": dry_run }),
        text,
    ))
}

/// `select --all-from <source>`: selects every server the source offers
/// that is not selected yet, for all harnesses. It never unselects or
/// changes an existing selection.
pub(super) fn select_all_from(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    id: &str,
    dry_run: bool,
) -> Result<Report> {
    paths.require_initialized()?;
    check_names(None, Some(id))?;
    let (offered, selected) = mutate(paths, ui, dry_run, |config| {
        if id != LOCAL_SOURCE_ID && config.source(id).is_none() {
            return Err(Error::SourceNotFound { id: id.to_owned() });
        }
        let inputs = ReadInputs::load(paths)?;
        let mut offered: Vec<String> = load_snapshots(paths, config, &inputs)
            .iter()
            .find(|snapshot| snapshot.id == id)
            .map(offered_names)
            .unwrap_or_default();
        offered.sort();
        offered.dedup();
        let mut selected = Vec::new();
        for name in &offered {
            if config.selection(name).is_none() {
                config.select(Selection {
                    server: name.clone(),
                    harnesses: HarnessSet::All,
                });
                selected.push(name.clone());
            }
        }
        Ok((offered, selected))
    })?;
    if offered.is_empty() {
        ui.warn(&format!(
            "source `{id}` offers no servers (or is unavailable)"
        ));
    }
    let already: Vec<&String> = offered
        .iter()
        .filter(|name| !selected.contains(name))
        .collect();
    let mut text = format!(
        "{}{} {} {} from source `{id}` for all",
        prefix(dry_run),
        if dry_run { "selected" } else { "Selected" },
        selected.len(),
        if selected.len() == 1 {
            "server"
        } else {
            "servers"
        },
    );
    if !selected.is_empty() {
        text.push_str(&format!(": {}", selected.join(", ")));
    }
    if !already.is_empty() {
        text.push_str(&format!(
            "\n  already selected: {}",
            already
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Report::new(
        json!({
            "source": id,
            "selected": selected,
            "already_selected": already,
            "dry_run": dry_run,
        }),
        text,
    ))
}
