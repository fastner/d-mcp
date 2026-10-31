//! Command-line definitions, dispatch and rendering.
//!
//! Mutating commands (`init`, `source add`, `source add-git`, `source
//! refresh`, `source trust`, `source remove`, `select`, `unselect`,
//! `approve`, `accept-risk`, `revoke`, `sync`, `unmanage`, `adopt --apply`,
//! `uninstall --apply`) take the store lock, then read `config.toml`, then
//! write; each creates a missing display-hash key (`.hash-key`) under the
//! lock. `source add-git` clones and `source refresh` fetches before they
//! take the lock. Read-only
//! commands (`resolve`, `source list`, `inventory`, `status`, `doctor`,
//! `adopt` and `uninstall` without `--apply`) and every `--dry-run` take no
//! lock and write nothing in the store.

mod approval;
mod source_git;

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::adopt::{AdoptOptions, EntryRef, Expect, Place, VarChoice, adopt};
use crate::config::{
    Config, Harness, HarnessSet, LOCAL_SOURCE_ID, Selection, SourceEntry, SourceOrigin,
};
use crate::definition::is_valid_name;
use crate::doctor::{DoctorOptions, doctor};
use crate::error::{Error, Result};
use crate::hashkey;
use crate::inventory::{InventoryOptions, inventory};
use crate::lockfile;
use crate::output::{Report, Ui};
use crate::resolver::ServerResolution;
use crate::source::git_source;
use crate::source::{
    ReadInputs, SourceSpec, is_available, load_resolution, offered_names, scan, source_specs,
};
use crate::status::{StatusOptions, status};
use crate::store::{self, Environment, StoreLock, StorePaths, StoreWarning};
use crate::sync::{SyncOptions, sync, unmanage};
use crate::uninstall::{UninstallOptions, uninstall};

/// Manage MCP server definitions once and sync them into every coding-agent
/// harness.
#[derive(Debug, Parser)]
#[command(
    name = "d-mcp",
    version,
    about,
    long_about = None,
    arg_required_else_help = true,
    after_help = "Examples:\n  d-mcp init\n  d-mcp source add ~/team-mcp --id team --priority 10\n  \
                  d-mcp source add-git git@github.com:me/mcp.git --id personal --trusted\n  \
                  d-mcp source refresh personal --dry-run\n  \
                  d-mcp approve github --expect <definition_hash>\n  \
                  d-mcp select github --harness claude\n  d-mcp select --all-from personal\n  \
                  d-mcp resolve --json\n  \
                  d-mcp inventory\n  d-mcp adopt claude:user:github codex:user:github\n  \
                  d-mcp sync --dry-run\n  d-mcp status --auth\n  d-mcp doctor\n  \
                  d-mcp unmanage github --harness codex\n  d-mcp uninstall"
)]
pub struct Cli {
    /// Store directory. Overrides `D_MCP_STORE` and the default `~/.d-mcp`.
    #[arg(long, global = true, value_name = "PATH")]
    pub store: Option<PathBuf>,
    /// Print one JSON document on stdout; warnings stay on stderr.
    #[arg(long, global = true)]
    pub json: bool,
    /// Show what a mutating command would do without taking the lock or
    /// writing anything.
    #[arg(long, global = true)]
    pub dry_run: bool,
    /// The command.
    #[command(subcommand)]
    pub command: Command,
}

/// The d-mcp commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the store, or complete a partial one. Idempotent.
    Init,
    /// Manage definition sources.
    Source {
        /// The source command.
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Select a server for harnesses (all harnesses by default).
    Select {
        /// The server name.
        #[arg(required_unless_present = "all_from")]
        server: Option<String>,
        /// Target harness; repeat for several. Without it, `all` is stored,
        /// which includes future adapters.
        #[arg(long = "harness", value_enum, value_name = "HARNESS")]
        harnesses: Vec<HarnessArg>,
        /// Select every server this source offers that is not selected yet,
        /// for all harnesses; existing selections are kept as they are.
        #[arg(long, value_name = "SOURCE", conflicts_with_all = ["server", "harnesses"])]
        all_from: Option<String>,
    },
    /// Approve a server's definition from a Git source, bound to its
    /// `definition_hash`; `--dry-run` shows the approval view and the hash
    /// to pass.
    Approve {
        /// The server name.
        #[arg(required_unless_present = "all_from")]
        server: Option<String>,
        /// The Git source; by default the source of the winning definition.
        #[arg(long, value_name = "SOURCE", conflicts_with = "all_from")]
        source: Option<String>,
        /// Approve every clean server of a trusted Git source; `--expect`
        /// then takes its pinned commit.
        #[arg(long, value_name = "SOURCE")]
        all_from: Option<String>,
        /// The `definition_hash` from the preview (the pinned commit with
        /// `--all-from`); required without `--dry-run`.
        #[arg(long, value_name = "HASH")]
        expect: Option<String>,
    },
    /// Accept the audit findings of a server's definition from a Git
    /// source, bound to its `definition_hash` and the findings; does not
    /// approve it.
    AcceptRisk {
        /// The server name.
        server: String,
        /// Why the risk is acceptable (at most 500 characters, no secrets;
        /// it is stored, never shown).
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// The `definition_hash` from the preview; required without
        /// `--dry-run`.
        #[arg(long, value_name = "HASH")]
        expect: Option<String>,
    },
    /// Revoke the approvals of a server from a Git source; the next sync
    /// removes its managed entries, and no lower definition takes its place.
    Revoke {
        /// The server name.
        server: String,
        /// The Git source; by default the source of the winning definition.
        #[arg(long, value_name = "SOURCE")]
        source: Option<String>,
    },
    /// Remove a selection.
    Unselect {
        /// The server name.
        server: String,
    },
    /// Resolve every selection against the sources (read-only).
    Resolve,
    /// List every MCP server entry of every harness scope d-mcp can read,
    /// classified against the write state, with duplicates and conflicts
    /// (read-only; entry content is redacted).
    Inventory {
        /// List only this harness; repeat for several. Without it, every
        /// harness with an adapter.
        #[arg(long = "harness", value_enum, value_name = "HARNESS")]
        harnesses: Vec<AdapterArg>,
    },
    /// Adopt existing harness entries as one d-mcp definition per name, in
    /// the `local` source: every env and header literal becomes a variable
    /// reference. Without `--apply` this is a read-only preview.
    Adopt {
        /// The entries, as `<harness>:<scope>:<name>` with the scope `user`
        /// or `project`; entries with the same name become one definition,
        /// selected for exactly their harnesses.
        #[arg(required = true, value_name = "HARNESS:SCOPE:NAME")]
        entries: Vec<EntryRef>,
        /// Write the definition and the selection and take the entries over.
        /// Needs an `--expect` per entry; with `--dry-run` it previews.
        #[arg(long)]
        apply: bool,
        /// The display hash of an entry from the preview; required for every
        /// entry with `--apply`.
        #[arg(long = "expect", value_name = "HARNESS:SCOPE:NAME=DISPLAY_HASH")]
        expect: Vec<Expect>,
        /// Keep this env literal as a value instead of a variable reference
        /// (env only, and only when it does not look like a secret).
        #[arg(long = "keep-literal", value_name = "NAME")]
        keep_literal: Vec<String>,
        /// The variable of a replaced header literal, by native field, such
        /// as `headers.Authorization=GITHUB_TOKEN`.
        #[arg(long = "var", value_name = "FIELD=NAME")]
        vars: Vec<VarChoice>,
        /// Adopt a project-scope entry into the user scope; the project entry
        /// is left as it is.
        #[arg(long)]
        to_user_scope: bool,
        /// The origin whose content wins when the harnesses' entries
        /// conflict, as `<harness>:<scope>`.
        #[arg(long, value_name = "HARNESS:SCOPE")]
        from: Option<Place>,
        /// Use OAuth instead of the static header.
        #[arg(long, conflicts_with = "oauth_supported")]
        prefer_oauth: bool,
        /// Keep the bearer token and declare that the server supports OAuth.
        #[arg(long)]
        oauth_supported: bool,
        /// Accept that rewriting an OAuth-capable Claude Code entry clears its
        /// stored tokens and client secret.
        #[arg(long)]
        accept_relogin: bool,
        /// The variable that holds the OAuth client secret (required when a
        /// rewritten Claude Code entry has `oauth.clientId`).
        #[arg(long, value_name = "NAME")]
        client_secret_env: Option<String>,
        /// Accept a variable whose value differs from the literal it
        /// replaces; the literal is unrecoverable after apply.
        #[arg(long)]
        allow_env_mismatch: bool,
        /// Accept a variable that is unset or empty; the literal is
        /// unrecoverable after apply.
        #[arg(long)]
        allow_missing_env: bool,
    },
    /// Write the selected servers into the harnesses (Codex and Claude
    /// Code), at user scope.
    Sync {
        /// Sync only this harness; repeat for several. Without it, every
        /// harness with an adapter is synced.
        #[arg(long = "harness", value_enum, value_name = "HARNESS")]
        harnesses: Vec<AdapterArg>,
        /// Accept the current harness entry of this server as managed
        /// (resolves Drift and Collision); repeat for several.
        #[arg(long = "take-over", value_name = "SERVER")]
        take_over: Vec<String>,
        /// Manage the recorded entries at the newly resolved harness file,
        /// for example after `CODEX_HOME` or `CLAUDE_CONFIG_DIR` changed. The
        /// old file is not edited.
        #[arg(long)]
        relocate: bool,
    },
    /// Show per harness and server the desired, recorded and actual state,
    /// the operation the next sync would run and the missing variables
    /// (read-only).
    Status {
        /// Also query the login state; for Claude Code this runs `claude mcp
        /// get`, which connects to each OAuth server.
        #[arg(long)]
        auth: bool,
        /// Only this harness; repeat for several. Without it, every harness
        /// with an adapter.
        #[arg(long = "harness", value_enum, value_name = "HARNESS")]
        harnesses: Vec<AdapterArg>,
        /// Only these servers. Without them, every selected or managed one.
        #[arg(value_name = "SERVER")]
        names: Vec<String>,
    },
    /// Check the store, the harnesses and the managed entries; exits 1 when
    /// a check is at `error` (read-only).
    Doctor {
        /// Also query the login state of managed entries; like `status
        /// --auth`, for Claude Code this connects to each OAuth server.
        #[arg(long)]
        auth: bool,
    },
    /// Stop managing every harness entry: remove the entries d-mcp created,
    /// release adopted ones, and leave drifted ones. Without `--apply` this
    /// is a read-only preview.
    Uninstall {
        /// Act: edit the harness configs and the state; with `--dry-run` it
        /// previews.
        #[arg(long)]
        apply: bool,
        /// Afterwards delete the known store files (config, state, `local/`,
        /// `sources/`, `.hash-key`, ...) inside the store root; this cannot
        /// be undone.
        #[arg(long)]
        purge: bool,
        /// With `--purge` on a store root that is a symlink: delete the
        /// store files inside its target.
        #[arg(long, requires = "purge")]
        follow_store_link: bool,
        /// With `--purge` when `local/` is a Git repository: delete it too,
        /// unpushed work included.
        #[arg(long, requires = "purge")]
        include_local_repo: bool,
    },
    /// Stop managing a server's harness entries and drop those harnesses
    /// from its selection. The entries are left as they are.
    Unmanage {
        /// The server name.
        server: String,
        /// Only this harness; repeat for several. Without it, every harness
        /// with a record.
        #[arg(long = "harness", value_enum, value_name = "HARNESS")]
        harnesses: Vec<AdapterArg>,
    },
}

/// A harness with an adapter, on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AdapterArg {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl From<AdapterArg> for Harness {
    fn from(arg: AdapterArg) -> Self {
        match arg {
            AdapterArg::Claude => Harness::Claude,
            AdapterArg::Codex => Harness::Codex,
        }
    }
}

/// The `source` subcommands.
#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    /// Register a local directory with `servers/<name>.toml` files.
    Add {
        /// The source root; `~` and relative paths are resolved and stored
        /// canonically.
        path: PathBuf,
        /// The source id (neutral name rule).
        #[arg(long)]
        id: String,
        /// Priority, at least 1 and unique. Higher wins; `local` has 0.
        #[arg(long)]
        priority: u32,
    },
    /// Register a Git repository with `servers/<name>.toml` files: clone it
    /// into the store, pin a commit and snapshot its definitions.
    AddGit {
        /// The remote: `https://...`, `ssh://...`, `git@host:org/repo.git`,
        /// `file:///...` or an absolute path; no password, query or fragment.
        url: String,
        /// The source id (neutral name rule); derived from the URL's last
        /// path segment by default.
        #[arg(long)]
        id: Option<String>,
        /// The branch or tag to follow; the remote's default branch by
        /// default.
        #[arg(long = "ref", value_name = "REF")]
        reference: Option<String>,
        /// Pin this commit (40 or 64 hex characters) instead of the head of
        /// the ref; it must be reachable from the ref.
        #[arg(long, value_name = "SHA")]
        commit: Option<String>,
        /// Priority, at least 1 and unique; one above the highest user
        /// source by default.
        #[arg(long)]
        priority: Option<u32>,
        /// Trust the source, which allows `approve --all-from`.
        #[arg(long)]
        trusted: bool,
    },
    /// Fetch a Git source's ref, show the change set, and move the pin
    /// (a moved pin never approves anything).
    Refresh {
        /// The source id.
        id: String,
        /// Accept a rewritten ref (force-push or moved tag) or a missing pin.
        #[arg(long)]
        allow_rewrite: bool,
    },
    /// Mark a Git source trusted or untrusted; trust only allows `approve
    /// --all-from`.
    #[command(group(clap::ArgGroup::new("state").required(true).args(["on", "off"])))]
    Trust {
        /// The source id.
        id: String,
        /// Trust the source.
        #[arg(long)]
        on: bool,
        /// Stop trusting the source.
        #[arg(long)]
        off: bool,
    },
    /// List the sources, including the built-in `local` source (read-only).
    List,
    /// Unregister a source; a Git source's clone, snapshot and pin are
    /// deleted. The built-in `local` source cannot be removed.
    Remove {
        /// The source id.
        id: String,
    },
}

/// A harness on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HarnessArg {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl From<HarnessArg> for Harness {
    fn from(arg: HarnessArg) -> Self {
        match arg {
            HarnessArg::Claude => Harness::Claude,
            HarnessArg::Codex => Harness::Codex,
        }
    }
}

/// Runs a parsed command line and returns the process exit code.
pub fn execute(cli: &Cli, env: &Environment, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let mut ui = Ui::new(cli.json, out, err);
    match dispatch(cli, env, &mut ui) {
        Ok((report, error)) => ui.finish(&report, error.as_ref()),
        Err(error) => ui.failure(&error),
    }
}

fn dispatch(cli: &Cli, env: &Environment, ui: &mut Ui<'_>) -> Result<(Report, Option<Error>)> {
    let command = &cli.command;
    let read_only = matches!(
        command,
        Command::Resolve
            | Command::Inventory { .. }
            | Command::Status { .. }
            | Command::Doctor { .. }
            | Command::Source {
                command: SourceCommand::List
            }
    );
    if read_only && cli.dry_run {
        ui.warn("--dry-run has no effect on read-only commands");
    }
    let paths = StorePaths::new(env.store_root(cli.store.as_deref())?);
    let dry_run = cli.dry_run;
    let report = match command {
        Command::Sync {
            harnesses,
            take_over,
            relocate,
        } => {
            let options = SyncOptions {
                dry_run,
                harnesses: harnesses.iter().copied().map(Harness::from).collect(),
                take_over: take_over.iter().cloned().collect(),
                relocate: *relocate,
                ..SyncOptions::default()
            };
            let mut warnings = Vec::new();
            let result = sync(&paths, env, &options, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            return result;
        }
        Command::Adopt {
            entries,
            apply,
            expect,
            keep_literal,
            vars,
            to_user_scope,
            from,
            prefer_oauth,
            oauth_supported,
            accept_relogin,
            client_secret_env,
            allow_env_mismatch,
            allow_missing_env,
        } => {
            let options = AdoptOptions {
                entries: entries.clone(),
                apply: *apply,
                dry_run,
                keep_literal: keep_literal.iter().cloned().collect(),
                vars: vars.clone(),
                to_user_scope: *to_user_scope,
                from: *from,
                prefer_oauth: *prefer_oauth,
                oauth_supported: *oauth_supported,
                accept_relogin: *accept_relogin,
                client_secret_env: client_secret_env.clone(),
                expect: expect.clone(),
                allow_env_mismatch: *allow_env_mismatch,
                allow_missing_env: *allow_missing_env,
                var: None,
            };
            let mut warnings = Vec::new();
            let result = adopt(&paths, env, &options, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            return result;
        }
        Command::Status {
            auth,
            harnesses,
            names,
        } => {
            let options = StatusOptions {
                harnesses: harnesses.iter().copied().map(Harness::from).collect(),
                names: names.iter().cloned().collect(),
                auth: *auth,
                var: None,
            };
            let mut warnings = Vec::new();
            let result = status(&paths, env, &options, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            result
        }
        Command::Doctor { auth } => {
            let mut warnings = Vec::new();
            let result = doctor(&paths, env, &DoctorOptions { auth: *auth }, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            return result;
        }
        Command::Uninstall {
            apply,
            purge,
            follow_store_link,
            include_local_repo,
        } => {
            let options = UninstallOptions {
                apply: *apply,
                dry_run,
                purge: *purge,
                follow_store_link: *follow_store_link,
                include_local_repo: *include_local_repo,
            };
            let mut warnings = Vec::new();
            let result = uninstall(&paths, env, &options, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            return result;
        }
        Command::Unmanage { server, harnesses } => {
            let harnesses = harnesses.iter().copied().map(Harness::from).collect();
            let mut warnings = Vec::new();
            let result = unmanage(&paths, server, &harnesses, dry_run, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            result
        }
        Command::Inventory { harnesses } => {
            let options = InventoryOptions {
                harnesses: harnesses.iter().copied().map(Harness::from).collect(),
            };
            let mut warnings = Vec::new();
            let result = inventory(&paths, env, &options, &mut warnings);
            for warning in &warnings {
                ui.warn(warning);
            }
            result
        }
        Command::Init => init(&paths, ui, dry_run),
        Command::Source { command } => match command {
            SourceCommand::Add { path, id, priority } => {
                source_add(&paths, env, ui, path, id, *priority, dry_run)
            }
            SourceCommand::AddGit {
                url,
                id,
                reference,
                commit,
                priority,
                trusted,
            } => source_git::add_git(
                &paths,
                env,
                ui,
                &git_source::AddGitOptions {
                    url,
                    id: id.as_deref(),
                    reference: reference.as_deref(),
                    commit: commit.as_deref(),
                    priority: *priority,
                    trusted: *trusted,
                    dry_run,
                },
            ),
            SourceCommand::Refresh { id, allow_rewrite } => source_git::refresh(
                &paths,
                env,
                ui,
                &crate::source::refresh::RefreshOptions {
                    id,
                    allow_rewrite: *allow_rewrite,
                    dry_run,
                },
            ),
            SourceCommand::Trust { id, on, off: _ } => {
                approval::trust(&paths, ui, id, *on, dry_run)
            }
            SourceCommand::List => source_list(&paths),
            SourceCommand::Remove { id } => source_remove(&paths, ui, id, dry_run),
        },
        Command::Select {
            server,
            harnesses,
            all_from,
        } => match (all_from, server) {
            (Some(id), _) => approval::select_all_from(&paths, ui, id, dry_run),
            (None, Some(server)) => select(&paths, ui, server, harnesses, dry_run),
            (None, None) => unreachable!("clap requires a server or --all-from"),
        },
        Command::Approve {
            server,
            source,
            all_from,
            expect,
        } => {
            return match (all_from, server) {
                (Some(id), _) => approval::approve_all(&paths, ui, id, expect.as_deref(), dry_run),
                (None, Some(server)) => approval::approve(
                    &paths,
                    ui,
                    server,
                    source.as_deref(),
                    expect.as_deref(),
                    dry_run,
                ),
                (None, None) => unreachable!("clap requires a server or --all-from"),
            };
        }
        Command::AcceptRisk {
            server,
            reason,
            expect,
        } => approval::accept_risk(&paths, ui, server, reason, expect.as_deref(), dry_run),
        Command::Revoke { server, source } => {
            approval::revoke(&paths, ui, server, source.as_deref(), dry_run)
        }
        Command::Unselect { server } => unselect(&paths, ui, server, dry_run),
        Command::Resolve => resolve_command(&paths),
    }?;
    Ok((report, None))
}

/// Runs a mutation: fails on an uninitialized store before locking, then
/// takes the lock (unless dry run), reads `config.toml`, applies `change`
/// and writes the result atomically (unless dry run). A write warning goes to
/// stderr.
fn mutate<T>(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    dry_run: bool,
    change: impl FnOnce(&mut Config) -> Result<T>,
) -> Result<T> {
    mutate_then(
        paths,
        ui,
        dry_run,
        |config| change(config).map(|value| (value, ())),
        |_, ()| Ok(Vec::new()),
    )
}

/// [`mutate`] with a second step that runs under the lock after
/// `config.toml` was written (never in a dry run), with the second value
/// `change` returned.
fn mutate_then<T, U>(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    dry_run: bool,
    change: impl FnOnce(&mut Config) -> Result<(T, U)>,
    after: impl FnOnce(&StoreLock, U) -> Result<Vec<StoreWarning>>,
) -> Result<T> {
    paths.require_initialized()?;
    let lock = if dry_run {
        None
    } else {
        Some(StoreLock::acquire(paths)?)
    };
    let mut config = paths.load_config()?;
    // Every mutating command creates a missing display-hash key.
    if let Some(lock) = &lock
        && let Some(warning) = hashkey::ensure_under_lock(paths, lock)?
    {
        ui.warn(&warning.to_string());
    }
    let (result, rest) = change(&mut config)?;
    if let Some(lock) = &lock {
        if let Some(warning) = paths.save_config(lock, &config)? {
            ui.warn(&warning.to_string());
        }
        for warning in after(lock, rest)? {
            ui.warn(&warning.to_string());
        }
    }
    Ok(result)
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn prefix(dry_run: bool) -> &'static str {
    if dry_run { "Dry run: would have " } else { "" }
}

fn source_json(spec: &SourceSpec) -> Value {
    source_json_with(spec, is_available(spec))
}

/// The source object; `url`, `ref`, `commit` and `trusted` are `null` for a
/// directory source.
fn source_json_with(spec: &SourceSpec, available: bool) -> Value {
    let git = spec.git.as_ref();
    json!({
        "id": spec.id,
        "kind": spec.kind.as_str(),
        "path": display(spec.location()),
        "priority": spec.priority,
        "builtin": spec.builtin,
        "available": available,
        "url": git.map(|git| git.url.as_str()),
        "ref": git.map(|git| git.reference.as_str()),
        "commit": git.and_then(|git| git.pin.as_ref().map(|pin| pin.as_str())),
        "trusted": git.map(|git| git.trusted),
    })
}

/// The spec of a registered user source.
fn user_spec(paths: &StorePaths, entry: &SourceEntry, pins: &lockfile::LockFile) -> SourceSpec {
    let config = Config {
        sources: vec![entry.clone()],
        selections: Vec::new(),
    };
    source_specs(paths, &config, pins)
        .into_iter()
        .find(|spec| spec.id == entry.id)
        .unwrap_or_else(|| unreachable!("source_specs lists every user source"))
}

fn init(paths: &StorePaths, ui: &mut Ui<'_>, dry_run: bool) -> Result<Report> {
    let report = store::init(paths, dry_run)?;
    for warning in &report.warnings {
        ui.warn(&warning.to_string());
    }
    let created: Vec<String> = report.created.iter().map(|path| display(path)).collect();
    let text = if created.is_empty() {
        format!(
            "The store at {} is already initialized.",
            display(&report.store)
        )
    } else {
        let verb = if dry_run { "Would create" } else { "Created" };
        let mut text = format!("{verb} in the store at {}:", display(&report.store));
        for path in &created {
            text.push_str(&format!("\n  {path}"));
        }
        text
    };
    Ok(Report::new(
        json!({ "store": display(&report.store), "created": created, "dry_run": dry_run }),
        text,
    ))
}

fn source_add(
    paths: &StorePaths,
    env: &Environment,
    ui: &mut Ui<'_>,
    path: &Path,
    id: &str,
    priority: u32,
    dry_run: bool,
) -> Result<Report> {
    paths.require_initialized()?;
    if id == LOCAL_SOURCE_ID {
        return Err(Error::SourceIdReserved);
    }
    if !is_valid_name(id) {
        return Err(Error::InvalidName { what: "source id" });
    }
    if priority == 0 {
        return Err(Error::PriorityInvalid);
    }
    let absolute = env.absolute(path)?;
    let canonical = fs::canonicalize(&absolute).map_err(|_| Error::SourcePathNotFound {
        path: absolute.clone(),
    })?;
    if !canonical.is_dir() {
        return Err(Error::SourcePathNotDirectory { path: canonical });
    }
    if canonical.to_str().is_none() {
        return Err(Error::SourcePathNotUtf8);
    }
    let store_root =
        fs::canonicalize(&paths.root).map_err(|error| Error::io("resolve", &paths.root, &error))?;
    if canonical.starts_with(&store_root) {
        return Err(Error::SourcePathInStore { path: canonical });
    }
    let entry = SourceEntry {
        id: id.to_owned(),
        priority,
        origin: SourceOrigin::Directory { path: canonical },
    };
    let added = entry.clone();
    mutate(paths, ui, dry_run, move |config| {
        if config.source(&entry.id).is_some() {
            return Err(Error::SourceIdTaken { id: entry.id });
        }
        if let Some(other) = config.sources.iter().find(|s| s.priority == entry.priority) {
            return Err(Error::PriorityTaken {
                priority: entry.priority,
                id: other.id.clone(),
            });
        }
        if let (SourceOrigin::Directory { path }, Some(other)) = (
            &entry.origin,
            config.sources.iter().find(|s| s.path() == entry.path()),
        ) {
            return Err(Error::SourcePathRegistered {
                path: path.clone(),
                id: other.id.clone(),
            });
        }
        config.sources.push(entry);
        Ok(())
    })?;
    let spec = user_spec(paths, &added, &lockfile::LockFile::default());
    if !is_available(&spec) {
        ui.warn(&format!(
            "source `{}` has no readable servers/ directory; it is unavailable until one exists",
            added.id
        ));
    }
    let text = format!(
        "{}{} source `{}` (priority {}): {}",
        prefix(dry_run),
        if dry_run { "added" } else { "Added" },
        added.id,
        added.priority,
        display(spec.location())
    );
    Ok(Report::new(
        json!({ "source": source_json(&spec), "dry_run": dry_run }),
        text,
    ))
}

fn source_list(paths: &StorePaths) -> Result<Report> {
    let config = paths.load_config()?;
    let pins = lockfile::load(paths)?;
    let mut specs = source_specs(paths, &config, &pins);
    specs.sort_by_key(|spec| std::cmp::Reverse(spec.priority));
    let sources: Vec<Value> = specs.iter().map(source_json).collect();
    let mut text = String::from("ID\tPRIORITY\tKIND\tSTATUS\tPATH");
    for source in &sources {
        text.push_str(&format!(
            "\n{}\t{}\t{}\t{}\t{}",
            source["id"].as_str().unwrap_or_default(),
            source["priority"],
            source["kind"].as_str().unwrap_or_default(),
            if source["available"] == true {
                "available"
            } else {
                "unavailable"
            },
            source["path"].as_str().unwrap_or_default(),
        ));
    }
    Ok(Report::new(json!({ "sources": sources }), text))
}

fn source_remove(paths: &StorePaths, ui: &mut Ui<'_>, id: &str, dry_run: bool) -> Result<Report> {
    if id == LOCAL_SOURCE_ID {
        return Err(Error::SourceIdReserved);
    }
    let (spec, lost) = mutate_then(
        paths,
        ui,
        dry_run,
        |config| {
            // Loaded before `config.toml` is written, so a corrupt
            // `lock.toml` fails the command before anything changes.
            let pins = lockfile::load(paths)?;
            let Some(position) = config.sources.iter().position(|s| s.id == id) else {
                return Err(Error::SourceNotFound { id: id.to_owned() });
            };
            let removed = config.sources.remove(position);
            let spec = user_spec(paths, &removed, &pins);
            let removed_names: BTreeSet<String> = offered_names(&scan(&spec)).into_iter().collect();
            let remaining: BTreeSet<String> = source_specs(paths, config, &pins)
                .iter()
                .flat_map(|spec| offered_names(&scan(spec)))
                .collect();
            let lost: Vec<String> = config
                .selections
                .iter()
                .map(|selection| selection.server.clone())
                .filter(|server| removed_names.contains(server) && !remaining.contains(server))
                .collect();
            let git = removed.git().is_some();
            Ok(((spec, lost), (git, pins)))
        },
        |lock, (git, pins)| {
            // A Git source's clone, snapshot and pin go; its approvals stay
            // and are reported as orphaned.
            if git {
                git_source::forget(paths, lock, id, pins)
            } else {
                Ok(Vec::new())
            }
        },
    )?;
    for server in &lost {
        ui.warn(&format!(
            "selected server `{server}` is no longer offered by any source"
        ));
    }
    let text = format!(
        "{}{} source `{}`",
        prefix(dry_run),
        if dry_run { "removed" } else { "Removed" },
        spec.id
    );
    Ok(Report::new(
        json!({ "source": source_json(&spec), "no_longer_offered": lost, "dry_run": dry_run }),
        text,
    ))
}

fn select(
    paths: &StorePaths,
    ui: &mut Ui<'_>,
    server: &str,
    harnesses: &[HarnessArg],
    dry_run: bool,
) -> Result<Report> {
    paths.require_initialized()?;
    if !is_valid_name(server) {
        return Err(Error::InvalidName {
            what: "server name",
        });
    }
    let harnesses = if harnesses.is_empty() {
        HarnessSet::All
    } else {
        HarnessSet::Only(harnesses.iter().copied().map(Harness::from).collect())
    };
    let selection = Selection {
        server: server.to_owned(),
        harnesses: harnesses.clone(),
    };
    let (replaced, offered) = mutate(paths, ui, dry_run, |config| {
        let pins = lockfile::load(paths)?;
        let offered = source_specs(paths, config, &pins)
            .iter()
            .any(|spec| offered_names(&scan(spec)).iter().any(|name| name == server));
        Ok((config.select(selection), offered))
    })?;
    if !offered {
        ui.warn(&format!(
            "no source offers `{server}` yet; it is selected anyway"
        ));
    }
    let names = harnesses.names();
    let text = format!(
        "{}{} `{server}` for {}",
        prefix(dry_run),
        if dry_run { "selected" } else { "Selected" },
        names.join(", ")
    );
    Ok(Report::new(
        json!({
            "server": server,
            "harnesses": names,
            "offered": offered,
            "replaced": replaced,
            "dry_run": dry_run,
        }),
        text,
    ))
}

fn unselect(paths: &StorePaths, ui: &mut Ui<'_>, server: &str, dry_run: bool) -> Result<Report> {
    mutate(paths, ui, dry_run, |config| {
        if config.unselect(server) {
            Ok(())
        } else {
            Err(Error::NotSelected {
                server: server.to_owned(),
            })
        }
    })?;
    let text = format!(
        "{}{} `{server}`",
        prefix(dry_run),
        if dry_run { "unselected" } else { "Unselected" }
    );
    Ok(Report::new(
        json!({ "server": server, "dry_run": dry_run }),
        text,
    ))
}

/// The limited `resolve` view of a server: no literal values, no `args`, no
/// override contents. A winner from a Git source also carries its
/// `definition_hash` and its audit findings while it is held or revoked by
/// the gate; the findings' matched arguments go through the approval view's
/// exception for Git-source definitions.
fn server_json(server: &ServerResolution) -> Value {
    let definition = server.definition.as_ref();
    let gated = server.gated.as_ref();
    json!({
        "name": server.name,
        "status": server.status,
        "reason": server.reason,
        "source": server.source,
        "file": server.file,
        "harnesses": server.harnesses,
        "transport": definition.map(|d| d.transport_kind()),
        "auth": definition.and_then(|d| d.auth_kind()),
        "forwarded_env": definition.map(|d| d.forwarded_env()),
        "definition_hash": definition
            .map(|d| d.definition_hash())
            .or_else(|| gated.map(|gated| gated.definition_hash.clone())),
        "core_hash": definition.map(|d| d.core_hash()),
        "findings": gated.map(|gated| &gated.findings),
    })
}

fn resolve_command(paths: &StorePaths) -> Result<Report> {
    let config = paths.load_config()?;
    let inputs = ReadInputs::load(paths)?;
    let resolution = load_resolution(paths, &config, &inputs);
    let servers: Vec<Value> = resolution.servers.iter().map(server_json).collect();
    let mut text = String::new();
    if servers.is_empty() {
        text.push_str("No servers are selected.");
    }
    for (index, server) in servers.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let status = match server["reason"].as_str() {
            Some(reason) => format!("hold ({reason})"),
            None => server["status"].as_str().unwrap_or_default().to_owned(),
        };
        text.push_str(&format!(
            "{}\t{status}\tsource={}\tharnesses={}",
            server["name"].as_str().unwrap_or_default(),
            server["source"].as_str().unwrap_or("-"),
            server["harnesses"]
                .as_array()
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
        ));
        if let Some(transport) = server["transport"].as_str() {
            text.push_str(&format!("\ttransport={transport}"));
        }
        if let Some(auth) = server["auth"].as_str() {
            text.push_str(&format!("\tauth={auth}"));
        }
    }
    Ok(Report {
        data: json!({ "store": display(&paths.root), "servers": servers }),
        problems: resolution.problems,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct Fixture {
        temp: tempfile::TempDir,
        env: Environment,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let work = temp.path().join("work");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&work).unwrap();
            let env = Environment {
                home: Some(OsString::from(&home)),
                store: Some(OsString::from(temp.path().join("store"))),
                cwd: Some(work),
                ..Environment::default()
            };
            Self { temp, env }
        }

        fn run(&self, args: &[&str]) -> (u8, String, String) {
            let cli =
                Cli::try_parse_from(std::iter::once("d-mcp").chain(args.iter().copied())).unwrap();
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = execute(&cli, &self.env, &mut out, &mut err);
            (
                code,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        }

        fn json(&self, args: &[&str]) -> (u8, Value) {
            let mut all = vec!["--json"];
            all.extend_from_slice(args);
            let (code, out, _) = self.run(&all);
            (code, serde_json::from_str(&out).unwrap())
        }

        fn source_dir(&self, name: &str) -> PathBuf {
            let dir = self.temp.path().join(name);
            fs::create_dir_all(dir.join("servers")).unwrap();
            dir
        }
    }

    const GITHUB: &str = "name = \"github\"\n[transport]\ntype = \"stdio\"\ncommand = \"npx\"\n";

    #[test]
    fn full_flow_in_process() {
        let f = Fixture::new();
        let (code, out, _) = f.run(&["init"]);
        assert_eq!(code, 0, "{out}");
        assert!(out.starts_with("Created in the store at"));
        let (code, out, _) = f.run(&["init"]);
        assert_eq!(code, 0);
        assert!(out.contains("already initialized"));

        fs::write(
            f.temp.path().join("store/local/servers/github.toml"),
            GITHUB,
        )
        .unwrap();
        let team = f.source_dir("team");
        fs::write(team.join("servers/github.toml"), GITHUB).unwrap();
        let team_arg = team.to_str().unwrap();
        let (code, out, err) =
            f.run(&["source", "add", team_arg, "--id", "team", "--priority", "5"]);
        assert_eq!(code, 0, "{err}");
        assert!(out.starts_with("Added source `team` (priority 5)"));

        let (code, out, _) = f.run(&["source", "list"]);
        assert_eq!(code, 0);
        assert!(out.contains("team\t5\tdirectory\tavailable"));
        assert!(out.contains("local\t0\tdirectory\tavailable"));

        let (code, out, _) = f.run(&["resolve"]);
        assert_eq!(code, 0);
        assert_eq!(
            out.trim(),
            "No servers are selected.\nproblems:\n  shadowed: servers/github.toml in source `local` is shadowed by servers/github.toml in source `team`"
        );

        let (code, out, _) = f.run(&["select", "github", "--harness", "codex"]);
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "Selected `github` for codex");
        let (_, out, _) = f.run(&["resolve"]);
        assert!(
            out.starts_with("github\tresolved\tsource=team\tharnesses=codex\ttransport=stdio\n")
        );

        let (code, out, err) = f.run(&["select", "nothing"]);
        assert_eq!(code, 0);
        assert!(out.contains("Selected `nothing` for all"));
        assert!(err.contains("no source offers `nothing`"));
        let (_, out, _) = f.run(&["resolve"]);
        assert!(out.contains("nothing\tnot-offered\tsource=-\tharnesses=all"));

        let (code, out, _) = f.run(&["--dry-run", "unselect", "nothing"]);
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "Dry run: would have unselected `nothing`");
        let (code, _, _) = f.run(&["unselect", "nothing"]);
        assert_eq!(code, 0);
        let (code, _, err) = f.run(&["unselect", "nothing"]);
        assert_eq!(code, 1);
        assert!(err.contains("[not-selected]"));

        let (code, out, _) = f.run(&["source", "remove", "team"]);
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "Removed source `team`");
    }

    #[test]
    fn http_server_text_shows_auth_and_holds_show_reason() {
        let f = Fixture::new();
        f.run(&["init"]);
        fs::write(
            f.temp.path().join("store/local/servers/linear.toml"),
            "name = \"linear\"\n[transport]\ntype = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n",
        )
        .unwrap();
        fs::write(f.temp.path().join("store/local/servers/bad.toml"), "x").unwrap();
        f.run(&["select", "linear"]);
        f.run(&["select", "bad"]);
        let (_, out, _) = f.run(&["resolve"]);
        assert!(out.contains("bad\thold (invalid-winner)\tsource=local"));
        assert!(
            out.contains(
                "linear\tresolved\tsource=local\tharnesses=all\ttransport=http\tauth=oauth"
            )
        );
        assert!(out.contains("(line 1, column 2)"));
    }

    #[test]
    fn dry_runs_write_nothing() {
        let f = Fixture::new();
        let (code, value) = f.json(&["--dry-run", "init"]);
        assert_eq!(code, 0);
        assert_eq!(value["data"]["created"].as_array().unwrap().len(), 6);
        assert!(!f.temp.path().join("store").exists());
        let (code, out, _) = f.run(&["--dry-run", "init"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("Would create in the store at"));
        f.run(&["init"]);
        let team = f.source_dir("team");
        let (code, out, _) = f.run(&[
            "--dry-run",
            "source",
            "add",
            team.to_str().unwrap(),
            "--id",
            "team",
            "--priority",
            "3",
        ]);
        assert_eq!(code, 0);
        assert!(out.starts_with("Dry run: would have added source `team`"));
        let (code, out, _) = f.run(&["--dry-run", "select", "github"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("Dry run: would have selected"));
        let config = fs::read_to_string(f.temp.path().join("store/config.toml")).unwrap();
        assert_eq!(config, "schema_version = 1\n");
    }

    #[test]
    fn read_only_dry_run_warns() {
        let f = Fixture::new();
        f.run(&["init"]);
        let (code, out, err) = f.run(&["--json", "--dry-run", "source", "list"]);
        assert_eq!(code, 0);
        assert!(err.contains("--dry-run has no effect"));
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["data"]["sources"][0]["id"], "local");
    }

    #[test]
    fn rejections() {
        let f = Fixture::new();
        let (code, _, err) = f.run(&["resolve"]);
        assert_eq!(code, 1);
        assert!(err.contains("[store-not-initialized]"));
        f.run(&["init"]);
        let team = f.source_dir("team");
        let team = team.to_str().unwrap();
        let add = |id: &str, priority: &str, path: &str| {
            f.json(&["source", "add", path, "--id", id, "--priority", priority])
        };
        assert_eq!(
            add("local", "1", team).1["error"]["code"],
            "source-id-reserved"
        );
        assert_eq!(add("Team", "1", team).1["error"]["code"], "invalid-name");
        assert_eq!(
            add("team", "0", team).1["error"]["code"],
            "priority-invalid"
        );
        assert_eq!(
            add("team", "1", "/nonexistent/x").1["error"]["code"],
            "source-path-not-found"
        );
        let file = f.temp.path().join("file.txt");
        fs::write(&file, "x").unwrap();
        assert_eq!(
            add("team", "1", file.to_str().unwrap()).1["error"]["code"],
            "source-path-not-directory"
        );
        let inside = f.temp.path().join("store/local");
        assert_eq!(
            add("team", "1", inside.to_str().unwrap()).1["error"]["code"],
            "source-path-in-store"
        );
        assert_eq!(add("team", "1", team).0, 0);
        assert_eq!(add("team", "2", team).1["error"]["code"], "source-id-taken");
        assert_eq!(add("other", "1", team).1["error"]["code"], "priority-taken");
        assert_eq!(
            add("other", "2", team).1["error"]["code"],
            "source-path-registered"
        );
        assert_eq!(
            f.json(&["source", "remove", "local"]).1["error"]["code"],
            "source-id-reserved"
        );
        assert_eq!(
            f.json(&["source", "remove", "nope"]).1["error"]["code"],
            "source-not-found"
        );
        assert_eq!(
            f.json(&["select", "Bad__name"]).1["error"]["code"],
            "invalid-name"
        );
    }
}
