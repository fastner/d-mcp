//! The shared resolution pipeline: scan, validate, apply the approval and
//! audit gate, resolve. `resolve`, `status`, `inventory`, `doctor`, `adopt`
//! and `sync` all go through it, so they agree on every server.

use crate::adapter;
use std::collections::BTreeSet;

use crate::approval::{self, ApprovalKey, Approvals, Orphan};
use crate::audit::audit;
use crate::config::{Config, Selection};
use crate::error::Result;
use crate::lockfile::{self, LockFile};
use crate::resolver::{Problem, Resolution, SourceSnapshot, resolve_with_detached};
use crate::store::StorePaths;
use crate::validate::validate;

use super::{scan, source_specs};

/// The store files that read-time resolution needs besides `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadInputs {
    /// `lock.toml`: the pins the Git snapshots must be stamped with.
    pub lock: LockFile,
    /// `approvals.toml`: the input of the approval and audit gate.
    pub approvals: Approvals,
}

impl ReadInputs {
    /// Loads `lock.toml` and `approvals.toml`; missing files are empty.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::LockCorrupt`],
    /// [`crate::error::Error::ApprovalsCorrupt`],
    /// [`crate::error::Error::SchemaUnsupported`] and
    /// [`crate::error::Error::Io`].
    pub fn load(paths: &StorePaths) -> Result<Self> {
        Ok(Self {
            lock: lockfile::load(paths)?,
            approvals: approval::load(paths)?,
        })
    }
}

/// Scans every source of the store, Git snapshots included. Never runs
/// `git`.
#[must_use]
pub fn load_snapshots(
    paths: &StorePaths,
    config: &Config,
    inputs: &ReadInputs,
) -> Vec<SourceSnapshot> {
    source_specs(paths, config, &inputs.lock)
        .iter()
        .map(scan)
        .collect()
}

/// Validates scanned snapshots against every adapter, applies the approval
/// and audit gate and resolves `selections`. `adopt` calls this with a
/// candidate injected into the snapshots and its own selections; every other
/// caller uses [`load_resolution`].
#[must_use]
pub fn resolve_snapshots(
    snapshots: &mut [SourceSnapshot],
    config: &Config,
    selections: &[Selection],
    inputs: &ReadInputs,
) -> Resolution {
    prepare_snapshots(snapshots, config, inputs);
    let mut resolution = resolve_with_detached(
        snapshots,
        selections,
        &detached_revoked(config, &inputs.approvals),
    );
    resolution.add_problems(
        orphaned_approvals(config, &inputs.approvals)
            .iter()
            .map(Problem::orphaned_approval),
    );
    resolution
}

/// The revoked markers whose source id is not a registered Git source
/// (removed, or now a directory source). No snapshot carries them, so the
/// gate cannot; the resolver revokes their servers when no source offers
/// the name, so `source remove` never brings a revoked entry back.
fn detached_revoked(config: &Config, approvals: &Approvals) -> BTreeSet<ApprovalKey> {
    approvals
        .revoked_keys()
        .filter(|key| {
            config
                .source(&key.source)
                .and_then(|source| source.git())
                .is_none()
        })
        .cloned()
        .collect()
}

/// The entries of `approvals` (approvals, accepted risks, revoked markers)
/// whose source is not a registered Git source with the recorded URL
/// (removed, re-added with another URL, or now a directory source). An
/// orphaned approval or risk stays inactive; an orphaned revoked marker
/// still blocks its server from a re-added Git source of that id, and,
/// while the id is not a Git source, still revokes the server when no
/// source offers it. Both are reported as `orphaned-approval`.
#[must_use]
pub fn orphaned_approvals(config: &Config, approvals: &Approvals) -> Vec<Orphan> {
    approvals.orphaned(|id| {
        config
            .source(id)
            .and_then(|source| source.git())
            .map(|git| git.url.as_str())
    })
}

/// Validates scanned snapshots against every adapter and applies the
/// approval and audit gate, without resolving. The approval commands use
/// this to find the definition they act on.
pub fn prepare_snapshots(snapshots: &mut [SourceSnapshot], config: &Config, inputs: &ReadInputs) {
    validate(snapshots, adapter::registry());
    apply_gate(snapshots, config, &inputs.approvals);
}

/// The approval and audit gate. Every valid definition is audited; a
/// definition from a Git source of `config` also gets its verdict from
/// `approvals`, bound to the source's current URL. A Git source's snapshot
/// also gets the names it carries a revoked marker for, valid, invalid or
/// not offered, so the resolver can remove what was revoked. Directory
/// sources get findings only (warnings).
fn apply_gate(snapshots: &mut [SourceSnapshot], config: &Config, approvals: &Approvals) {
    for snapshot in snapshots {
        let git = config.source(&snapshot.id).and_then(|source| source.git());
        snapshot.revoked = match git {
            Some(_) => approvals
                .revoked_servers(&snapshot.id)
                .map(str::to_owned)
                .collect(),
            None => std::collections::BTreeSet::new(),
        };
        for file in &mut snapshot.files {
            let Ok(definition) = &file.outcome else {
                continue;
            };
            file.findings = audit(definition);
            file.gate = git.map(|git| {
                approvals.verdict(
                    &ApprovalKey::new(&snapshot.id, &definition.name),
                    &git.url,
                    &definition.definition_hash(),
                    &file.findings,
                )
            });
        }
    }
}

/// Scans, validates, gates and resolves the selections of `config`.
#[must_use]
pub fn load_resolution(paths: &StorePaths, config: &Config, inputs: &ReadInputs) -> Resolution {
    let mut snapshots = load_snapshots(paths, config, inputs);
    resolve_snapshots(&mut snapshots, config, &config.selections, inputs)
}
