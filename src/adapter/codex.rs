//! The Codex adapter: direct, format-preserving edits of
//! `$CODEX_HOME/config.toml` at user scope.
//!
//! Rendering (see `docs/reference.md`, "Codex mapping"):
//!
//! - stdio: `command`, `args` (omitted when empty); forwards go into
//!   `env_vars`, non-secret literals into the `env` sub-table; `cwd` comes
//!   from `harness.codex`.
//! - http: `url`; `bearer_env` becomes `bearer_token_env_var`, `header_env`
//!   becomes `env_http_headers`; OAuth's `client_id` goes to
//!   `oauth.client_id`, `scopes` to `scopes`, and `auth` is omitted (Codex's
//!   default).
//! - `oauth.client_secret` is a harness-local preserved key: Codex has no
//!   environment form for it, so d-mcp never writes, reads, hashes or shows
//!   it and keeps it on Update. A missing one is a manual step (`note` gap).
//!   `oauth.callback_url` and `oauth.callback_port` are preserved the same
//!   way. An Update keeps a preserved key only while it fits the new
//!   rendering (`document::preserved_key_fits`); otherwise it is removed and
//!   reported as a `note` gap naming the key.
//! - The allowed `harness.codex` keys are copied in, except default values
//!   (`enabled = true`, `required = false`, `disabled_tools = []`): Codex
//!   drops them when it re-saves its file, so d-mcp never writes them.

mod document;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::{
    ActualEntry, Adapter, ApplyMode, ApplyResult, EntryResult, Gap, GapSeverity, HarnessPlan,
    HarnessRead, Hooks, Intent, Journal, Location, LoginState, OverrideKey, OverrideType,
    PrivateRead, Rendered, Scope, WriteKind,
};
use crate::config::Harness;
use crate::definition::{Auth, Definition, EnvValue, Transport};
use crate::error::{Error, Result};
use crate::process::{Invocation, ProcessRunner, find_program};
use crate::store::{self, Environment, StoreWarning};

pub use document::{
    ENTRY_HASH_TAG, PRESERVED_CLIENT_SECRET, PRESERVED_KEYS, canonical_entry, entry_hash,
};

/// The Codex versions this adapter was verified against (Codex verification
/// gate of WP2).
pub const VERIFIED_VERSIONS: &[&str] = &["codex-cli 0.165.0"];

/// The allowed `harness.codex` override keys, in writing order.
///
/// `cwd` is stdio-only: Codex refuses to load a configuration with `cwd` on
/// a streamable HTTP entry. `enabled_tools = []` is rejected because Codex
/// drops an empty `enabled_tools` when it re-saves its file, which turns
/// "allow no tool" into "allow every tool".
pub const OVERRIDE_KEYS: &[OverrideKey] = &[
    OverrideKey {
        key: "cwd",
        kind: OverrideType::AbsolutePath,
        stdio_only: true,
    },
    OverrideKey {
        key: "enabled",
        kind: OverrideType::Boolean,
        stdio_only: false,
    },
    OverrideKey {
        key: "required",
        kind: OverrideType::Boolean,
        stdio_only: false,
    },
    OverrideKey {
        key: "startup_timeout_sec",
        kind: OverrideType::Seconds,
        stdio_only: false,
    },
    OverrideKey {
        key: "tool_timeout_sec",
        kind: OverrideType::Seconds,
        stdio_only: false,
    },
    OverrideKey {
        key: "enabled_tools",
        kind: OverrideType::NonEmptyStringArray,
        stdio_only: false,
    },
    OverrideKey {
        key: "disabled_tools",
        kind: OverrideType::StringArray,
        stdio_only: false,
    },
];

/// Codex keys deliberately excluded from the override allow-list in v1 for
/// security reasons: tool approval modes and `http_headers_helper` (it runs a
/// local command), and `experimental_environment`.
pub const EXCLUDED_OVERRIDE_KEYS: &[&str] = &[
    "default_tools_approval_mode",
    "tools",
    "http_headers_helper",
    "experimental_environment",
];

/// The Codex adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

/// Why a harness file cannot be read as a regular file.
enum ReadProblem {
    NotRegular,
    Io(std::io::Error),
}

/// The permission bits and the owner of a file.
#[derive(Debug, Clone, Copy, Default)]
struct FileMeta {
    mode: Option<u32>,
    owner: Option<(u32, u32)>,
}

/// Reads a file without following a final symlink and without blocking on a
/// FIFO; it must be a regular file. Returns the bytes, the mode and the
/// owner.
fn read_regular(path: &Path) -> std::result::Result<(Vec<u8>, FileMeta), ReadProblem> {
    let mut options = OpenOptions::new();
    options.read(true);
    store::no_follow(&mut options, true);
    let mut file = options.open(path).map_err(|error| {
        if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
            ReadProblem::NotRegular
        } else {
            ReadProblem::Io(error)
        }
    })?;
    let metadata = file.metadata().map_err(ReadProblem::Io)?;
    if !metadata.is_file() {
        return Err(ReadProblem::NotRegular);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(ReadProblem::Io)?;
    Ok((
        bytes,
        FileMeta {
            mode: mode_of(&metadata),
            owner: owner_of(&metadata),
        },
    ))
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn owner_of(metadata: &fs::Metadata) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.uid(), metadata.gid()))
}

#[cfg(not(unix))]
fn owner_of(_metadata: &fs::Metadata) -> Option<(u32, u32)> {
    None
}

/// Whether an atomic replace would change the file's owner or group: the
/// temporary file, created by this process, has the process's owner and
/// group (std has no `getuid`, so the temp file stands in for it).
fn owner_changes(original: Option<(u32, u32)>, temp: &Path) -> bool {
    let Some(original) = original else {
        return false;
    };
    fs::metadata(temp)
        .ok()
        .and_then(|metadata| owner_of(&metadata))
        .is_some_and(|current| current != original)
}

fn invalid(path: &Path, reason: &'static str, position: Option<(usize, usize)>) -> Error {
    Error::HarnessConfigInvalid {
        path: path.to_path_buf(),
        reason,
        position,
    }
}

/// Reads and parses the target of the config file.
fn read_target(file: &Path, target: &Path) -> Result<(String, FileMeta, document::Parsed)> {
    let (bytes, meta) = read_regular(target).map_err(|problem| match problem {
        ReadProblem::NotRegular => invalid(file, "not a regular file", None),
        ReadProblem::Io(error) => Error::io("read", file, &error),
    })?;
    let text = String::from_utf8(bytes).map_err(|_| invalid(file, "not UTF-8", None))?;
    let parsed = document::parse(&text)
        .map_err(|position| invalid(file, "not valid TOML", Some(position)))?;
    Ok((text, meta, parsed))
}

/// Whether a spliced document keeps everything the plan does not touch:
/// the root stays editable, every entry outside the plan keeps its hash,
/// and every planned entry is present exactly unless it is removed. A
/// failure means the splice itself is wrong, so nothing may be written.
fn splice_keeps_the_rest(
    original: &document::Parsed,
    updated: &document::Parsed,
    plan: &HarnessPlan,
) -> bool {
    let planned = |name: &str| plan.ops.iter().find(|op| op.name == name);
    let untouched = original
        .entries
        .iter()
        .filter(|(name, _)| planned(name).is_none())
        .all(|(name, entry)| {
            updated
                .entries
                .get(name)
                .is_some_and(|after| after.hash == entry.hash)
        });
    let no_strangers = updated.entries.keys().all(|name| {
        original.entries.contains_key(name)
            || planned(name).is_some_and(|op| op.kind == WriteKind::Add)
    });
    let ops = plan
        .ops
        .iter()
        .all(|op| (op.kind == WriteKind::Remove) != updated.entries.contains_key(&op.name));
    updated.root_supported && untouched && no_strangers && ops
}

/// Maps a `codex mcp list --json` `auth_status` value.
fn login_state_of(status: &str) -> LoginState {
    match status {
        "o_auth" | "oauth" | "logged_in" => LoginState::LoggedIn,
        "not_logged_in" => LoginState::NeedsLogin,
        "unsupported" | "bearer_token" => LoginState::NotApplicable,
        _ => LoginState::Unknown,
    }
}

/// Parses `codex mcp list --json`: an array of `{name, auth_status, ...}`.
fn parse_listing(stdout: &[u8]) -> Option<BTreeMap<String, LoginState>> {
    let listing: Vec<Value> = serde_json::from_slice(stdout).ok()?;
    Some(
        listing
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                let status = item
                    .get("auth_status")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Some((name.to_owned(), login_state_of(status)))
            })
            .collect(),
    )
}

fn note(field: &str, reason: String, unless_present: Option<&'static str>) -> Gap {
    Gap {
        field: field.to_owned(),
        severity: GapSeverity::Note,
        reason,
        unless_present,
    }
}

impl CodexAdapter {
    fn render_entry(definition: &Definition) -> (Vec<(String, Value)>, Vec<Gap>) {
        let mut entry: Vec<(String, Value)> = Vec::new();
        let mut tables: Vec<(String, Value)> = Vec::new();
        let mut gaps = Vec::new();
        match &definition.transport {
            Transport::Stdio { command, args, env } => {
                entry.push(("command".to_owned(), json!(command)));
                if !args.is_empty() {
                    entry.push(("args".to_owned(), json!(args)));
                }
                let forwards: Vec<&String> = env
                    .iter()
                    .filter(|(_, value)| matches!(value, EnvValue::Forward))
                    .map(|(name, _)| name)
                    .collect();
                if !forwards.is_empty() {
                    entry.push(("env_vars".to_owned(), json!(forwards)));
                }
                let literals: Map<String, Value> = env
                    .iter()
                    .filter_map(|(name, value)| match value {
                        EnvValue::Literal(literal) => Some((name.clone(), json!(literal))),
                        EnvValue::Forward => None,
                    })
                    .collect();
                if !literals.is_empty() {
                    tables.push(("env".to_owned(), Value::Object(literals)));
                }
            }
            Transport::Http { url, auth } => {
                entry.push(("url".to_owned(), json!(url)));
                match auth {
                    Auth::OAuth {
                        client_id,
                        scopes,
                        client_secret_env,
                    } => {
                        if !scopes.is_empty() {
                            entry.push(("scopes".to_owned(), json!(scopes)));
                        }
                        if let Some(client_id) = client_id {
                            tables.push(("oauth".to_owned(), json!({ "client_id": client_id })));
                        }
                        if client_secret_env.is_some() {
                            gaps.push(note(
                                "auth.client_secret_env",
                                format!(
                                    "manual step: Codex has no environment form for the client \
                                     secret; enter it once as `oauth.client_secret` in the \
                                     `mcp_servers.{}` table",
                                    definition.name
                                ),
                                Some(PRESERVED_CLIENT_SECRET),
                            ));
                        }
                    }
                    Auth::BearerEnv {
                        var,
                        oauth_supported,
                    } => {
                        entry.push(("bearer_token_env_var".to_owned(), json!(var)));
                        if *oauth_supported {
                            gaps.push(note(
                                "auth.oauth_supported",
                                "Codex ignores `oauth_supported`; it sends the bearer token"
                                    .to_owned(),
                                None,
                            ));
                        }
                    }
                    Auth::HeaderEnv { header, var } => {
                        let mut headers = Map::new();
                        headers.insert(header.clone(), json!(var));
                        tables.push(("env_http_headers".to_owned(), Value::Object(headers)));
                    }
                    Auth::None => gaps.push(note(
                        "auth",
                        "Codex has no explicit no-auth setting; it falls back to OAuth discovery"
                            .to_owned(),
                        None,
                    )),
                }
            }
        }
        if let Some(overrides) = definition.overrides.get(Harness::Codex.as_str()) {
            for key in OVERRIDE_KEYS {
                if let Some(value) = overrides.get(key.key)
                    && !document::is_default(key.key, value)
                {
                    entry.push((key.key.to_owned(), value.clone()));
                }
            }
        }
        entry.extend(tables);
        (entry, gaps)
    }
}

impl Adapter for CodexAdapter {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn apply_mode(&self) -> ApplyMode {
        ApplyMode::DirectEdit
    }

    fn discover_scopes(&self) -> &'static [Scope] {
        &[Scope::User]
    }

    fn write_scopes(&self) -> &'static [Scope] {
        &[Scope::User]
    }

    fn hashes_comparable(&self) -> bool {
        true
    }

    fn override_keys(&self) -> &'static [OverrideKey] {
        OVERRIDE_KEYS
    }

    fn excluded_override_keys(&self) -> &'static [&'static str] {
        EXCLUDED_OVERRIDE_KEYS
    }

    fn foreign_security_keys(&self) -> &'static [(Harness, &'static str)] {
        // `harness.claude` stays opaque until WP3.
        &[]
    }

    fn reserved_names(&self) -> &'static [&'static str] {
        &[]
    }

    fn entries_can_vanish(&self) -> bool {
        false
    }

    fn verified_versions(&self) -> &'static [&'static str] {
        VERIFIED_VERSIONS
    }

    fn locate(&self, env: &Environment, scope: Scope) -> Result<Location> {
        let file = env.codex_home()?.join("config.toml");
        // The write state stores the path as text; it is never stored lossily.
        if file.to_str().is_none() {
            return Err(Error::HarnessPathNotUtf8 {
                harness: Harness::Codex.as_str(),
            });
        }
        Ok(Location {
            scope,
            file_exists: fs::symlink_metadata(&file).is_ok(),
            binary_found: find_program(env.path.as_deref(), "codex".as_ref()).is_some(),
            file,
        })
    }

    fn read(&self, location: &Location) -> Result<HarnessRead> {
        let mut read = HarnessRead {
            location: location.clone(),
            entries: BTreeMap::new(),
            root_supported: true,
            private: PrivateRead::default(),
        };
        if !location.file_exists {
            return Ok(read);
        }
        let file = &location.file;
        // A symlinked config is resolved to its target first; the link
        // itself is kept.
        let target = fs::canonicalize(file).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                invalid(file, "not a regular file", None)
            } else {
                Error::io("resolve", file, &error)
            }
        })?;
        let (text, meta, parsed) = read_target(file, &target)?;
        read.root_supported = parsed.root_supported;
        read.entries = parsed
            .entries
            .into_iter()
            .map(|(name, entry)| {
                (
                    name,
                    ActualEntry {
                        hash: entry.hash,
                        supported_style: entry.region.is_some(),
                        preserved: entry.preserved,
                    },
                )
            })
            .collect();
        read.private = PrivateRead {
            target: Some(target),
            bytes: text.into_bytes(),
            mode: meta.mode,
            owner: meta.owner,
        };
        Ok(read)
    }

    fn dropped_preserved(
        &self,
        rendered: &Rendered,
        present: &BTreeSet<&'static str>,
    ) -> Vec<&'static str> {
        present
            .iter()
            .copied()
            .filter(|key| !document::preserved_key_fits(key, &rendered.entry))
            .collect()
    }

    fn render(&self, definition: &Definition) -> Rendered {
        let (entry, gaps) = Self::render_entry(definition);
        let object: Map<String, Value> = entry.iter().cloned().collect();
        Rendered {
            sent_hash: entry_hash(&Value::Object(object)),
            entry,
            gaps,
        }
    }

    fn apply(
        &self,
        read: &HarnessRead,
        plan: &HarnessPlan,
        journal: &mut dyn Journal,
        hooks: &Hooks<'_>,
    ) -> Result<ApplyResult> {
        let file = &read.location.file;
        let original = String::from_utf8(read.private.bytes.clone())
            .map_err(|_| invalid(file, "not UTF-8", None))?;
        let parsed = document::parse(&original)
            .map_err(|position| invalid(file, "not valid TOML", Some(position)))?;
        let mut edits = Vec::new();
        for op in &plan.ops {
            let edit = match (op.kind, &op.rendered) {
                (WriteKind::Remove, _) => document::Edit::Remove,
                (kind, Some(rendered)) => {
                    // Only preserved keys that still fit the new rendering
                    // are carried over; `dropped_preserved` reports the rest.
                    let preserved = if kind == WriteKind::Update {
                        document::preserved_values(&original, &op.name)
                            .into_iter()
                            .filter(|(key, _)| document::preserved_key_fits(key, &rendered.entry))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let text =
                        document::entry_text(&op.name, &rendered.entry, preserved, parsed.eol);
                    if kind == WriteKind::Add {
                        document::Edit::Add(text)
                    } else {
                        document::Edit::Replace(text)
                    }
                }
                (_, None) => continue,
            };
            edits.push((op.name.clone(), edit));
        }
        let mut updated = document::apply_edits(&original, &parsed, &edits);
        if let Some(hook) = hooks.after_splice {
            updated = hook(&updated);
        }
        // Nothing has been written yet: a splice that does not parse, or that
        // changes anything outside the plan, aborts before the journal.
        let not_spliced = || Error::HarnessEditFailed { path: file.clone() };
        let expected = document::parse(&updated).map_err(|_| not_spliced())?;
        if !splice_keeps_the_rest(&parsed, &expected, plan) {
            return Err(not_spliced());
        }
        let mut intents = Vec::new();
        for op in &plan.ops {
            let expected_readback = expected.entries.get(&op.name).map(|e| e.hash.clone());
            intents.push(Intent {
                name: op.name.clone(),
                kind: op.kind,
                expected_readback,
            });
        }
        journal.intend(&intents)?;

        let mut result = ApplyResult::default();
        let target = match &read.private.target {
            Some(target) => target.clone(),
            None => {
                // The binary is present but the file is missing: create it
                // (and its directory with mode 0700).
                if let Some(parent) = file.parent()
                    && fs::symlink_metadata(parent).is_err()
                {
                    store::create_dir_all(parent, &mut Vec::new())?;
                }
                result.created = true;
                file.clone()
            }
        };
        let mut owner_changed = false;
        let recheck = |temp: &Path| -> Result<()> {
            owner_changed = owner_changes(read.private.owner, temp);
            if let Some(hook) = hooks.before_persist {
                hook()?;
            }
            let changed = || Error::HarnessFileChanged { path: file.clone() };
            match &read.private.target {
                Some(target) => {
                    if fs::canonicalize(file).ok().as_ref() != Some(target) {
                        return Err(changed());
                    }
                    match read_regular(target) {
                        Ok((bytes, _)) if bytes == read.private.bytes => Ok(()),
                        _ => Err(changed()),
                    }
                }
                None if fs::symlink_metadata(file).is_ok() => Err(changed()),
                None => Ok(()),
            }
        };
        let warning =
            store::write_atomic_file(&target, updated.as_bytes(), read.private.mode, recheck)?;
        if owner_changed {
            result
                .warnings
                .push(StoreWarning::OwnerNotKept { path: file.clone() });
        }
        result.warnings.extend(warning);
        if let Some(hook) = hooks.after_persist {
            hook()?;
        }
        let verified = read_target(file, &target).ok().map(|(text, _, actual)| {
            let entries = plan
                .ops
                .iter()
                .map(|op| {
                    let readback = actual.entries.get(&op.name).map(|e| e.hash.clone());
                    let wanted = expected.entries.get(&op.name).map(|e| e.hash.clone());
                    (readback == wanted).then(|| EntryResult {
                        name: op.name.clone(),
                        readback,
                    })
                })
                .collect::<Option<Vec<_>>>();
            (text, entries)
        });
        match verified {
            Some((_, Some(entries))) if !hooks.force_verify_failure => {
                result.entries = entries;
                Ok(result)
            }
            other => {
                // Restore only when the file still holds what d-mcp wrote.
                let ours = other.is_some_and(|(text, _)| text == updated);
                let restored = ours && restore(&target, read).is_ok();
                Err(Error::HarnessVerifyFailed {
                    path: file.clone(),
                    restored,
                })
            }
        }
    }

    fn login_state(
        &self,
        env: &Environment,
        runner: &ProcessRunner,
        names: &[String],
    ) -> BTreeMap<String, LoginState> {
        let unknown = || {
            names
                .iter()
                .map(|name| (name.clone(), LoginState::Unknown))
                .collect()
        };
        let Some(program) = find_program(env.path.as_deref(), "codex".as_ref()) else {
            return unknown();
        };
        let Ok(codex_home) = env.codex_home() else {
            return unknown();
        };
        let invocation = Invocation::new(program)
            .args(["mcp", "list", "--json"])
            .env("CODEX_HOME", codex_home);
        let listing = match runner.run(&invocation, env.path.as_deref()) {
            Ok(output) if output.success() && !output.truncated => parse_listing(&output.stdout),
            _ => None,
        };
        let Some(listing) = listing else {
            return unknown();
        };
        names
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    listing.get(name).copied().unwrap_or(LoginState::Unknown),
                )
            })
            .collect()
    }
}

/// Restores the original bytes atomically, or removes a file d-mcp created.
fn restore(target: &Path, read: &HarnessRead) -> Result<()> {
    if read.private.target.is_some() {
        store::write_atomic_file(target, &read.private.bytes, read.private.mode, |_| Ok(()))
            .map(|_| ())
    } else {
        fs::remove_file(target).map_err(|error| Error::io("remove", target, &error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::WriteOp;

    fn op(name: &str, kind: WriteKind) -> WriteOp {
        WriteOp {
            name: name.to_owned(),
            kind,
            rendered: None,
        }
    }

    #[test]
    fn a_splice_must_keep_everything_outside_the_plan() {
        let original =
            document::parse("[mcp_servers.a]\ncommand = \"a\"\n[mcp_servers.b]\ncommand = \"b\"\n")
                .unwrap();
        let plan = HarnessPlan {
            ops: vec![op("a", WriteKind::Update), op("n", WriteKind::Add)],
        };
        let good = document::parse(
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.b]\ncommand = \"b\"\n\
             [mcp_servers.n]\ncommand = \"n\"\n",
        )
        .unwrap();
        assert!(splice_keeps_the_rest(&original, &good, &plan));
        for bad in [
            // A foreign entry changed.
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.b]\ncommand = \"c\"\n[mcp_servers.n]\ncommand = \"n\"\n",
            // A foreign entry vanished.
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.n]\ncommand = \"n\"\n",
            // A planned Add is missing.
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.b]\ncommand = \"b\"\n",
            // An entry nobody planned appeared.
            "[mcp_servers.a]\ncommand = \"x\"\n[mcp_servers.b]\ncommand = \"b\"\n[mcp_servers.n]\ncommand = \"n\"\n[mcp_servers.z]\ncommand = \"z\"\n",
            // The root is no longer editable.
            "mcp_servers = { a = { command = \"x\" } }\n",
        ] {
            let updated = document::parse(bad).unwrap();
            assert!(!splice_keeps_the_rest(&original, &updated, &plan), "{bad}");
        }
        let remove = HarnessPlan {
            ops: vec![op("a", WriteKind::Remove)],
        };
        let removed = document::parse("[mcp_servers.b]\ncommand = \"b\"\n").unwrap();
        assert!(splice_keeps_the_rest(&original, &removed, &remove));
        assert!(!splice_keeps_the_rest(&original, &original, &remove));
    }

    #[cfg(unix)]
    #[test]
    fn an_owner_change_is_detected_against_the_temp_file() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let own = owner_of(&fs::metadata(temp.path()).unwrap()).unwrap();
        assert!(!owner_changes(Some(own), temp.path()));
        assert!(owner_changes(
            Some((own.0.wrapping_add(1), own.1)),
            temp.path()
        ));
        assert!(owner_changes(
            Some((own.0, own.1.wrapping_add(1))),
            temp.path()
        ));
        assert!(!owner_changes(None, temp.path()));
        assert!(!owner_changes(Some(own), Path::new("/nonexistent/d-mcp")));
        let warning = StoreWarning::OwnerNotKept {
            path: "/c/config.toml".into(),
        };
        assert!(
            warning
                .to_string()
                .contains("owned by another user or group")
        );
    }

    #[test]
    fn rendering_omits_values_codex_drops() {
        let definition = crate::definition::parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\nargs = []\n\
             [harness.codex]\nenabled = true\nrequired = false\ndisabled_tools = []\n\
             tool_timeout_sec = 5\n",
            "a",
        )
        .unwrap();
        let rendered = CodexAdapter.render(&definition);
        let keys: Vec<&str> = rendered.entry.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["command", "tool_timeout_sec"]);
        let explicit = crate::definition::parse_definition(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n\
             [harness.codex]\nenabled = false\nrequired = true\ndisabled_tools = [\"w\"]\n",
            "a",
        )
        .unwrap();
        let keys: Vec<String> = CodexAdapter
            .render(&explicit)
            .entry
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, ["command", "enabled", "required", "disabled_tools"]);
    }

    #[test]
    fn listing_maps_auth_status() {
        let listing = parse_listing(
            br#"[{"name":"a","auth_status":"o_auth"},{"name":"b","auth_status":"not_logged_in"},
                {"name":"c","auth_status":"unsupported"},{"name":"d","auth_status":"bearer_token"},
                {"name":"e","auth_status":"unknown"},{"name":"f"},{"auth_status":"o_auth"}]"#,
        )
        .unwrap();
        assert_eq!(listing["a"], LoginState::LoggedIn);
        assert_eq!(listing["b"], LoginState::NeedsLogin);
        assert_eq!(listing["c"], LoginState::NotApplicable);
        assert_eq!(listing["d"], LoginState::NotApplicable);
        assert_eq!(listing["e"], LoginState::Unknown);
        assert_eq!(listing["f"], LoginState::Unknown);
        assert_eq!(listing.len(), 6);
        assert!(parse_listing(b"not json").is_none());
        assert_eq!(login_state_of("logged_in"), LoginState::LoggedIn);
    }

    #[test]
    fn declarations() {
        let adapter = CodexAdapter;
        assert_eq!(adapter.harness(), Harness::Codex);
        assert_eq!(adapter.apply_mode(), ApplyMode::DirectEdit);
        assert_eq!(adapter.discover_scopes(), [Scope::User]);
        assert_eq!(adapter.write_scopes(), [Scope::User]);
        assert!(adapter.hashes_comparable());
        assert!(adapter.reserved_names().is_empty());
        assert!(!adapter.entries_can_vanish());
        assert!(adapter.foreign_security_keys().is_empty());
        assert_eq!(adapter.verified_versions(), ["codex-cli 0.165.0"]);
        assert_eq!(adapter.override_keys().len(), 7);
        assert!(
            adapter
                .excluded_override_keys()
                .contains(&"http_headers_helper")
        );
    }
}
