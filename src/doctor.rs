//! `d-mcp doctor`: health checks of the store, the harnesses and the
//! managed entries. Read-only: no lock, no writes, no `.hash-key`.
//!
//! Every check has a severity, `ok`, `warn` or `error`, a message and a
//! hint. `doctor` exits 1 when any check is `error` (`doctor-failed`, with
//! the document as `data`); `warn` keeps exit 0. A corrupt or uninitialized
//! store is reported as `error` checks instead of aborting, and the checks
//! that need the store are then left out.
//!
//! The checks, in output order: `store`, `state`, `hash-key`,
//! `harness-version` (per adapter, see `version`), `discovery` (only when
//! it fails), `harness-config`,
//! `drift`, `location-changed`, `pending`, `recurring-vanish`, `shadowing`,
//! `static-token`, `name-length`, `credential-names`, `managed-policy`,
//! and with `--auth` `login`. A check without findings reports one `ok`.
//! Messages name harnesses, scopes, files, entries and variables only.

mod render;
mod version;

use crate::adapter::{self, Adapter, LoginState, Scope};
use crate::config::{Config, Harness, HarnessSet};
use crate::definition::{Auth, MAX_NAME_LEN, Transport};
use crate::error::{Error, Result};
use crate::hashkey;
use crate::inventory::{self, Class, Inventory, InventoryOptions};
use crate::output::Report;
use crate::process::ProcessRunner;
use crate::resolver::{Status, resolve};
use crate::source::{scan, source_specs};
use crate::state::{self, OriginScope, State};
use crate::status::{AUTH_WARNING, login_states};
use crate::store::{Environment, StorePaths};
use crate::validate::validate;
use version::Probe;

/// How serious a check is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Nothing to do (also informational checks).
    Ok,
    /// Something to look at; `doctor` still exits 0.
    Warn,
    /// Something is broken; `doctor` exits 1.
    Error,
}

impl Severity {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// The kind of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    /// `config.toml`: initialized, valid, supported schema version.
    Store,
    /// `state.toml`: valid, supported schema version.
    State,
    /// `.hash-key`: present and valid.
    HashKey,
    /// The harness CLI's presence and `--version`.
    HarnessVersion,
    /// Discovery (the inventory the harness checks build on) could run.
    Discovery,
    /// Every discovered harness file could be read.
    HarnessConfig,
    /// An owned entry was changed outside d-mcp.
    Drift,
    /// Records for another harness file.
    LocationChanged,
    /// An interrupted write.
    Pending,
    /// An owned entry vanished and was re-added at least twice.
    RecurringVanish,
    /// A narrower scope shadows a managed entry.
    Shadowing,
    /// A static token where the server supports OAuth.
    StaticToken,
    /// An unmanaged entry name longer than the name limit.
    NameLength,
    /// The credential-name list can be incomplete (informational).
    CredentialNames,
    /// Managed-policy deny lists are invisible to d-mcp (informational).
    ManagedPolicy,
    /// The login state of a managed entry (`--auth`).
    Login,
}

impl CheckId {
    /// The stable string form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CheckId::Store => "store",
            CheckId::State => "state",
            CheckId::HashKey => "hash-key",
            CheckId::HarnessVersion => "harness-version",
            CheckId::Discovery => "discovery",
            CheckId::HarnessConfig => "harness-config",
            CheckId::Drift => "drift",
            CheckId::LocationChanged => "location-changed",
            CheckId::Pending => "pending",
            CheckId::RecurringVanish => "recurring-vanish",
            CheckId::Shadowing => "shadowing",
            CheckId::StaticToken => "static-token",
            CheckId::NameLength => "name-length",
            CheckId::CredentialNames => "credential-names",
            CheckId::ManagedPolicy => "managed-policy",
            CheckId::Login => "login",
        }
    }
}

/// One check result.
#[derive(Debug, Clone)]
pub struct Check {
    /// The kind.
    pub check: CheckId,
    /// The severity.
    pub severity: Severity,
    /// The harness it is about.
    pub harness: Option<Harness>,
    /// The entry or server it is about.
    pub server: Option<String>,
    /// The error code behind it, if any.
    pub code: Option<&'static str>,
    /// What was found.
    pub message: String,
    /// What to do; `None` when nothing is to do.
    pub hint: Option<String>,
}

impl Check {
    fn new(check: CheckId, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            check,
            severity,
            harness: None,
            server: None,
            code: None,
            message: message.into(),
            hint: None,
        }
    }

    fn ok(check: CheckId, message: impl Into<String>) -> Self {
        Self::new(check, Severity::Ok, message)
    }

    fn warn(check: CheckId, message: impl Into<String>) -> Self {
        Self::new(check, Severity::Warn, message)
    }

    fn error(check: CheckId, error: &Error) -> Self {
        let mut check = Self::new(check, Severity::Error, error.hint());
        check.code = Some(error.code());
        check
    }

    fn harness(mut self, harness: Harness) -> Self {
        self.harness = Some(harness);
        self
    }

    fn server(mut self, server: &str) -> Self {
        self.server = Some(server.to_owned());
        self
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// The whole doctor report.
#[derive(Debug, Clone)]
pub struct Doctor {
    /// The store root.
    pub store: std::path::PathBuf,
    /// The current repository, when discovery ran.
    pub repository: Option<std::path::PathBuf>,
    /// Whether `--auth` was given.
    pub auth: bool,
    /// The checks, in output order.
    pub checks: Vec<Check>,
}

impl Doctor {
    /// The number of checks at a severity.
    #[must_use]
    pub fn count(&self, severity: Severity) -> usize {
        self.checks
            .iter()
            .filter(|check| check.severity == severity)
            .count()
    }
}

/// The options of `doctor`.
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    /// Query the login state of managed entries.
    pub auth: bool,
}

/// What the store checks could load.
struct Loaded {
    config: Option<Config>,
    state: Option<State>,
}

fn store_checks(paths: &StorePaths, checks: &mut Vec<Check>) -> Loaded {
    if !paths.is_initialized() {
        let error = Error::StoreNotInitialized {
            path: paths.root.clone(),
        };
        checks.push(Check::error(CheckId::Store, &error).hint("run `d-mcp init`"));
        return Loaded {
            config: None,
            state: None,
        };
    }
    let config = match paths.load_config() {
        Ok(config) => {
            checks.push(Check::ok(
                CheckId::Store,
                format!(
                    "the store at {} is initialized; config.toml is valid (schema version 1)",
                    paths.root.display()
                ),
            ));
            Some(config)
        }
        Err(error) => {
            checks.push(Check::error(CheckId::Store, &error).hint(
                "fix config.toml by hand (d-mcp never rewrites a corrupt file); the checks that need the store are skipped",
            ));
            None
        }
    };
    let state = match state::load(paths) {
        Ok(state) => {
            checks.push(Check::ok(
                CheckId::State,
                format!(
                    "state.toml is valid ({} record{})",
                    state.records.len(),
                    if state.records.len() == 1 { "" } else { "s" }
                ),
            ));
            Some(state)
        }
        Err(error) => {
            checks.push(Check::error(CheckId::State, &error).hint(
                "fix state.toml by hand (d-mcp never rewrites a corrupt file); the checks that need the store are skipped",
            ));
            None
        }
    };
    checks.push(match hashkey::load(paths) {
        Ok(Some(_)) => Check::ok(CheckId::HashKey, ".hash-key is valid"),
        Ok(None) => Check::warn(
            CheckId::HashKey,
            "the store has no .hash-key, so no display hashes are shown",
        )
        .hint("run `d-mcp init` to create it"),
        Err(error) => Check::error(CheckId::HashKey, &error)
            .hint("remove .hash-key and run `d-mcp init` to create a new one"),
    });
    Loaded { config, state }
}

/// Whether d-mcp has a reason to expect the harness: its file exists, or
/// the store records or selects entries for it.
fn expected(adapter: &dyn Adapter, env: &Environment, loaded: &Loaded) -> bool {
    let harness = adapter.harness();
    let file = adapter
        .locate(env, Scope::User)
        .is_ok_and(|location| location.file_exists);
    let recorded = loaded
        .state
        .as_ref()
        .is_some_and(|state| state.records.keys().any(|key| key.harness == harness));
    let selected = loaded.config.as_ref().is_some_and(|config| {
        config
            .selections
            .iter()
            .any(|selection| match &selection.harnesses {
                HarnessSet::All => true,
                HarnessSet::Only(set) => set.contains(&harness),
            })
    });
    file || recorded || selected
}

fn version_check(adapter: &dyn Adapter, env: &Environment, loaded: &Loaded) -> Check {
    let harness = adapter.harness();
    let program = adapter.program();
    let verified = adapter.verified_versions().join(", ");
    let check = match version::probe(adapter, env, &ProcessRunner::default()) {
        Probe::NotFound if !expected(adapter, env, loaded) => Check::ok(
            CheckId::HarnessVersion,
            format!("`{program}` is not on PATH, and nothing of {} is configured or managed", harness.as_str()),
        ),
        Probe::NotFound => {
            let hint = if adapter.apply_mode() == adapter::ApplyMode::HarnessCli {
                format!("d-mcp writes {} only through `{program}`; install it or put it on PATH", harness.as_str())
            } else {
                format!("d-mcp edits the {} configuration directly, but cannot check the version; put `{program}` on PATH", harness.as_str())
            };
            Check::warn(
                CheckId::HarnessVersion,
                format!("`{program}` is not on PATH"),
            )
            .hint(hint)
        }
        Probe::Version(found) if adapter.verified_versions().contains(&found.as_str()) => {
            Check::ok(
                CheckId::HarnessVersion,
                format!("`{program} --version` reports {found}, a verified version"),
            )
        }
        Probe::Version(found) => Check::warn(
            CheckId::HarnessVersion,
            format!("`{program} --version` reports {found}, which d-mcp was not verified against (verified: {verified})"),
        )
        .hint("d-mcp may still work; check `d-mcp sync --dry-run` before writing"),
        Probe::Unrecognized => Check::warn(
            CheckId::HarnessVersion,
            format!("`{program} --version` printed no recognizable version (its output is not shown)"),
        )
        .hint(format!("d-mcp was verified against {verified}")),
        Probe::Failed(reason) => Check::warn(
            CheckId::HarnessVersion,
            format!("`{program} --version` failed ({reason}); its output is not shown"),
        )
        .hint(format!("d-mcp was verified against {verified}")),
    };
    check.harness(harness)
}

/// One `ok` check when a check kind found nothing.
fn or_ok(checks: &mut Vec<Check>, found: Vec<Check>, check: CheckId, message: &str) {
    if found.is_empty() {
        checks.push(Check::ok(check, message));
    } else {
        checks.extend(found);
    }
}

/// The findings of the checks that build on the inventory, by kind.
#[derive(Default)]
struct Findings {
    config: Vec<Check>,
    drift: Vec<Check>,
    location_changed: Vec<Check>,
    pending: Vec<Check>,
    shadowing: Vec<Check>,
    name_length: Vec<Check>,
}

fn inventory_findings(inventory: &Inventory, state: Option<&State>) -> Findings {
    let mut found = Findings::default();
    for harness in &inventory.harnesses {
        let name = harness.harness.as_str();
        for scope in &harness.scopes {
            if let Some(error) = &scope.error {
                let user = scope.scope == adapter::DiscoveredScope::User;
                let mut check =
                    Check::error(CheckId::HarnessConfig, error).harness(harness.harness);
                check.message = format!("{name} {} scope: {}", scope.scope.as_str(), error.hint());
                if user {
                    check = check.hint("fix the file by hand; sync, status and uninstall cannot plan this harness until then");
                } else {
                    check.severity = Severity::Warn;
                    check = check.hint("fix the file by hand; the harness may ignore or refuse it");
                }
                found.config.push(check);
            }
            for entry in &scope.entries {
                if scope.scope == adapter::DiscoveredScope::User {
                    let class = match entry.class {
                        Class::Drifted => Some((&mut found.drift, Check::warn(
                            CheckId::Drift,
                            format!("`{}` was changed outside d-mcp", entry.name),
                        )
                        .hint(format!(
                            "run `d-mcp sync --take-over {}` to accept the change, or `d-mcp unmanage {} --harness {name}` to keep it unmanaged",
                            entry.name, entry.name
                        )))),
                        Class::LocationChanged => Some((&mut found.location_changed, Check::warn(
                            CheckId::LocationChanged,
                            format!("`{}` is recorded for another {name} file", entry.name),
                        )
                        .hint("run `d-mcp sync --relocate` to manage it at the current location"))),
                        Class::Pending => Some((&mut found.pending, Check::warn(
                            CheckId::Pending,
                            format!("a write of `{}` was interrupted", entry.name),
                        )
                        .hint("run `d-mcp sync` to settle it"))),
                        _ => None,
                    };
                    if let Some((list, check)) = class {
                        list.push(check.harness(harness.harness).server(&entry.name));
                    }
                    let owned = !matches!(entry.class, Class::Unmanaged | Class::Local);
                    if owned {
                        let origin = state.and_then(|state| {
                            state
                                .records
                                .iter()
                                .find(|(key, _)| {
                                    key.harness == harness.harness && key.name == entry.name
                                })
                                .and_then(|(_, record)| record.meta.adopted_from.as_ref())
                                .and_then(|from| match &from.scope {
                                    OriginScope::Project { root } => Some(root.clone()),
                                    OriginScope::User => None,
                                })
                        });
                        for shadow in &entry.shadowed_in {
                            let path = shadow.path.display();
                            let check = if shadow.scope == "project"
                                && origin.as_ref() == Some(&shadow.path)
                            {
                                Check::warn(
                                    CheckId::Shadowing,
                                    format!("`{}` is shadowed in {path} by the project-scope entry it was adopted from, which still holds its literals", entry.name),
                                )
                                .hint("remove the entry from the project's configuration by hand, so the project uses the managed entry and the literals are gone")
                            } else {
                                Check::warn(
                                    CheckId::Shadowing,
                                    format!("`{}` is shadowed by a {}-scope entry in {path}; {name} uses that entry there", entry.name, shadow.scope),
                                )
                                .hint("remove or rename the narrower entry if the managed one should apply there")
                            };
                            found
                                .shadowing
                                .push(check.harness(harness.harness).server(&entry.name));
                        }
                    }
                }
                if matches!(entry.class, Class::Unmanaged | Class::Local)
                    && entry.name.chars().count() > MAX_NAME_LEN
                {
                    found.name_length.push(
                        Check::warn(
                            CheckId::NameLength,
                            format!(
                                "the {} entry `{}` has a name longer than {MAX_NAME_LEN} characters; d-mcp cannot adopt or manage it",
                                scope.scope.as_str(),
                                entry.name
                            ),
                        )
                        .hint("rename it in the harness if d-mcp should manage it")
                        .harness(harness.harness)
                        .server(&entry.name),
                    );
                }
            }
        }
    }
    found
}

fn vanish_checks(state: &State) -> Vec<Check> {
    state
        .records
        .iter()
        .filter(|(_, record)| record.meta.vanish_count >= 2)
        .map(|(key, record)| {
            Check::warn(
                CheckId::RecurringVanish,
                format!(
                    "`{}` was removed outside d-mcp and re-added {} times (last at {} Unix time)",
                    key.name,
                    record.meta.vanish_count,
                    record.meta.last_vanish.unwrap_or_default()
                ),
            )
            .hint(format!(
                "something keeps removing it (a running {} session can rewrite its configuration from memory); close it before syncing, or run `d-mcp unmanage {} --harness {}`",
                key.harness.as_str(), key.name, key.harness.as_str()
            ))
            .harness(key.harness)
            .server(&key.name)
        })
        .collect()
}

fn static_token_checks(paths: &StorePaths, config: &Config) -> Vec<Check> {
    let mut snapshots: Vec<_> = source_specs(paths, config).iter().map(scan).collect();
    validate(&mut snapshots, adapter::registry());
    let resolution = resolve(&snapshots, &config.selections);
    resolution
        .servers
        .iter()
        .filter(|server| server.status == Status::Resolved)
        .filter_map(|server| match &server.definition.as_ref()?.transport {
            Transport::Http {
                auth: Auth::BearerEnv {
                    var,
                    oauth_supported: true,
                },
                ..
            } => Some(
                Check::warn(
                    CheckId::StaticToken,
                    format!("`{}` sends a static bearer token from `{var}`, although the server supports OAuth", server.name),
                )
                .hint("prefer `auth = { type = \"oauth\" }` in the definition, so no long-lived token is needed")
                .server(&server.name),
            ),
            _ => None,
        })
        .collect()
}

fn login_checks(inventory: &Inventory, env: &Environment) -> Vec<Check> {
    let mut checks = Vec::new();
    for harness in &inventory.harnesses {
        let Some(adapter) = adapter::adapter_for(harness.harness) else {
            continue;
        };
        let names: Vec<String> = harness
            .scopes
            .iter()
            .filter(|scope| scope.scope == adapter::DiscoveredScope::User)
            .flat_map(|scope| scope.entries.iter())
            .filter(|entry| matches!(entry.class, Class::Managed | Class::Drifted))
            .map(|entry| entry.name.clone())
            .collect();
        for (name, login) in login_states(adapter, env, &names) {
            let check = match login {
                LoginState::LoggedIn | LoginState::NotApplicable => {
                    Check::ok(CheckId::Login, format!("`{name}`: {}", login.as_str()))
                }
                LoginState::NeedsLogin => {
                    Check::warn(CheckId::Login, format!("`{name}` needs a login"))
                        .hint(format!("run `{} mcp login {name}`", adapter.program()))
                }
                LoginState::Unknown => Check::warn(
                    CheckId::Login,
                    format!("the login state of `{name}` could not be determined"),
                )
                .hint(format!(
                    "check that `{}` is on PATH and the server is reachable",
                    adapter.program()
                )),
            };
            checks.push(check.harness(harness.harness).server(&name));
        }
    }
    checks
}

/// Runs every check.
///
/// # Errors
///
/// None so far: every store and harness error is a check. The signature
/// keeps room for global errors such as an unknown current directory.
pub fn build(
    paths: &StorePaths,
    env: &Environment,
    options: &DoctorOptions,
    warnings: &mut Vec<String>,
) -> Result<Doctor> {
    let mut checks = Vec::new();
    let loaded = store_checks(paths, &mut checks);
    for adapter in adapter::registry() {
        checks.push(version_check(*adapter, env, &loaded));
    }
    let store_ok = !paths.is_initialized() || (loaded.config.is_some() && loaded.state.is_some());
    // The inventory reports its own key warnings; the `hash-key` check
    // covers them.
    let mut ignored = Vec::new();
    let inventory = if store_ok {
        match inventory::build(paths, env, &InventoryOptions::default(), &mut ignored) {
            Ok(inventory) => Some(inventory),
            Err(error) => {
                checks.push(Check::error(CheckId::Discovery, &error).hint(
                    "the harness files could not be discovered, so the checks that build on them are skipped; fix the error and run `d-mcp doctor` again",
                ));
                None
            }
        }
    } else {
        None
    };
    if store_ok {
        let mut found = inventory
            .as_ref()
            .map(|inventory| inventory_findings(inventory, loaded.state.as_ref()));
        let mut take = |pick: fn(&mut Findings) -> &mut Vec<Check>| {
            found.as_mut().map(|found| std::mem::take(pick(found)))
        };
        let state = loaded.state.as_ref();
        let config = loaded.config.as_ref();
        // The checks of the inventory are `None` without one; the others
        // need the loaded store only.
        let groups = [
            (
                CheckId::HarnessConfig,
                take(|found| &mut found.config),
                "every discovered harness file could be read",
            ),
            (
                CheckId::Drift,
                take(|found| &mut found.drift),
                "no managed entry was changed outside d-mcp",
            ),
            (
                CheckId::LocationChanged,
                take(|found| &mut found.location_changed),
                "every record is for the current harness files",
            ),
            (
                CheckId::Pending,
                take(|found| &mut found.pending),
                "no write is pending",
            ),
            (
                CheckId::RecurringVanish,
                Some(state.map(vanish_checks).unwrap_or_default()),
                "no managed entry vanished repeatedly",
            ),
            (
                CheckId::Shadowing,
                take(|found| &mut found.shadowing),
                "no managed entry is shadowed by a narrower scope",
            ),
            (
                CheckId::StaticToken,
                Some(
                    config
                        .map(|config| static_token_checks(paths, config))
                        .unwrap_or_default(),
                ),
                "no definition uses a static token where OAuth is possible",
            ),
            (
                CheckId::NameLength,
                take(|found| &mut found.name_length),
                "every unmanaged entry name fits the name limit",
            ),
        ];
        for (id, found, message) in groups {
            if let Some(found) = found {
                or_ok(&mut checks, found, id, message);
            }
        }
    }
    checks.push(
        Check::ok(
            CheckId::CredentialNames,
            "Claude Code reads some variables as empty in a remote server's URL and headers; d-mcp's list of these credential names can be incomplete",
        )
        .hint("if a header variable reaches a Claude Code server empty, rename the variable")
        .harness(Harness::Claude),
    );
    checks.push(
        Check::ok(
            CheckId::ManagedPolicy,
            "managed-policy deny lists (Claude Code managed settings) can block user servers, and d-mcp cannot see them",
        )
        .hint("if a synced server does not show up in Claude Code, ask your administrator about managed MCP policies")
        .harness(Harness::Claude),
    );
    if options.auth {
        warnings.push(AUTH_WARNING.to_owned());
        match &inventory {
            Some(inventory) => {
                let found = login_checks(inventory, env);
                or_ok(
                    &mut checks,
                    found,
                    CheckId::Login,
                    "no managed entry to query",
                );
            }
            None if store_ok => checks.push(Check::warn(
                CheckId::Login,
                "the login state was not queried, because the harness files could not be discovered (see the `discovery` check)",
            )),
            None => checks.push(Check::warn(
                CheckId::Login,
                "the login state was not queried, because the store cannot be read",
            )),
        }
    }
    Ok(Doctor {
        store: paths.root.clone(),
        repository: inventory.map(|inventory| inventory.repository),
        auth: options.auth,
        checks,
    })
}

/// Runs `doctor` and renders it; the error is `doctor-failed` when a check
/// is at `error`, with the report as `data`.
///
/// # Errors
///
/// See [`build`].
pub fn doctor(
    paths: &StorePaths,
    env: &Environment,
    options: &DoctorOptions,
    warnings: &mut Vec<String>,
) -> Result<(Report, Option<Error>)> {
    let doctor = build(paths, env, options, warnings)?;
    let errors = doctor.count(Severity::Error);
    let error = (errors > 0).then_some(Error::DoctorFailed { count: errors });
    Ok((
        Report::new(render::json(&doctor), render::text(&doctor)),
        error,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable() {
        assert_eq!(Severity::Warn.as_str(), "warn");
        assert_eq!(CheckId::RecurringVanish.as_str(), "recurring-vanish");
        assert_eq!(CheckId::HashKey.as_str(), "hash-key");
        assert!(Severity::Error > Severity::Warn && Severity::Warn > Severity::Ok);
    }

    /// A discovery that fails is an `error` check with its code; the checks
    /// that need the store only still run, and `--auth` says why nothing
    /// was queried.
    #[test]
    fn a_failed_discovery_is_an_error_check() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().join("store"));
        crate::store::init(&paths, false).unwrap();
        let env = Environment {
            home: Some(temp.path().join("home").into_os_string()),
            claude_config_dir: Some(temp.path().join("claude").into_os_string()),
            codex_home: Some(temp.path().join("codex").into_os_string()),
            path: Some(temp.path().join("empty-bin").into_os_string()),
            cwd: None,
            ..Environment::default()
        };
        let mut warnings = Vec::new();
        let doctor = build(&paths, &env, &DoctorOptions { auth: true }, &mut warnings).unwrap();
        let ids: Vec<&str> = doctor
            .checks
            .iter()
            .map(|check| check.check.as_str())
            .collect();
        assert!(!ids.contains(&"harness-config"), "{ids:?}");
        assert!(ids.contains(&"recurring-vanish"), "{ids:?}");
        assert!(ids.contains(&"static-token"), "{ids:?}");
        let discovery = doctor
            .checks
            .iter()
            .find(|check| check.check == CheckId::Discovery)
            .unwrap();
        assert_eq!(discovery.severity, Severity::Error);
        assert_eq!(discovery.code, Some("cwd-unavailable"));
        assert!(discovery.hint.is_some());
        let login = doctor
            .checks
            .iter()
            .find(|check| check.check == CheckId::Login)
            .unwrap();
        assert!(
            login.message.contains("could not be discovered"),
            "{}",
            login.message
        );
        assert_eq!(doctor.count(Severity::Error), 1);
    }
}
