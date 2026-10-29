//! `adopt --apply` under the store lock: the `--expect` binding, then the
//! definition, then the selection, then the take-over as a scoped sync.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use super::origin::WriteMode;
use super::{Adoption, Context, Refusal, ScopeKind, Written};
use crate::config::{Config, Harness, HarnessSet, Selection};
use crate::definition::to_toml;
use crate::error::Error;
use crate::state::{AdoptedFrom, OriginScope};
use crate::store::{self, StoreLock};
use crate::sync::{BoundTakeOver, ExpectedHash, SyncOptions, sync_under_lock};

/// Checks that every origin has an `--expect` and that its display hash
/// still matches the entry just read. Returns the refusals (a missing or
/// unknown `--expect`) and the first changed entry.
pub(super) fn check_expectations(
    context: &Context<'_>,
    adoption: &Adoption,
) -> (Vec<Refusal>, Option<Error>) {
    let expects = &context.options.expect;
    let mut refusals = Vec::new();
    let mut changed = None;
    let mut matched = BTreeSet::new();
    for origin in adoption.servers.iter().flat_map(|server| &server.origins) {
        let label = origin.label();
        let Some((index, expect)) = expects
            .iter()
            .enumerate()
            .find(|(_, expect)| expect.entry == origin.entry)
        else {
            refusals.push(Refusal::new(Some(label), "", "expect-missing"));
            continue;
        };
        matched.insert(index);
        if origin.display_hash.as_deref() != Some(expect.display_hash.as_str()) {
            changed.get_or_insert(Error::AdoptEntryChanged { entry: label });
        }
    }
    for (index, expect) in expects.iter().enumerate() {
        if !matched.contains(&index) {
            refusals.push(Refusal::new(
                Some(expect.entry.to_string()),
                "",
                "expect-unknown",
            ));
        }
    }
    (refusals, changed)
}

/// Writes the definitions and the selections, then takes the entries over
/// with a scoped sync. A failure after the first write is reported per
/// entry and returned as the error; definitions and selections stay.
pub(super) fn apply(
    context: &Context<'_>,
    lock: &StoreLock,
    mut config: Config,
    adoption: &mut Adoption,
    warnings: &mut Vec<String>,
) -> Option<Error> {
    let paths = context.paths;
    let mut written = Written::default();
    for server in &adoption.servers {
        let Some(definition) = &server.definition else {
            continue;
        };
        match store::write_atomic(&server.file, to_toml(definition).as_bytes()) {
            Ok(warning) => warnings.extend(warning.map(|warning| warning.to_string())),
            Err(error) => {
                adoption.written = Some(written);
                return Some(error);
            }
        }
        written.definitions.push(server.file.clone());
    }
    for server in &adoption.servers {
        config.select(Selection {
            server: server.name.clone(),
            harnesses: HarnessSet::Only(server.harnesses.clone()),
        });
    }
    match paths.save_config(lock, &config) {
        Ok(warning) => warnings.extend(warning.map(|warning| warning.to_string())),
        Err(error) => {
            adoption.written = Some(written);
            return Some(error);
        }
    }
    written.selections = adoption
        .servers
        .iter()
        .map(|server| server.name.clone())
        .collect();
    adoption.written = Some(written);
    let mut options = SyncOptions {
        only: adoption
            .servers
            .iter()
            .map(|server| server.name.clone())
            .collect(),
        var: context.options.var,
        ..SyncOptions::default()
    };
    for server in &adoption.servers {
        for origin in &server.origins {
            let harness = origin.entry.place.harness;
            options.harnesses.insert(harness);
            let scope = match (&origin.entry.place.scope, &origin.path) {
                (ScopeKind::Project, Some(root)) => OriginScope::Project { root: root.clone() },
                _ => OriginScope::User,
            };
            let expected = match origin.write {
                Some(WriteMode::Add) => None,
                _ => origin.hash.clone().map(ExpectedHash::new),
            };
            options.bound.entry(harness).or_default().insert(
                server.name.clone(),
                BoundTakeOver {
                    expected,
                    adopted_from: AdoptedFrom { harness, scope },
                    replaced_fields: origin.replaced_fields.clone(),
                },
            );
        }
    }
    let (report, error) = match sync_under_lock(paths, context.env, lock, &options, warnings) {
        Ok(result) => result,
        Err(error) => {
            adoption.notices.push(failed_after_write());
            return Some(error);
        }
    };
    record_outcomes(adoption, &report.data);
    adoption.sync = Some(report.data);
    error
}

fn failed_after_write() -> String {
    "the definitions and selections above were written; the take-over did not complete, see the entries".to_owned()
}

/// The outcome and error of one harness entry in the `sync` data, or the
/// harness error when the entry was not reached.
fn outcome_of(data: &Value, harness: Harness, name: &str) -> Option<(String, Option<Value>)> {
    let report = data["harnesses"]
        .as_array()?
        .iter()
        .find(|report| report["harness"] == harness.as_str())?;
    let entry = report["entries"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["name"] == name));
    match entry {
        Some(entry) => Some((
            entry["outcome"].as_str().unwrap_or_default().to_owned(),
            Some(entry["error"].clone()).filter(|error| !error.is_null()),
        )),
        None => Some((
            report["status"].as_str().unwrap_or_default().to_owned(),
            Some(report["error"].clone()).filter(|error| !error.is_null()),
        )),
    }
}

/// Copies the scoped sync's outcomes into the origins and explains every
/// failure after the definition write.
fn record_outcomes(adoption: &mut Adoption, data: &Value) {
    let mut notices = Vec::new();
    for server in &mut adoption.servers {
        let variables: Vec<String> = server
            .variables
            .iter()
            .map(|check| check.name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut failed: BTreeMap<Harness, String> = BTreeMap::new();
        for origin in &mut server.origins {
            let harness = origin.entry.place.harness;
            let Some((outcome, error)) = outcome_of(data, harness, &server.name) else {
                continue;
            };
            if !matches!(outcome.as_str(), "applied" | "recorded" | "unchanged") {
                failed.insert(harness, outcome.clone());
            }
            origin.outcome = Some(outcome);
            origin.error = error;
        }
        for (harness, outcome) in failed {
            if harness == Harness::Claude && outcome == "lost" {
                notices.push(format!(
                    "`{}` was removed from Claude Code and could not be added again; the original literals were never stored, so set {} in the environment Claude Code starts from and run `d-mcp sync` to re-add it in reference form",
                    server.name,
                    if variables.is_empty() {
                        "its variables".to_owned()
                    } else {
                        variables.iter().map(|var| format!("`{var}`")).collect::<Vec<_>>().join(", ")
                    }
                ));
            } else {
                notices.push(format!(
                    "`{}` was not taken over in {} ({outcome}); {} and its selection stay in place, so fix the cause and run `d-mcp sync` (with `--take-over {}` if it reports a collision)",
                    server.name,
                    harness.as_str(),
                    server.file.display(),
                    server.name
                ));
            }
        }
    }
    adoption.notices.extend(notices);
}
