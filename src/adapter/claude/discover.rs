//! Discovery of the Claude Code scopes: the user scope of its configuration
//! file, the local scope of every project in it (names and project paths
//! only, ADR 0005), and the project scope of the current repository
//! (`<repository>/.mcp.json`), with each entry's approval (see `settings`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::file::{self, Parsed, entry_hash, invalid};
use super::settings::ProjectApprovals;
use super::{ClaudeAdapter, harness_read};
use crate::adapter::{
    Adapter, DiscoveredEntry, DiscoveredScope, RawEntry, Scope, ScopeRead, path_exists,
    read_repository_file,
};
use crate::config::Harness;
use crate::error::Result;
use crate::store::Environment;

/// The entries of a `.mcp.json` file: its `mcpServers` object.
fn parse_mcp_json(path: &Path, bytes: &[u8]) -> Result<BTreeMap<String, Value>> {
    // serde would report a top-level array or scalar as a type error
    // without a useful position; anything but an object is refused.
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid(path, "not valid JSON", Some((error.line(), error.column()))))?;
    let Value::Object(mut root) = value else {
        return Err(invalid(path, "not a JSON object", None));
    };
    match root.remove("mcpServers") {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(servers)) => Ok(servers.into_iter().collect()),
        Some(_) => Err(invalid(
            path,
            "holding an `mcpServers` that is not an object",
            None,
        )),
    }
}

fn discovered(value: Value) -> DiscoveredEntry {
    DiscoveredEntry {
        hash: Some(entry_hash(&value)),
        supported_style: value.is_object(),
        raw: Some(RawEntry::new(value)),
        preserved: BTreeSet::new(),
        approval: None,
    }
}

/// The local scopes: one per project path with local-scope entries.
fn local_scopes(file: &Path, parsed: &Parsed) -> Vec<ScopeRead> {
    let mut by_project: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (name, projects) in &parsed.local {
        for project in projects {
            by_project.entry(project).or_default().insert(name);
        }
    }
    by_project
        .into_iter()
        .map(|(project, names)| {
            let scope = DiscoveredScope::Local {
                project: PathBuf::from(project),
            };
            let mut read = ScopeRead::new(Harness::Claude, scope, file.to_path_buf());
            read.present = true;
            read.orphaned = std::fs::symlink_metadata(project)
                .is_err_and(|error| crate::adapter::is_absent(&error));
            read.entries = names
                .into_iter()
                .map(|name| (name.to_owned(), DiscoveredEntry::name_only()))
                .collect();
            read
        })
        .collect()
}

/// The project scope: `<repository>/.mcp.json`, when it exists. A file
/// whose existence cannot be determined (an unreadable directory) is that
/// scope's error.
fn project_scope(
    env: &Environment,
    repo_root: &Path,
    parsed: Option<&Parsed>,
) -> Option<ScopeRead> {
    let path = repo_root.join(".mcp.json");
    let scope = DiscoveredScope::Project {
        root: repo_root.to_path_buf(),
    };
    let mut read = ScopeRead::new(Harness::Claude, scope, path.clone());
    match path_exists(&path) {
        Ok(true) => {}
        Ok(false) => return None,
        Err(error) => {
            read.present = true;
            return Some(read.failed(error));
        }
    }
    read.present = true;
    let cwd = env.cwd.as_deref();
    let flags =
        parsed.map(|parsed| cwd.and_then(|cwd| parsed.flags.get(cwd.to_string_lossy().as_ref())));
    let approvals = ProjectApprovals::read(env, cwd, flags);
    read.trust = Some(approvals.trust);
    let entries = read_repository_file(&path)
        .and_then(|bytes| parse_mcp_json(&path, &bytes.unwrap_or_default()));
    match entries {
        Ok(entries) => {
            read.entries = entries
                .into_iter()
                .map(|(name, value)| {
                    let mut entry = discovered(value);
                    entry.approval = Some(approvals.approval(&name));
                    (name, entry)
                })
                .collect();
            Some(read)
        }
        Err(error) => Some(read.failed(error)),
    }
}

/// Discovers the user, local and project scopes.
pub(super) fn discover(
    adapter: &ClaudeAdapter,
    env: &Environment,
    repo_root: &Path,
) -> Vec<ScopeRead> {
    let mut user = ScopeRead::new(Harness::Claude, DiscoveredScope::User, PathBuf::new());
    let mut parsed = None;
    match adapter.locate(env, Scope::User) {
        Err(error) => user = user.failed(error),
        Ok(location) => {
            user.file.clone_from(&location.file);
            user.present = location.present();
            match file::read(&location.file) {
                Ok(read) => {
                    user.read = Some(harness_read(&location, &read));
                    user.entries = read
                        .entries
                        .iter()
                        .map(|(name, value)| (name.clone(), discovered(value.clone())))
                        .collect();
                    if !read.root_supported {
                        user.error = Some(invalid(
                            &location.file,
                            "holding an `mcpServers` that is not an object",
                            None,
                        ));
                    }
                    parsed = Some(read);
                }
                Err(error) => user = user.failed(error),
            }
        }
    }
    let locals = parsed
        .as_ref()
        .map(|parsed| local_scopes(&user.file, parsed))
        .unwrap_or_default();
    let project = project_scope(env, repo_root, parsed.as_ref());
    let mut reads = vec![user];
    reads.extend(locals);
    reads.extend(project);
    reads
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Approval, Trust};
    use std::ffi::OsString;
    use std::fs;

    #[test]
    fn mcp_json_must_hold_an_object_of_servers() {
        let path = Path::new("/r/.mcp.json");
        assert_eq!(
            parse_mcp_json(path, br#"{"mcpServers":{"a":{"command":"x"}}}"#)
                .unwrap()
                .len(),
            1
        );
        assert!(parse_mcp_json(path, b"{}").unwrap().is_empty());
        for (bytes, reason) in [
            (&b"[]"[..], "is not a JSON object"),
            (b"{\"mcpServers\":[]}", "holding an `mcpServers`"),
            (
                b"{\"mcpServers\": CANARY}",
                "not valid JSON at line 1, column 16",
            ),
        ] {
            let error = parse_mcp_json(path, bytes).unwrap_err();
            assert!(error.hint().contains(reason), "{}", error.hint());
            assert!(!error.hint().contains("CANARY"));
        }
    }

    #[test]
    fn every_scope_is_discovered_on_its_own() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(root.join("claude")).unwrap();
        let config = serde_json::json!({
            "mcpServers": {"u": {"command": "x"}},
            "projects": {
                repo.to_string_lossy(): {"hasTrustDialogAccepted": true, "mcpServers": {"l": {"env": {"T": "CANARY"}}},
                                         "enabledMcpjsonServers": ["p"]},
                "/gone/project": {"mcpServers": {"l": {}, "m": {}}}
            }
        });
        fs::write(root.join("claude/.claude.json"), config.to_string()).unwrap();
        fs::write(
            repo.join(".mcp.json"),
            r#"{"mcpServers":{"p":{"command":"y"},"q":{"command":"z"}}}"#,
        )
        .unwrap();
        let env = Environment {
            home: Some(OsString::from(root.join("home"))),
            claude_config_dir: Some(OsString::from(root.join("claude"))),
            cwd: Some(repo.clone()),
            ..Environment::default()
        };
        let reads = ClaudeAdapter.discover(&env, &repo);
        let scopes: Vec<&str> = reads.iter().map(|read| read.scope.as_str()).collect();
        assert_eq!(scopes, ["user", "local", "local", "project"]);
        assert!(reads[0].read.is_some());
        assert!(reads[0].entries["u"].raw.is_some());
        assert_eq!(
            reads[1].scope,
            DiscoveredScope::Local {
                project: PathBuf::from("/gone/project")
            }
        );
        assert!(reads[1].orphaned);
        assert_eq!(reads[1].entries.len(), 2);
        assert!(!reads[2].orphaned);
        assert!(reads[2].entries["l"].raw.is_none());
        assert_eq!(reads[3].trust, Some(Trust::Trusted));
        assert_eq!(reads[3].entries["p"].approval, Some(Approval::Approved));
        assert_eq!(reads[3].entries["q"].approval, Some(Approval::Pending));
        assert!(!format!("{reads:?}").contains("CANARY"));
        // A broken project file is that scope's error only.
        fs::write(repo.join(".mcp.json"), "{").unwrap();
        let reads = ClaudeAdapter.discover(&env, &repo);
        assert!(reads[0].error.is_none());
        assert!(reads[3].error.is_some());
        // A broken configuration file leaves the trust unknown.
        fs::write(root.join("claude/.claude.json"), "{").unwrap();
        fs::write(repo.join(".mcp.json"), r#"{"mcpServers":{"p":{}}}"#).unwrap();
        let reads = ClaudeAdapter.discover(&env, &repo);
        assert_eq!(reads.len(), 2);
        assert!(reads[0].error.is_some());
        assert_eq!(reads[1].trust, Some(Trust::UnknownTrust));
        assert_eq!(reads[1].entries["p"].approval, Some(Approval::Unknown));
    }

    /// Only a path that does not exist is absent: a directory that cannot be
    /// searched makes its files unknown, so the settings make the approval
    /// `unknown` and an `.mcp.json` the project scope's error.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_not_absence() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir_all(repo.join(".claude")).unwrap();
        fs::create_dir_all(root.join("claude")).unwrap();
        let config = serde_json::json!({
            "projects": {repo.to_string_lossy(): {"hasTrustDialogAccepted": true}}
        });
        fs::write(root.join("claude/.claude.json"), config.to_string()).unwrap();
        fs::write(
            repo.join(".mcp.json"),
            r#"{"mcpServers":{"p":{"command":"y"}}}"#,
        )
        .unwrap();
        let env = Environment {
            home: Some(OsString::from(root.join("home"))),
            claude_config_dir: Some(OsString::from(root.join("claude"))),
            cwd: Some(repo.clone()),
            ..Environment::default()
        };
        let mode = |dir: &Path, mode: u32| {
            fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
        };
        let settings = repo.join(".claude");
        mode(&settings, 0o000);
        if fs::read_dir(&settings).is_ok() {
            // Running as root: permissions do not apply.
            mode(&settings, 0o755);
            return;
        }
        let reads = ClaudeAdapter.discover(&env, &repo);
        mode(&settings, 0o755);
        let project = reads.last().unwrap();
        assert_eq!(project.scope.as_str(), "project");
        assert!(project.error.is_none());
        assert_eq!(project.entries["p"].approval, Some(Approval::Unknown));
        mode(&repo, 0o600);
        let reads = ClaudeAdapter.discover(&env, &repo);
        mode(&repo, 0o755);
        let project = reads.last().unwrap();
        assert_eq!(project.scope.as_str(), "project");
        assert!(project.present);
        assert_eq!(
            project.error.as_ref().map(crate::error::Error::code),
            Some("io-error")
        );
        assert!(project.entries.is_empty());
    }
}
