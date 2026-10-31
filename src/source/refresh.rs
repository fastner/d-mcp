//! `source refresh`: fetch the followed ref of a Git source, detect
//! rewrites, re-materialize the snapshot and move the pin.
//!
//! - The fetch takes exactly the fully qualified ref recorded in
//!   `lock.toml` (`refs/heads/<ref>` or `refs/tags/<ref>`), so a tag pushed
//!   with the name of the followed branch (or the other way round) is never
//!   taken instead. A pin without a recorded ref is qualified first with
//!   `git ls-remote` (a branch wins over a tag, as at `add-git`), and the
//!   refresh records it.
//! - The fetch runs into the store clone outside the store lock. A clone
//!   that is missing or broken (its pinned commit no longer resolves) is
//!   replaced by a fresh clone in `sources/.tmp-<random>` (cloned, then the
//!   followed ref fetched); if the old pin is not reachable in the fresh
//!   clone either, the refresh fails with `source-commit-unreachable`.
//! - **Rewrites:** the new commit must descend from the pinned one
//!   (`git merge-base --is-ancestor`); a followed tag that moved at all
//!   counts as a rewrite too, and so does a source without a pin (nothing
//!   to compare with, so the new commit cannot be checked). A rewrite is
//!   refused without `allow_rewrite` (`source-rewrite`); `refs/d-mcp/pin`
//!   keeps the old commit reachable. A ref that no longer exists on the
//!   remote is `source-ref-deleted`.
//! - Under the lock, the source must still be registered unchanged with
//!   the same pin (`source-changed`, exit 3, otherwise). Then the pin ref is
//!   moved and the new snapshot renamed into place; a replaced clone's
//!   directory is swapped in whole, with the old one parked in
//!   `sources/.tmp-<random>` and restored if the rename fails. Finally
//!   `lock.toml` is written.
//! - A moved pin never approves anything: approvals are bound to the
//!   `definition_hash`, and a changed definition is held as
//!   `pending-approval` until it is approved.
//! - A dry run clones into a fresh directory under `TMPDIR`, which is
//!   deleted afterwards; the store, the existing clone included, stays
//!   byte-identical.

use std::fs;
use std::path::Path;

use crate::config::SourceEntry;
use crate::error::{Error, Result};
use crate::git::{CommitId, Git, GitUrl, validate_url_syntax};
use crate::hashkey;
use crate::lockfile;
use crate::resolver::SourceSnapshot;
use crate::store::{Environment, StoreLock, StorePaths, StoreWarning};

use super::git_source::{REPO_DIR, SNAPSHOT_DIR, Staging, TEMP_PREFIX, git_spec, swap_in, sweep};
use super::{ReadInputs, prepare_snapshots, scan, snapshot};

/// The ref a refresh fetches the followed ref into.
pub const FETCHED_REF: &str = "refs/d-mcp/fetched";

/// The arguments of `source refresh`.
#[derive(Debug, Clone, Copy)]
pub struct RefreshOptions<'a> {
    /// The source id.
    pub id: &'a str,
    /// Accept a rewritten ref (force-push or moved tag) or a missing pin.
    pub allow_rewrite: bool,
    /// Whether this is a dry run.
    pub dry_run: bool,
}

/// What a refresh did, or would do in a dry run.
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    /// The registered source.
    pub entry: SourceEntry,
    /// The pin before the refresh.
    pub previous: Option<CommitId>,
    /// The new pin.
    pub commit: CommitId,
    /// Whether the new commit does not descend from the old one (accepted
    /// with `allow_rewrite`).
    pub rewrite: bool,
    /// Whether the clone was replaced by a fresh one.
    pub recloned: bool,
    /// The old snapshot, validated and gated.
    pub old: SourceSnapshot,
    /// The new snapshot, validated and gated.
    pub new: SourceSnapshot,
    /// Non-fatal store conditions.
    pub warnings: Vec<StoreWarning>,
}

/// Runs `source refresh` on a registered Git source.
///
/// # Errors
///
/// `store-not-initialized`, `source-not-found`, `source-rewrite`,
/// `source-ref-deleted`, `source-commit-unreachable`, `source-changed`,
/// `store-locked`, `config-corrupt`, `lock-corrupt`, `approvals-corrupt`,
/// the git errors and [`Error::Io`].
pub fn refresh(
    paths: &StorePaths,
    env: &Environment,
    options: &RefreshOptions<'_>,
) -> Result<RefreshOutcome> {
    paths.require_initialized()?;
    let id = options.id;
    let config = paths.load_config()?;
    let entry = config
        .source(id)
        .cloned()
        .ok_or_else(|| Error::SourceNotFound { id: id.to_owned() })?;
    let Some(origin) = entry.git().cloned() else {
        return Err(Error::SourceNotFound { id: id.to_owned() });
    };
    let url = validate_url_syntax(&origin.url)?;
    let inputs = ReadInputs::load(paths)?;
    let previous = inputs.lock.pin(id).cloned();
    let recorded = inputs.lock.followed_ref(id).map(str::to_owned);
    let git = Git::locate(env)?;
    let location = paths.git_source(id);
    let mut old = scan(&git_spec(
        id,
        entry.priority,
        &origin,
        location.join(SNAPSHOT_DIR),
        previous.clone(),
    ));

    let staging = Staging::create(paths, options.dry_run, "d-mcp-refresh-")?;
    let store_git_dir = location.join(REPO_DIR).join(".git");
    let healthy = !options.dry_run && clone_healthy(&git, &store_git_dir, previous.as_ref());
    let deleted = |error: Error| match error {
        Error::GitRefNotFound { .. } => Error::SourceRefDeleted {
            id: id.to_owned(),
            reference: origin.reference.clone(),
        },
        other => other,
    };
    let followed = match &recorded {
        Some(followed) => followed.clone(),
        None => qualify(&git, staging.path(), &url, &origin.reference).map_err(deleted)?,
    };
    let (git_dir, commit) = if healthy {
        let commit = fetch_followed(&git, &store_git_dir, &url, &followed).map_err(deleted)?;
        (store_git_dir.clone(), commit)
    } else {
        let repo = staging.path().join(REPO_DIR);
        git.clone_repo(staging.path(), &url, None, &repo)?;
        let git_dir = repo.join(".git");
        let commit = fetch_followed(&git, &git_dir, &url, &followed).map_err(deleted)?;
        (git_dir, commit)
    };
    let recloned = !healthy;

    let rewrite = match &previous {
        // Without a pin there is nothing to check the new commit against.
        None => true,
        Some(previous) if *previous != commit => {
            let reachable =
                git.rev_parse_commit(&git_dir, previous.as_str())?.as_ref() == Some(previous);
            if !reachable && !options.dry_run {
                // A fresh clone that lacks the old pin: it cannot be kept
                // reachable, so the refresh stops here.
                return Err(Error::SourceCommitUnreachable {
                    id: id.to_owned(),
                    reference: origin.reference.clone(),
                    commit: previous.to_string(),
                });
            }
            !reachable || is_tag(&followed) || !git.is_ancestor(&git_dir, previous, &commit)?
        }
        Some(_) => false,
    };
    if rewrite && !options.allow_rewrite {
        return Err(Error::SourceRewrite {
            id: id.to_owned(),
            reference: origin.reference.clone(),
        });
    }
    if recloned {
        git.update_pin(&git_dir, &commit)?;
    }
    let staged_snapshot = staging.path().join(SNAPSHOT_DIR);
    snapshot::materialize(&git, &git_dir, &commit, &staged_snapshot)?;
    let mut new = scan(&git_spec(
        id,
        entry.priority,
        &origin,
        staged_snapshot.clone(),
        Some(commit.clone()),
    ));
    {
        let mut pair = [old, new];
        prepare_snapshots(&mut pair, &config, &inputs);
        [old, new] = pair;
    }

    let mut warnings = Vec::new();
    if !options.dry_run {
        let lock = StoreLock::acquire(paths)?;
        warnings.extend(hashkey::ensure_under_lock(paths, &lock)?);
        let current = paths.load_config()?;
        let mut pins = lockfile::load(paths)?;
        if current.source(id) != Some(&entry) || pins.pin(id) != previous.as_ref() {
            return Err(Error::SourceChanged { id: id.to_owned() });
        }
        sweep(paths, staging.path());
        let up_to_date = !recloned
            && previous.as_ref() == Some(&commit)
            && snapshot::stamp_matches(&location.join(SNAPSHOT_DIR), &commit);
        if !up_to_date {
            if recloned {
                // The fresh clone and its snapshot replace the whole
                // directory; the old one is parked until the swap is done.
                let park = tempfile::Builder::new()
                    .prefix(TEMP_PREFIX)
                    .tempdir_in(&paths.sources)
                    .map_err(|error| Error::io("create directory", &paths.sources, &error))?;
                let staged = staging.keep()?;
                if let Err(error) = swap_in(&location, &staged, park.path()) {
                    let _ = fs::remove_dir_all(&staged);
                    return Err(error);
                }
            } else {
                git.update_pin(&store_git_dir, &commit)?;
                swap_in(
                    &location.join(SNAPSHOT_DIR),
                    &staged_snapshot,
                    staging.path(),
                )?;
            }
        }
        if !up_to_date || pins.followed_ref(id) != Some(followed.as_str()) {
            pins.set_pin(id, commit.clone(), followed);
            warnings.extend(lockfile::save(paths, &lock, &pins)?);
        }
    }
    Ok(RefreshOutcome {
        entry,
        previous,
        commit,
        rewrite,
        recloned,
        old,
        new,
        warnings,
    })
}

/// Whether the store clone exists and resolves its pin (or, without a
/// pin, its `HEAD`). Any git error counts as broken.
fn clone_healthy(git: &Git, git_dir: &Path, pin: Option<&CommitId>) -> bool {
    if !fs::symlink_metadata(git_dir).is_ok_and(|metadata| metadata.is_dir()) {
        return false;
    }
    match pin {
        Some(pin) => {
            matches!(git.rev_parse_commit(git_dir, pin.as_str()), Ok(Some(found)) if found == *pin)
        }
        None => matches!(git.rev_parse_commit(git_dir, "HEAD"), Ok(Some(_))),
    }
}

/// Whether a fully qualified ref is a tag.
fn is_tag(followed: &str) -> bool {
    followed.starts_with("refs/tags/")
}

/// Qualifies the followed ref of a pin that has none recorded:
/// `refs/heads/<ref>` when the remote has that branch, otherwise
/// `refs/tags/<ref>` when it has that tag (the order of
/// `git clone --branch`).
///
/// # Errors
///
/// `git-ref-not-found` when the remote has neither; the git errors.
fn qualify(git: &Git, cwd: &Path, url: &GitUrl, reference: &str) -> Result<String> {
    let refs = git.ls_remote(cwd, url, reference)?;
    [
        format!("refs/heads/{reference}"),
        format!("refs/tags/{reference}"),
    ]
    .into_iter()
    .find(|candidate| refs.iter().any(|(_, name)| name == candidate))
    .ok_or(Error::GitRefNotFound {
        operation: "ls-remote",
    })
}

/// Fetches the fully qualified `followed` ref from `url` into
/// [`FETCHED_REF`] of the repository at `git_dir` and returns its commit.
fn fetch_followed(git: &Git, git_dir: &Path, url: &GitUrl, followed: &str) -> Result<CommitId> {
    let refspec = format!("+{followed}:{FETCHED_REF}");
    git.fetch(
        git_dir.parent().unwrap_or(git_dir),
        git_dir,
        url,
        &[refspec],
    )?;
    git.rev_parse_commit(git_dir, FETCHED_REF)?
        .ok_or(Error::GitRefNotFound { operation: "fetch" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_told_from_branches_by_the_qualified_ref() {
        assert!(is_tag("refs/tags/v1"));
        assert!(!is_tag("refs/heads/v1"));
        assert!(!is_tag("refs/heads/refs/tags/v1"));
    }
}
