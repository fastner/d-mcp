//! The checks of the Git sources and their store files: `lock`,
//! `approvals`, `git` and `pending-approval`. Like every doctor check they
//! run nothing: `git` is only looked up on `PATH`, and the approval state
//! comes from the shared resolution.

use crate::approval::{self, OrphanKind};
use crate::config::Config;
use crate::git::Git;
use crate::lockfile;
use crate::resolver::{HoldReason, Resolution, Status, orphan_message};
use crate::source::{ReadInputs, orphaned_approvals};
use crate::store::{Environment, StorePaths};

use super::{Check, CheckId};

/// The `lock` and `approvals` checks. Returns the loaded files, or `None`
/// when one of them is corrupt; the checks that need the store are then
/// left out, as for a corrupt `config.toml`. Orphaned approvals need
/// `config`; without it only the file is checked.
pub(super) fn file_checks(
    paths: &StorePaths,
    config: Option<&Config>,
    checks: &mut Vec<Check>,
) -> Option<ReadInputs> {
    let lock = match lockfile::load(paths) {
        Ok(lock) => {
            let pins = lock.pins().count();
            checks.push(Check::ok(
                CheckId::Lock,
                if pins == 0 {
                    "lock.toml holds no pin (no Git source is pinned)".to_owned()
                } else {
                    format!(
                        "lock.toml is valid ({pins} pin{})",
                        if pins == 1 { "" } else { "s" }
                    )
                },
            ));
            Some(lock)
        }
        Err(error) => {
            checks.push(Check::error(CheckId::Lock, &error).hint(
                "fix lock.toml by hand (d-mcp never rewrites a corrupt file); the checks that need the store are skipped",
            ));
            None
        }
    };
    let approvals = match approval::load(paths) {
        Ok(approvals) => {
            let orphaned = config
                .map(|config| orphaned_approvals(config, &approvals))
                .unwrap_or_default();
            if config.is_none() {
                // Without `config.toml` the sources are unknown, so the
                // orphan check cannot run.
                checks.push(Check::ok(
                    CheckId::Approvals,
                    "approvals.toml is valid; whether its entries belong to registered Git sources is not checked, because config.toml cannot be read",
                ));
            } else if orphaned.is_empty() {
                checks.push(Check::ok(
                    CheckId::Approvals,
                    "approvals.toml is valid; every approval, accepted risk and revoked marker belongs to a registered Git source with its recorded URL",
                ));
            }
            for orphan in orphaned {
                let hint = match orphan.kind {
                    OrphanKind::Approval => {
                        "it applies to nothing; re-adding the source with the same URL makes it apply again, another URL never does; delete its entries from approvals.toml by hand to drop it"
                    }
                    OrphanKind::Revoked => {
                        "it still blocks the server from that source id, whatever its URL (a revoke is never undone by re-adding the source), and while the id is not a registered Git source it revokes the server when no source offers it; approve the server from the source to clear it, or delete the marker from approvals.toml by hand"
                    }
                };
                checks.push(
                    Check::warn(CheckId::Approvals, orphan_message(&orphan))
                        .code("orphaned-approval")
                        .hint(hint)
                        .server(&orphan.key.server),
                );
            }
            Some(approvals)
        }
        Err(error) => {
            checks.push(Check::error(CheckId::Approvals, &error).hint(
                "fix approvals.toml by hand (d-mcp never rewrites a corrupt file); the checks that need the store are skipped",
            ));
            None
        }
    };
    Some(ReadInputs {
        lock: lock?,
        approvals: approvals?,
    })
}

/// The `git` check: `warn` when a Git source is registered and `git` is not
/// on `PATH`. Nothing is run.
pub(super) fn git_check(config: &Config, env: &Environment) -> Check {
    let ids: Vec<String> = config
        .sources
        .iter()
        .filter(|source| source.git().is_some())
        .map(|source| format!("`{}`", source.id))
        .collect();
    if ids.is_empty() {
        return Check::ok(
            CheckId::Git,
            "no Git source is registered, so `git` is not needed",
        );
    }
    let ids = ids.join(", ");
    match Git::locate(env) {
        Ok(_) => Check::ok(
            CheckId::Git,
            format!("`git` is on PATH for the Git sources {ids}; read-only commands never run it"),
        ),
        Err(error) => {
            let mut check = Check::warn(
                CheckId::Git,
                format!("`git` is not on PATH, but the store has the Git sources {ids}"),
            )
            .hint("resolve, status, sync and the other read-only work run from the snapshots without it; `source add-git` and `source refresh` need it, so install git or put it on PATH");
            check.code = Some(error.code());
            check
        }
    }
}

/// The `pending-approval` check: one `warn` per selected server held for
/// approval (`pending-approval` or `risk-not-accepted`).
pub(super) fn pending_checks(resolution: &Resolution) -> Vec<Check> {
    resolution
        .servers
        .iter()
        .filter(|server| server.status == Status::Hold)
        .filter_map(|server| {
            let name = &server.name;
            let source = server.source.as_deref().unwrap_or_default();
            let check = match server.reason? {
                HoldReason::PendingApproval => Check::warn(
                    CheckId::PendingApproval,
                    format!("`{name}` is held: its definition in Git source `{source}` is not approved"),
                )
                .hint(format!(
                    "review it with `d-mcp approve {name} --dry-run`, then approve it with `--expect <definition_hash>`; until then sync keeps an existing entry unchanged"
                )),
                HoldReason::RiskNotAccepted => Check::warn(
                    CheckId::PendingApproval,
                    format!("`{name}` is held: the audit findings of its definition in Git source `{source}` are not accepted"),
                )
                .hint(format!(
                    "review them with `d-mcp accept-risk {name} --reason <text> --dry-run`, then run it with `--expect <definition_hash>`; until then sync keeps an existing entry unchanged"
                )),
                HoldReason::InvalidWinner | HoldReason::SourceUnavailable => return None,
            };
            Some(check.server(name))
        })
        .collect()
}
