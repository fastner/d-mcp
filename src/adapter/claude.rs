//! The Claude Code adapter: user-scope entries written through the `claude`
//! CLI, read from Claude Code's configuration file (`~/.claude.json`), which
//! d-mcp never writes.
//!
//! Rendering (see `docs/reference.md`, "Claude Code mapping"):
//!
//! - stdio: `command`, `args` (omitted when empty); a forward becomes an
//!   `env` value `${VAR:-}` (an unset variable reads as empty; a bare
//!   `${VAR}` would reach the server as the literal text), a literal passes
//!   through.
//! - http: `type = "http"`, `url`; `bearer_env` becomes the header
//!   `Authorization: Bearer ${VAR}`, `header_env` the named header with
//!   `${VAR}`; OAuth's `client_id` becomes `oauth.clientId`, `scopes` the
//!   space-joined `oauth.scopes`, and `harness.claude.oauth_callback_port`
//!   `oauth.callbackPort`.
//! - `client_secret_env`: the variable's value is passed to `add-json
//!   --client-secret` through `MCP_CLIENT_SECRET` in the child environment
//!   only, on Add and Update.
//! - `harness.claude.timeout` becomes `timeout` (milliseconds).
//!
//! The entry hash (`d-mcp-claude-entry-v1`) is semantic and applied alike to
//! the rendering and the read-back entry (see [`file::canonical_entry`]), so
//! the two are comparable.

mod cli;
mod discover;
mod file;
mod import;
mod settings;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use std::path::Path;

use super::{
    ActualEntry, Adapter, AdapterTexts, ApplyContext, ApplyMode, ApplyResult, Gap, GapSeverity,
    HarnessPlan, HarnessRead, Hooks, ImportGap, Imported, Journal, Location, LoginState,
    OverrideKey, OverrideType, PrivateRead, RawEntry, Rendered, Scope, ScopeRead,
};
use crate::config::Harness;
use crate::definition::{Auth, Definition, EnvValue, Transport};
use crate::error::{Error, Result};
use crate::process::{ProcessRunner, find_program};
use crate::store::Environment;

pub use cli::{PROGRAM, WRITE_TIMEOUT, uses_oauth};
pub use file::{
    Approvals, ENTRY_HASH_TAG, Parsed, ProjectFlags, ReadOnlyFile, canonical_entry, entry_hash,
    parse,
};

/// The Claude Code versions this adapter was verified against.
pub const VERIFIED_VERSIONS: &[&str] = &["2.1.310 (Claude Code)"];

/// Entry names Claude Code reserves; `claude mcp add` rejects them
/// (code.claude.com/docs/en/mcp; `workspace` verified against 2.1.310). The
/// neutral name rule already excludes the last two.
pub const RESERVED_NAMES: &[&str] = &[
    "workspace",
    "claude-in-chrome",
    "computer-use",
    "Claude Preview",
    "Claude Browser",
];

/// The allowed `harness.claude` override keys.
pub const OVERRIDE_KEYS: &[OverrideKey] = &[
    OverrideKey {
        key: "timeout",
        kind: OverrideType::Milliseconds,
        stdio_only: false,
    },
    OverrideKey {
        key: "oauth_callback_port",
        kind: OverrideType::Port,
        stdio_only: false,
    },
];

/// Claude Code keys deliberately excluded from the override allow-list:
/// `headersHelper` runs a local command (like Codex's
/// `http_headers_helper`), and `env` belongs to the neutral definition.
pub const EXCLUDED_OVERRIDE_KEYS: &[&str] = &["headersHelper", "headers_helper", "env"];

/// Variable names Claude Code reads as empty in a remote server's `url` and
/// `headers`, ignoring a `:-default` (code.claude.com/docs/en/mcp, and
/// observed with Claude Code 2.1.310 through a local HTTP endpoint). The
/// list can be incomplete.
pub const CREDENTIAL_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_SECRET_ACCESS_KEY",
    "CODEARTIFACT_AUTH_TOKEN",
    "HTTPS_PROXY",
    "NPM_TOKEN",
];

/// Prefixes treated as credential names (decision: conservative, because
/// Claude Code's own credentials live there).
pub const CREDENTIAL_PREFIXES: &[&str] = &["ANTHROPIC_", "AWS_"];

/// A name under a credential prefix that is not a credential.
pub const CREDENTIAL_EXCEPTIONS: &[&str] = &["ANTHROPIC_BASE_URL"];

/// Suffixes treated as credential names (proxy settings).
pub const CREDENTIAL_SUFFIXES: &[&str] = &["_PROXY"];

/// Whether Claude Code may read a variable as empty in a remote server's
/// headers. Case-insensitive.
#[must_use]
pub fn is_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if CREDENTIAL_EXCEPTIONS.contains(&upper.as_str()) {
        return false;
    }
    CREDENTIAL_NAMES.contains(&upper.as_str())
        || CREDENTIAL_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| upper.ends_with(suffix))
}

/// The Claude Code adapter's report texts.
pub const TEXTS: AdapterTexts = AdapterTexts {
    unsupported_root: "`mcpServers` in the Claude Code configuration is not a JSON object; d-mcp does not edit it",
    unsupported_entry: "not a JSON object, so d-mcp does not edit it; restore the entry",
    relocated_leftover: "the formerly managed entries",
    location_variable: "CLAUDE_CONFIG_DIR",
};

/// The Claude Code adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeAdapter;

fn gap(field: &str, severity: GapSeverity, reason: String) -> Gap {
    Gap {
        field: field.to_owned(),
        severity,
        reason,
        unless_present: None,
    }
}

fn credential_gap(var: &str) -> Gap {
    gap(
        "auth.var",
        GapSeverity::Important,
        format!(
            "Claude Code reads `{var}` as empty in a remote server's headers (it is one of Claude \
             Code's own credential or proxy variables); use another variable name"
        ),
    )
}

/// The [`HarnessRead`] of the user scope from a parsed configuration file.
fn harness_read(location: &Location, parsed: &Parsed) -> HarnessRead {
    HarnessRead {
        location: location.clone(),
        entries: parsed
            .entries
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    ActualEntry {
                        hash: entry_hash(value),
                        supported_style: value.is_object(),
                        preserved: BTreeSet::new(),
                    },
                )
            })
            .collect(),
        root_supported: parsed.root_supported,
        // No file bytes are kept.
        private: PrivateRead::default(),
        shadowed: parsed.local.clone(),
    }
}

impl ClaudeAdapter {
    fn render_entry(definition: &Definition) -> (Vec<(String, Value)>, Vec<Gap>, Option<String>) {
        let mut entry: Vec<(String, Value)> = Vec::new();
        let mut gaps = Vec::new();
        let mut secret_env = None;
        let overrides = definition.overrides.get(Harness::Claude.as_str());
        let port = overrides.and_then(|table| table.get("oauth_callback_port"));
        let mut port_used = false;
        match &definition.transport {
            Transport::Stdio { command, args, env } => {
                entry.push(("command".to_owned(), json!(command)));
                if !args.is_empty() {
                    entry.push(("args".to_owned(), json!(args)));
                }
                let env: Map<String, Value> = env
                    .iter()
                    .map(|(name, value)| {
                        let value = match value {
                            EnvValue::Forward => format!("${{{name}:-}}"),
                            EnvValue::Literal(literal) => literal.clone(),
                        };
                        (name.clone(), Value::String(value))
                    })
                    .collect();
                if !env.is_empty() {
                    entry.push(("env".to_owned(), Value::Object(env)));
                }
            }
            Transport::Http { url, auth } => {
                entry.push(("type".to_owned(), json!("http")));
                entry.push(("url".to_owned(), json!(url)));
                match auth {
                    Auth::OAuth {
                        client_id,
                        scopes,
                        client_secret_env,
                    } => {
                        let mut oauth = Map::new();
                        if let Some(client_id) = client_id {
                            oauth.insert("clientId".to_owned(), json!(client_id));
                        }
                        if !scopes.is_empty() {
                            oauth.insert("scopes".to_owned(), json!(scopes.join(" ")));
                        }
                        if let Some(port) = port {
                            oauth.insert("callbackPort".to_owned(), port.clone());
                            port_used = true;
                        }
                        if !oauth.is_empty() {
                            entry.push(("oauth".to_owned(), Value::Object(oauth)));
                        }
                        secret_env.clone_from(client_secret_env);
                    }
                    Auth::BearerEnv {
                        var,
                        oauth_supported,
                    } => {
                        entry.push((
                            "headers".to_owned(),
                            json!({ "Authorization": format!("Bearer ${{{var}}}") }),
                        ));
                        if is_credential_name(var) {
                            gaps.push(credential_gap(var));
                        }
                        if *oauth_supported {
                            gaps.push(gap(
                                "auth.oauth_supported",
                                GapSeverity::Note,
                                "Claude Code ignores `oauth_supported`; it sends the bearer token"
                                    .to_owned(),
                            ));
                        }
                    }
                    Auth::HeaderEnv { header, var } => {
                        let mut headers = Map::new();
                        headers.insert(header.clone(), Value::String(format!("${{{var}}}")));
                        entry.push(("headers".to_owned(), Value::Object(headers)));
                        if is_credential_name(var) {
                            gaps.push(credential_gap(var));
                        }
                    }
                    Auth::None => gaps.push(gap(
                        "auth",
                        GapSeverity::Note,
                        "Claude Code has no explicit no-auth setting; it falls back to OAuth \
                         discovery"
                            .to_owned(),
                    )),
                }
            }
        }
        if port.is_some() && !port_used {
            gaps.push(gap(
                "harness.claude.oauth_callback_port",
                GapSeverity::Note,
                "ignored: the server does not authenticate through OAuth".to_owned(),
            ));
        }
        if let Some(timeout) = overrides.and_then(|table| table.get("timeout")) {
            entry.push(("timeout".to_owned(), timeout.clone()));
        }
        if definition
            .overrides
            .get(Harness::Codex.as_str())
            .is_some_and(|table| table.contains_key("cwd"))
        {
            gaps.push(gap(
                "harness.codex.cwd",
                GapSeverity::Note,
                "Claude Code has no working-directory setting; the server starts in Claude \
                 Code's working directory"
                    .to_owned(),
            ));
        }
        (entry, gaps, secret_env)
    }
}

impl Adapter for ClaudeAdapter {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn apply_mode(&self) -> ApplyMode {
        ApplyMode::HarnessCli
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
        // Claude Code has no per-server tool filter, so it cannot enforce
        // Codex's: such definitions are not written to Claude Code.
        &[
            (Harness::Codex, "enabled_tools"),
            (Harness::Codex, "disabled_tools"),
        ]
    }

    fn reserved_names(&self) -> &'static [&'static str] {
        RESERVED_NAMES
    }

    fn entries_can_vanish(&self) -> bool {
        // A concurrent Claude Code session rewrites the file from memory.
        true
    }

    fn verified_versions(&self) -> &'static [&'static str] {
        VERIFIED_VERSIONS
    }

    fn program(&self) -> &'static str {
        PROGRAM
    }

    fn texts(&self) -> &'static AdapterTexts {
        &TEXTS
    }

    fn locate(&self, env: &Environment, scope: Scope) -> Result<Location> {
        let file = env.claude_config_file()?;
        if file.to_str().is_none() {
            return Err(Error::HarnessPathNotUtf8 {
                harness: Harness::Claude.as_str(),
            });
        }
        Ok(Location {
            scope,
            file_exists: std::fs::symlink_metadata(&file).is_ok(),
            binary_found: find_program(env.path.as_deref(), PROGRAM.as_ref()).is_some(),
            file,
        })
    }

    fn read(&self, location: &Location) -> Result<HarnessRead> {
        let parsed = file::read(&location.file)?;
        Ok(harness_read(location, &parsed))
    }

    fn discover(&self, env: &Environment, repo_root: &Path) -> Vec<ScopeRead> {
        discover::discover(self, env, repo_root)
    }

    fn import(
        &self,
        read: &ScopeRead,
        name: &str,
    ) -> std::result::Result<Imported, Vec<ImportGap>> {
        import::import(read, name)
    }

    fn canonical_entry(&self, value: &Value) -> Value {
        canonical_entry(value)
    }

    fn entry_hash(&self, value: &Value) -> String {
        entry_hash(value)
    }

    fn normalized_entry(&self, raw: &RawEntry, imported: &Imported) -> Value {
        import::normalized(raw.value(), imported)
    }

    fn render(&self, definition: &Definition) -> Rendered {
        let (entry, gaps, secret_env) = Self::render_entry(definition);
        Rendered {
            sent_hash: entry_hash(&cli::entry_object(&entry)),
            entry,
            gaps,
            secret_env,
        }
    }

    fn apply(
        &self,
        ctx: &ApplyContext<'_>,
        read: &HarnessRead,
        plan: &HarnessPlan,
        journal: &mut dyn Journal,
        hooks: &Hooks<'_>,
    ) -> Result<ApplyResult> {
        cli::apply(ctx, read, plan, journal, hooks)
    }

    fn login_state(
        &self,
        env: &Environment,
        runner: &ProcessRunner,
        names: &[String],
    ) -> BTreeMap<String, LoginState> {
        cli::login_state(env, runner, names)
    }
}

#[cfg(test)]
mod tests;
