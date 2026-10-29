//! The approval of Claude Code project-scope (`.mcp.json`) entries.
//!
//! Verified against Claude Code 2.1.312 (isolated `HOME` and
//! `CLAUDE_CONFIG_DIR`, `claude mcp list` in a temporary project with a
//! `.mcp.json` stdio server): an entry is loaded only when it is approved.
//! The keys are `enabledMcpjsonServers` and `disabledMcpjsonServers` (arrays
//! of names) and `enableAllProjectMcpServers` (a boolean), read from
//!
//! - the user settings, `$CLAUDE_CONFIG_DIR/settings.json` (or
//!   `$HOME/.claude/settings.json`), which always count;
//! - the project's `.claude/settings.json` and `.claude/settings.local.json`,
//!   and the legacy keys of the project entry in `.claude.json`, which count
//!   only once the workspace is trusted (`hasTrustDialogAccepted = true` in
//!   that project entry). Claude Code moves the legacy keys into
//!   `.claude/settings.local.json` on its next run.
//!
//! A disabled name wins over an enabled one; an entry that is neither is
//! "pending approval". The project is Claude Code's working directory, so
//! d-mcp uses its own. Managed settings are not read; they can change the
//! outcome without d-mcp seeing it.
//!
//! Only the three keys are deserialized; every other key of a settings file
//! (which can hold `env` values) is skipped unread.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::file::{Approvals, Flag, NameList, ProjectFlags};
use crate::adapter::{Approval, Trust, read_repository_file};
use crate::store::Environment;

/// The approval keys of one settings file; every other key is skipped.
#[derive(Deserialize, Default)]
struct SettingsFile {
    #[serde(rename = "enabledMcpjsonServers", default)]
    enabled: NameList,
    #[serde(rename = "disabledMcpjsonServers", default)]
    disabled: NameList,
    #[serde(rename = "enableAllProjectMcpServers", default)]
    enable_all: Flag,
}

/// Reads the approval keys of a settings file: `Some(empty)` when it does
/// not exist, `None` when it cannot be read or parsed.
fn read_settings(path: &Path) -> Option<Approvals> {
    let bytes = read_repository_file(path).ok()?;
    let Some(bytes) = bytes else {
        return Some(Approvals::default());
    };
    // serde would also accept a top-level array as the struct.
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return None;
    }
    let file: SettingsFile = serde_json::from_slice(&bytes).ok()?;
    Some(Approvals {
        enabled: file.enabled.0,
        disabled: file.disabled.0,
        enable_all: file.enable_all.0,
    })
}

/// The user settings file; `None` when its location is unknown (an empty
/// `CLAUDE_CONFIG_DIR` makes it relative to Claude Code's working directory,
/// and an unset `HOME` leaves no default).
fn user_settings_file(env: &Environment) -> Option<PathBuf> {
    if env.claude_config_dir_empty() {
        return None;
    }
    match env.claude_config_dir().ok()? {
        Some(dir) => Some(dir.join("settings.json")),
        None => Some(env.home().ok()?.join(".claude").join("settings.json")),
    }
}

/// Every source of approvals for the project at `cwd`, and its trust.
pub(super) struct ProjectApprovals {
    /// The workspace trust of the project.
    pub trust: Trust,
    /// The sources that count, in reading order; `None` for one that could
    /// not be read.
    sources: Vec<Option<Approvals>>,
}

impl ProjectApprovals {
    /// Reads the sources. `flags` is the project's entry in `.claude.json`;
    /// `None` when that file could not be read.
    pub(super) fn read(
        env: &Environment,
        cwd: Option<&Path>,
        flags: Option<Option<&ProjectFlags>>,
    ) -> Self {
        let user = user_settings_file(env).and_then(|path| read_settings(&path));
        let (Some(cwd), Some(flags)) = (cwd, flags) else {
            return Self {
                trust: Trust::UnknownTrust,
                sources: vec![user],
            };
        };
        let flags = flags.cloned().unwrap_or_default();
        let mut sources = vec![user];
        let trust = if flags.trusted {
            sources.push(Some(flags.approvals));
            for file in ["settings.json", "settings.local.json"] {
                sources.push(read_settings(&cwd.join(".claude").join(file)));
            }
            Trust::Trusted
        } else {
            Trust::Untrusted
        };
        Self { trust, sources }
    }

    /// The approval of one `.mcp.json` entry.
    pub(super) fn approval(&self, name: &str) -> Approval {
        if self.trust == Trust::UnknownTrust || self.sources.iter().any(Option::is_none) {
            return Approval::Unknown;
        }
        let sources = || self.sources.iter().flatten();
        if sources().any(|source| source.disabled.contains(name)) {
            Approval::Disabled
        } else if sources().any(|source| source.enable_all || source.enabled.contains(name)) {
            Approval::Approved
        } else {
            Approval::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::fs;

    fn env(root: &Path) -> Environment {
        Environment {
            home: Some(OsString::from(root.join("home"))),
            claude_config_dir: Some(OsString::from(root.join("claude"))),
            cwd: Some(root.join("proj")),
            ..Environment::default()
        }
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn project_settings_count_only_in_a_trusted_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("claude")).unwrap();
        fs::create_dir_all(root.join("proj/.claude")).unwrap();
        let env = env(root);
        let cwd = root.join("proj");
        fs::write(
            root.join("proj/.claude/settings.local.json"),
            r#"{"enabledMcpjsonServers":["a"],"env":{"T":"CANARY"}}"#,
        )
        .unwrap();
        let untrusted = ProjectApprovals::read(&env, Some(&cwd), Some(None));
        assert_eq!(untrusted.trust, Trust::Untrusted);
        assert_eq!(untrusted.approval("a"), Approval::Pending);
        let trusted_flags = ProjectFlags {
            trusted: true,
            approvals: Approvals {
                disabled: names(&["b"]),
                ..Approvals::default()
            },
        };
        let trusted = ProjectApprovals::read(&env, Some(&cwd), Some(Some(&trusted_flags)));
        assert_eq!(trusted.trust, Trust::Trusted);
        assert_eq!(trusted.approval("a"), Approval::Approved);
        assert_eq!(trusted.approval("b"), Approval::Disabled);
        assert_eq!(trusted.approval("c"), Approval::Pending);
        // The user settings count without trust, and disabled wins.
        fs::write(
            root.join("claude/settings.json"),
            r#"{"enableAllProjectMcpServers":true,"disabledMcpjsonServers":["c"]}"#,
        )
        .unwrap();
        let user = ProjectApprovals::read(&env, Some(&cwd), Some(None));
        assert_eq!(user.approval("a"), Approval::Approved);
        assert_eq!(user.approval("c"), Approval::Disabled);
    }

    #[test]
    fn unreadable_sources_make_the_approval_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("claude")).unwrap();
        let env = env(root);
        let cwd = root.join("proj");
        assert_eq!(
            ProjectApprovals::read(&env, None, Some(None)).approval("a"),
            Approval::Unknown
        );
        assert_eq!(
            ProjectApprovals::read(&env, Some(&cwd), None).trust,
            Trust::UnknownTrust
        );
        for broken in ["{", "[]", "{\"enabledMcpjsonServers\":"] {
            fs::write(root.join("claude/settings.json"), broken).unwrap();
            assert_eq!(
                ProjectApprovals::read(&env, Some(&cwd), Some(None)).approval("a"),
                Approval::Unknown,
                "{broken}"
            );
        }
        let mut empty = env.clone();
        empty.claude_config_dir = Some(OsString::new());
        assert_eq!(
            ProjectApprovals::read(&empty, Some(&cwd), Some(None)).approval("a"),
            Approval::Unknown
        );
        let mut default = env;
        default.claude_config_dir = None;
        fs::create_dir_all(root.join("home/.claude")).unwrap();
        fs::write(
            root.join("home/.claude/settings.json"),
            r#"{"enabledMcpjsonServers":["a"]}"#,
        )
        .unwrap();
        assert_eq!(
            ProjectApprovals::read(&default, Some(&cwd), Some(None)).approval("a"),
            Approval::Approved
        );
    }
}
