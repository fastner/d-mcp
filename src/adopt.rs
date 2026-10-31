//! `d-mcp adopt`: take an existing harness entry over as a d-mcp definition,
//! preview first, lossless and literal-free.
//!
//! - **Preview** (without `--apply`, or with `--apply --dry-run`): read-only,
//!   no lock, no write. It shows the definition d-mcp would write (redacted),
//!   per harness a redacted before/after view of every entry it would take
//!   over with its `display_hash` and `write` (`none`, `update` or `add`),
//!   the replaced literals and the declared normalizations, and every
//!   refusal.
//! - **Literals**: every env and header literal becomes a variable reference
//!   (see [`crate::adapter::Imported`]); `--keep-literal` keeps an env literal
//!   that passes `secret_value_heuristic`, `--var` names a header variable.
//! - **Refusals** (`adopt-refused`, exit 1): the candidate is serialized to
//!   TOML and must pass `parse_definition` and `validate_definition`
//!   unchanged, the import must be lossless (an entry-hash round trip with the
//!   declared normalizations), and every rule of `docs/reference.md`,
//!   "Adoption", must hold. A refusal names the entry, the field and a reason
//!   code, never a value.
//! - **Apply** (`--apply`, with an `--expect` per entry): under the store
//!   lock, everything is checked again against a fresh read (an entry that
//!   changed is `adopt-entry-changed`, exit 3), then the definition is
//!   written to `local/servers/<name>.toml`, then the selection, then the
//!   entries are taken over by a scoped sync ([`crate::sync::sync_under_lock`]).
//!
//! Entry content is rendered only through [`crate::redact`]; replaced
//! literals stay in memory ([`crate::adapter::Literal`]) and are compared with
//! the variables of the current shell, never printed.

mod apply;
mod candidate;
mod origin;
mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::Serialize;

use crate::adapter::{VarLookup, repository_root};
use crate::config::Harness;
use crate::error::{Error, Result};
use crate::hashkey::{self, HashKey};
use crate::output::Report;
use crate::source::ReadInputs;
use crate::store::{Environment, StoreLock, StorePaths};

pub use candidate::ServerPlan;
pub use origin::OriginPlan;

/// The scope part of `<harness>:<scope>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeKind {
    /// The user scope.
    User,
    /// Claude Code's local scope (names only; never adopted).
    Local,
    /// The project scope of the current repository.
    Project,
}

impl ScopeKind {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::User => "user",
            ScopeKind::Local => "local",
            ScopeKind::Project => "project",
        }
    }
}

/// `<harness>:<scope>`: where an entry lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Place {
    /// The harness.
    pub harness: Harness,
    /// The scope.
    pub scope: ScopeKind,
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.harness.as_str(), self.scope.as_str())
    }
}

impl FromStr for Place {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let (harness, scope) = text
            .split_once(':')
            .ok_or_else(|| "expected <harness>:<scope>".to_owned())?;
        let harness = match harness {
            "claude" => Harness::Claude,
            "codex" => Harness::Codex,
            _ => return Err("the harness must be `claude` or `codex`".to_owned()),
        };
        let scope = match scope {
            "user" => ScopeKind::User,
            "local" => ScopeKind::Local,
            "project" => ScopeKind::Project,
            _ => return Err("the scope must be `user`, `local` or `project`".to_owned()),
        };
        Ok(Self { harness, scope })
    }
}

/// `<harness>:<scope>:<name>`: one entry to adopt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryRef {
    /// Where it lives.
    pub place: Place,
    /// The entry name.
    pub name: String,
}

impl fmt::Display for EntryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.place, self.name)
    }
}

impl FromStr for EntryRef {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = text.splitn(3, ':');
        let (Some(harness), Some(scope), Some(name)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err("expected <harness>:<scope>:<name>".to_owned());
        };
        if name.is_empty() {
            return Err("the entry name is empty".to_owned());
        }
        Ok(Self {
            place: format!("{harness}:{scope}").parse()?,
            name: name.to_owned(),
        })
    }
}

/// `--expect <harness>:<scope>:<name>=<display-hash>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    /// The entry.
    pub entry: EntryRef,
    /// The display hash from the preview.
    pub display_hash: String,
}

impl FromStr for Expect {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let (entry, hash) = text
            .rsplit_once('=')
            .ok_or_else(|| "expected <harness>:<scope>:<name>=<display-hash>".to_owned())?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("the display hash must be 64 hex characters".to_owned());
        }
        Ok(Self {
            entry: entry.parse()?,
            display_hash: hash.to_ascii_lowercase(),
        })
    }
}

/// `--var <field>=<NAME>`: the variable of a replaced header literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarChoice {
    /// The native field, such as `headers.Authorization`.
    pub field: String,
    /// The variable name.
    pub var: String,
}

impl FromStr for VarChoice {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        match text.rsplit_once('=') {
            Some((field, var)) if !field.is_empty() && !var.is_empty() => Ok(Self {
                field: field.to_owned(),
                var: var.to_owned(),
            }),
            _ => Err("expected <field>=<NAME>".to_owned()),
        }
    }
}

/// The options of `adopt`.
#[derive(Default)]
pub struct AdoptOptions<'a> {
    /// The entries to adopt; grouped by name, one definition per name.
    pub entries: Vec<EntryRef>,
    /// Act instead of previewing.
    pub apply: bool,
    /// With `apply`: preview only.
    pub dry_run: bool,
    /// Env variables whose literal value is kept (`{ value = ... }`).
    pub keep_literal: BTreeSet<String>,
    /// Header variable names, by native field.
    pub vars: Vec<VarChoice>,
    /// Adopt a project-scope entry into the user scope.
    pub to_user_scope: bool,
    /// The origin whose core wins a cross-harness conflict.
    pub from: Option<Place>,
    /// Replace header auth with OAuth.
    pub prefer_oauth: bool,
    /// Keep `bearer_env` and declare `oauth_supported`.
    pub oauth_supported: bool,
    /// Accept that rewriting an OAuth-capable Claude Code entry clears its
    /// stored tokens and client secret.
    pub accept_relogin: bool,
    /// The variable holding the OAuth client secret.
    pub client_secret_env: Option<String>,
    /// The display hashes from the preview, required with `apply`.
    pub expect: Vec<Expect>,
    /// Accept a variable whose value differs from the replaced literal.
    pub allow_env_mismatch: bool,
    /// Accept a variable that is unset or empty.
    pub allow_missing_env: bool,
    /// Looks up a variable of d-mcp's environment; `None` reads the process
    /// environment. The values are compared in memory only.
    pub var: Option<VarLookup<'a>>,
}

/// Why `adopt` refuses an entry. Names only: never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    /// The entry (`<harness>:<scope>:<name>`), or `None` for the server as a
    /// whole or a command-line flag.
    pub entry: Option<String>,
    /// The field (native, or of the neutral definition), or `""`.
    pub field: String,
    /// The reason code: an import gap reason, a definition rule code, or an
    /// adoption reason (see `docs/reference.md`).
    pub reason: String,
}

impl Refusal {
    fn new(entry: Option<String>, field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            entry,
            field: field.into(),
            reason: reason.into(),
        }
    }
}

/// The whole adoption: one plan per server name.
pub struct Adoption {
    /// The store root.
    pub store: PathBuf,
    /// The current repository.
    pub repository: PathBuf,
    /// Whether `--apply` acted (not a preview).
    pub apply: bool,
    /// Whether display hashes are shown (the store has a key).
    pub display_hashes: bool,
    /// The servers.
    pub servers: Vec<ServerPlan>,
    /// Refusals of command-line flags that match no server.
    pub refusals: Vec<Refusal>,
    /// Command-level notices.
    pub notices: Vec<String>,
    /// What the apply wrote: the definition files and the selections.
    pub written: Option<Written>,
    /// The `data` of the scoped sync.
    pub sync: Option<serde_json::Value>,
}

/// What an apply wrote before the scoped sync.
#[derive(Debug, Clone, Default)]
pub struct Written {
    /// The definition files.
    pub definitions: Vec<PathBuf>,
    /// The selected servers.
    pub selections: Vec<String>,
}

impl Adoption {
    /// The number of refusals.
    #[must_use]
    pub fn refusal_count(&self) -> usize {
        self.refusals.len()
            + self
                .servers
                .iter()
                .map(|server| server.refusals.len())
                .sum::<usize>()
    }
}

/// What the plan needs from the store and the environment.
pub(crate) struct Context<'a> {
    pub(crate) paths: &'a StorePaths,
    pub(crate) env: &'a Environment,
    pub(crate) options: &'a AdoptOptions<'a>,
    pub(crate) repository: PathBuf,
    pub(crate) key: Option<HashKey>,
    /// `lock.toml` and `approvals.toml`, for the shared resolution.
    pub(crate) inputs: ReadInputs,
}

impl Context<'_> {
    /// Looks a variable up in d-mcp's environment.
    pub(crate) fn lookup(&self, name: &str) -> Option<std::ffi::OsString> {
        match self.options.var {
            Some(var) => var(name),
            None => std::env::var_os(name),
        }
    }
}

/// The error of a plan, in the precedence 3, 4, 1: an entry that changed,
/// a missing harness CLI, then the refusals.
fn plan_error(adoption: &Adoption, changed: Option<Error>, cli: Option<Error>) -> Option<Error> {
    changed.or(cli).or_else(|| {
        let count = adoption.refusal_count();
        (count > 0).then_some(Error::AdoptRefused { count })
    })
}

/// Runs `adopt`. A preview reads only; `--apply` writes under the store
/// lock. The report carries `data` also with an error, like `sync`.
///
/// # Errors
///
/// Global errors before anything is planned: an uninitialized store, the
/// lock, a corrupt `config.toml`, `state.toml` or `.hash-key`, an unknown
/// current directory.
pub fn adopt(
    paths: &StorePaths,
    env: &Environment,
    options: &AdoptOptions<'_>,
    warnings: &mut Vec<String>,
) -> Result<(Report, Option<Error>)> {
    paths.require_initialized()?;
    let apply = options.apply && !options.dry_run;
    let lock = if apply {
        Some(StoreLock::acquire(paths)?)
    } else {
        None
    };
    let config = paths.load_config()?;
    let state = crate::state::load(paths)?;
    let key = match &lock {
        Some(lock) => {
            let (key, _, warning) = hashkey::ensure(paths, lock)?;
            warnings.extend(warning.map(|warning| warning.to_string()));
            Some(key)
        }
        None => match hashkey::load(paths) {
            Ok(key) => key,
            Err(error) => {
                warnings.push(format!("no display hashes: {}", error.hint()));
                None
            }
        },
    };
    let inputs = ReadInputs::load(paths)?;
    let cwd = env.cwd.clone().ok_or(Error::CurrentDirUnavailable)?;
    let context = Context {
        paths,
        env,
        options,
        repository: repository_root(&cwd),
        key,
        inputs,
    };
    let mut adoption = Adoption {
        store: paths.root.clone(),
        repository: context.repository.clone(),
        apply,
        display_hashes: context.key.is_some(),
        servers: Vec::new(),
        refusals: Vec::new(),
        notices: Vec::new(),
        written: None,
        sync: None,
    };
    if context.key.is_none() {
        adoption.notices.push(
            "the store has no .hash-key, so no display hashes are shown; run `d-mcp init` to create it"
                .to_owned(),
        );
    }
    let mut groups: BTreeMap<&str, Vec<&EntryRef>> = BTreeMap::new();
    for entry in &options.entries {
        groups.entry(entry.name.as_str()).or_default().push(entry);
    }
    // Every harness is read once, fresh (under the lock for an apply).
    let reads: BTreeMap<Harness, Vec<crate::adapter::ScopeRead>> = options
        .entries
        .iter()
        .map(|entry| entry.place.harness)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|harness| {
            let adapter = crate::adapter::adapter_for(harness)?;
            Some((harness, adapter.discover(env, &context.repository)))
        })
        .collect();
    let mut cli_missing = None;
    for (name, entries) in groups {
        let server = candidate::plan_server(&context, &reads, &config, &state, name, &entries);
        cli_missing = cli_missing.or_else(|| server.cli_missing.clone());
        adoption.servers.push(server);
    }
    adoption
        .refusals
        .extend(candidate::unmatched_flags(options, &adoption.servers));
    let mut changed = None;
    if apply {
        let (expect_refusals, expect_changed) = apply::check_expectations(&context, &adoption);
        adoption.refusals.extend(expect_refusals);
        changed = expect_changed;
    }
    let mut error = plan_error(&adoption, changed, cli_missing);
    if let (Some(lock), None) = (&lock, &error) {
        error = apply::apply(&context, lock, config, &mut adoption, warnings);
    }
    drop(lock);
    let report = Report::new(render::json(&adoption), render::text(&adoption));
    Ok((report, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_parse_strictly() {
        let entry: EntryRef = "codex:user:git:hub".parse().unwrap();
        assert_eq!(entry.place.harness, Harness::Codex);
        assert_eq!(entry.place.scope, ScopeKind::User);
        assert_eq!(entry.name, "git:hub");
        assert_eq!(entry.to_string(), "codex:user:git:hub");
        for bad in [
            "codex",
            "codex:user",
            "codex:user:",
            "x:user:a",
            "codex:global:a",
        ] {
            assert!(bad.parse::<EntryRef>().is_err(), "{bad}");
        }
        let place: Place = "claude:project".parse().unwrap();
        assert_eq!(place.to_string(), "claude:project");
        assert_eq!(ScopeKind::Local.as_str(), "local");
        assert!("claude".parse::<Place>().is_err());
        let hash = "A".repeat(64);
        let expect: Expect = format!("claude:local:a={hash}").parse().unwrap();
        assert_eq!(expect.display_hash, "a".repeat(64));
        assert!("claude:user:a=xyz".parse::<Expect>().is_err());
        assert!("claude:user:a".parse::<Expect>().is_err());
        assert!(format!("claude:user={hash}").parse::<Expect>().is_err());
        let var: VarChoice = "headers.Authorization=GH_TOKEN".parse().unwrap();
        assert_eq!(
            (var.field.as_str(), var.var.as_str()),
            ("headers.Authorization", "GH_TOKEN")
        );
        for bad in ["x", "=X", "x="] {
            assert!(bad.parse::<VarChoice>().is_err(), "{bad}");
        }
    }
}
