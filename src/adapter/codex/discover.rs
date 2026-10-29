//! Discovery of the Codex scopes: the user `config.toml` and the project
//! layers of the current repository.
//!
//! Verified against Codex 0.165.0 (isolated `HOME` and `CODEX_HOME`, `codex
//! mcp list --json`): Codex loads `.codex/config.toml` of every directory from
//! the project root (the nearest ancestor with `.git`, or the working
//! directory) down to its working directory, but only while the user
//! configuration trusts the root: `[projects."<root>"] trust_level =
//! "trusted"`. Without the key, or with `"untrusted"`, it ignores them. A
//! project entry replaces a user entry of the same name, and one unparsable
//! project file makes Codex fail as a whole. d-mcp reads each layer as its
//! own scope, with the root's trust.

use std::path::{Path, PathBuf};

use super::{CodexAdapter, document, invalid};
use crate::adapter::{
    Adapter, DiscoveredEntry, DiscoveredScope, Scope, ScopeRead, Trust, read_repository_file,
};
use crate::config::Harness;
use crate::error::Error;
use crate::store::Environment;

/// The trust of a project root from the user configuration's text; `None`
/// when the user configuration could not be read.
fn trust_of(user_text: Option<&str>, root: &Path) -> Trust {
    let (Some(text), Some(root)) = (user_text, root.to_str()) else {
        return Trust::UnknownTrust;
    };
    match document::trust_level(text, root).as_deref() {
        Some("trusted") => Trust::Trusted,
        // An absent key leaves the project untrusted.
        Some("untrusted") | None => Trust::Untrusted,
        Some(_) => Trust::UnknownTrust,
    }
}

/// The directories whose `.codex/config.toml` Codex loads, root first: from
/// the repository root down to the working directory.
fn layers(repo_root: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match cwd {
        Some(cwd) if cwd.starts_with(repo_root) => cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(repo_root))
            .map(Path::to_path_buf)
            .collect(),
        _ => vec![repo_root.to_path_buf()],
    };
    dirs.reverse();
    dirs
}

/// Reads one project layer.
fn read_layer(file: &Path, repo_root: &Path, trust: Trust) -> Option<ScopeRead> {
    let scope = DiscoveredScope::Project {
        root: repo_root.to_path_buf(),
    };
    let mut read = ScopeRead::new(Harness::Codex, scope, file.to_path_buf());
    read.trust = Some(trust);
    let bytes = match read_repository_file(file) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(error) => {
            read.present = true;
            return Some(read.failed(error));
        }
    };
    read.present = true;
    let Ok(text) = String::from_utf8(bytes) else {
        return Some(read.failed(invalid(file, "not UTF-8", None)));
    };
    let parsed = match document::parse(&text) {
        Ok(parsed) => parsed,
        Err(position) => {
            return Some(read.failed(invalid(file, "not valid TOML", Some(position))));
        }
    };
    if !parsed.root_supported {
        return Some(read.failed(invalid(
            file,
            "using an inline or dotted `mcp_servers` root",
            None,
        )));
    }
    read.entries = parsed
        .entries
        .into_iter()
        .map(|(name, entry)| (name, discovered(entry)))
        .collect();
    Some(read)
}

fn discovered(entry: document::ParsedEntry) -> DiscoveredEntry {
    DiscoveredEntry {
        supported_style: entry.region.is_some(),
        hash: Some(entry.hash),
        raw: Some(entry.value),
        preserved: entry.preserved,
        approval: None,
    }
}

/// Discovers the user scope and the project layers.
pub(super) fn discover(
    adapter: &CodexAdapter,
    env: &Environment,
    repo_root: &Path,
) -> Vec<ScopeRead> {
    let mut user = ScopeRead::new(Harness::Codex, DiscoveredScope::User, PathBuf::new());
    let mut user_text: Option<String> = None;
    let location = adapter.locate(env, Scope::User);
    match location {
        Err(error) => user = user.failed(error),
        Ok(location) => {
            user.file.clone_from(&location.file);
            user.present = location.present();
            match CodexAdapter::read_document(&location) {
                Ok((read, entries)) => {
                    user_text = Some(String::from_utf8_lossy(&read.private.bytes).into_owned());
                    user.entries = entries
                        .into_iter()
                        .map(|(name, entry)| (name, discovered(entry)))
                        .collect();
                    if !read.root_supported {
                        user.error = Some(Error::HarnessConfigInvalid {
                            path: location.file.clone(),
                            reason: "using an inline or dotted `mcp_servers` root",
                            position: None,
                        });
                    }
                    user.read = Some(read);
                }
                Err(error) => user = user.failed(error),
            }
        }
    }
    let trust = trust_of(user_text.as_deref(), repo_root);
    let user_file = user.file.clone();
    let mut reads = vec![user];
    for dir in layers(repo_root, env.cwd.as_deref()) {
        let file = dir.join(".codex").join("config.toml");
        // A repository at `$HOME` holds the user configuration itself.
        if file == user_file {
            continue;
        }
        reads.extend(read_layer(&file, repo_root, trust));
    }
    reads
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;

    #[test]
    fn trust_follows_the_user_configuration() {
        let root = Path::new("/r");
        let text = "[projects.\"/r\"]\ntrust_level = \"trusted\"\n";
        assert_eq!(trust_of(Some(text), root), Trust::Trusted);
        assert_eq!(trust_of(Some(""), root), Trust::Untrusted);
        assert_eq!(
            trust_of(
                Some("[projects.\"/r\"]\ntrust_level = \"untrusted\"\n"),
                root
            ),
            Trust::Untrusted
        );
        assert_eq!(
            trust_of(Some("[projects.\"/r\"]\ntrust_level = \"maybe\"\n"), root),
            Trust::UnknownTrust
        );
        assert_eq!(trust_of(None, root), Trust::UnknownTrust);
    }

    #[test]
    fn layers_run_from_the_root_to_the_working_directory() {
        let root = Path::new("/r");
        assert_eq!(
            layers(root, Some(Path::new("/r/a/b"))),
            [
                PathBuf::from("/r"),
                PathBuf::from("/r/a"),
                PathBuf::from("/r/a/b")
            ]
        );
        assert_eq!(layers(root, Some(Path::new("/elsewhere"))), [root]);
        assert_eq!(layers(root, None), [root]);
    }

    #[test]
    fn every_layer_is_its_own_scope_and_errors_stay_local() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let sub = repo.join("sub");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::create_dir_all(sub.join(".codex")).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        fs::create_dir_all(root.join("codex")).unwrap();
        fs::write(
            root.join("codex/config.toml"),
            format!(
                "[mcp_servers.u]\ncommand = \"x\"\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                repo.display()
            ),
        )
        .unwrap();
        fs::write(
            repo.join(".codex/config.toml"),
            "[mcp_servers.p]\ncommand = \"y\"\n",
        )
        .unwrap();
        fs::write(sub.join(".codex/config.toml"), "broken = \"CANARY\n").unwrap();
        let env = Environment {
            home: Some(OsString::from(root.join("home"))),
            codex_home: Some(OsString::from(root.join("codex"))),
            cwd: Some(sub.clone()),
            ..Environment::default()
        };
        let reads = CodexAdapter.discover(&env, &repo);
        assert_eq!(reads.len(), 3);
        assert_eq!(reads[0].scope, DiscoveredScope::User);
        assert!(reads[0].entries.contains_key("u"));
        assert!(reads[0].read.is_some());
        assert_eq!(reads[1].trust, Some(Trust::Trusted));
        assert!(reads[1].entries.contains_key("p"));
        assert_eq!(reads[2].file, sub.join(".codex/config.toml"));
        let error = reads[2].error.as_ref().unwrap();
        assert_eq!(error.code(), "harness-config-invalid");
        assert!(!error.hint().contains("CANARY"));
        // An unreadable user configuration leaves the trust unknown.
        fs::write(root.join("codex/config.toml"), "x = [").unwrap();
        let reads = CodexAdapter.discover(&env, &repo);
        assert!(reads[0].error.is_some());
        assert_eq!(reads[1].trust, Some(Trust::UnknownTrust));
    }
}
