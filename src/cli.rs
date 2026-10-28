//! Command-line definitions, dispatch and rendering.
//!
//! Mutating commands (`init`, `source add`, `source remove`, `select`,
//! `unselect`, `sync`, `unmanage`) take the store lock, then read
//! `config.toml`, then write. Read-only commands (`resolve`, `source list`)
//! and every `--dry-run` take no lock and write nothing.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::adapter::registry;
use crate::config::{
    Config, Harness, HarnessSet, LOCAL_SOURCE_ID, Selection, SourceEntry, SourceKind,
};
use crate::definition::is_valid_name;
use crate::error::{Error, Result};
use crate::output::{Report, Ui};
use crate::resolver::{ServerResolution, resolve};
use crate::source::{SourceSpec, is_available, offered_names, scan, source_specs};
use crate::store::{self, Environment, StoreLock, StorePaths};
use crate::sync::{SyncOptions, sync, unmanage};
use crate::validate::validate;

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
                  d-mcp select github --harness claude\n  d-mcp resolve --json\n  \
                  d-mcp sync --dry-run\n  d-mcp unmanage github --harness codex"
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
        server: String,
        /// Target harness; repeat for several. Without it, `all` is stored,
        /// which includes future adapters.
        #[arg(long = "harness", value_enum, value_name = "HARNESS")]
        harnesses: Vec<HarnessArg>,
    },
    /// Remove a selection.
    Unselect {
        /// The server name.
        server: String,
    },
    /// Resolve every selection against the sources (read-only).
    Resolve,
    /// Write the selected servers into the harnesses (Codex in this
    /// version), at user scope.
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
        /// for example after `CODEX_HOME` changed. The old file is not
        /// edited.
        #[arg(long)]
        relocate: bool,
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
    /// Codex.
    Codex,
}

impl From<AdapterArg> for Harness {
    fn from(arg: AdapterArg) -> Self {
        match arg {
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
    /// List the sources, including the built-in `local` source (read-only).
    List,
    /// Unregister a source. The built-in `local` source cannot be removed.
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
        Command::Unmanage { server, harnesses } => {
            let harnesses = harnesses.iter().copied().map(Harness::from).collect();
            let mut warnings = Vec::new();
            let result = unmanage(&paths, server, &harnesses, dry_run, &mut warnings);
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
            SourceCommand::List => source_list(&paths),
            SourceCommand::Remove { id } => source_remove(&paths, ui, id, dry_run),
        },
        Command::Select { server, harnesses } => select(&paths, ui, server, harnesses, dry_run),
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
    paths.require_initialized()?;
    let lock = if dry_run {
        None
    } else {
        Some(StoreLock::acquire(paths)?)
    };
    let mut config = paths.load_config()?;
    let result = change(&mut config)?;
    if let Some(lock) = &lock
        && let Some(warning) = paths.save_config(lock, &config)?
    {
        ui.warn(&warning.to_string());
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
    json!({
        "id": spec.id,
        "kind": spec.kind.as_str(),
        "path": display(&spec.root),
        "priority": spec.priority,
        "builtin": spec.builtin,
        "available": is_available(spec),
    })
}

fn user_spec(entry: &SourceEntry) -> SourceSpec {
    SourceSpec {
        id: entry.id.clone(),
        kind: entry.kind,
        root: entry.path.clone(),
        priority: entry.priority,
        builtin: false,
    }
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
        kind: SourceKind::Directory,
        path: canonical,
        priority,
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
        if let Some(other) = config.sources.iter().find(|s| s.path == entry.path) {
            return Err(Error::SourcePathRegistered {
                path: entry.path,
                id: other.id.clone(),
            });
        }
        config.sources.push(entry);
        Ok(())
    })?;
    let spec = user_spec(&added);
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
        display(&added.path)
    );
    Ok(Report::new(
        json!({ "source": source_json(&spec), "dry_run": dry_run }),
        text,
    ))
}

fn sorted_specs(paths: &StorePaths, config: &Config) -> Vec<SourceSpec> {
    let mut specs = source_specs(paths, config);
    specs.sort_by_key(|spec| std::cmp::Reverse(spec.priority));
    specs
}

fn source_list(paths: &StorePaths) -> Result<Report> {
    let config = paths.load_config()?;
    let specs = sorted_specs(paths, &config);
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
    let (removed, lost) = mutate(paths, ui, dry_run, |config| {
        let Some(position) = config.sources.iter().position(|s| s.id == id) else {
            return Err(Error::SourceNotFound { id: id.to_owned() });
        };
        let removed = config.sources.remove(position);
        let removed_names: BTreeSet<String> = offered_names(&scan(&user_spec(&removed)))
            .into_iter()
            .collect();
        let remaining: BTreeSet<String> = source_specs(paths, config)
            .iter()
            .flat_map(|spec| offered_names(&scan(spec)))
            .collect();
        let lost: Vec<String> = config
            .selections
            .iter()
            .map(|selection| selection.server.clone())
            .filter(|server| removed_names.contains(server) && !remaining.contains(server))
            .collect();
        Ok((removed, lost))
    })?;
    for server in &lost {
        ui.warn(&format!(
            "selected server `{server}` is no longer offered by any source"
        ));
    }
    let spec = user_spec(&removed);
    let text = format!(
        "{}{} source `{}`",
        prefix(dry_run),
        if dry_run { "removed" } else { "Removed" },
        removed.id
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
        let offered = source_specs(paths, config)
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
/// override contents.
fn server_json(server: &ServerResolution) -> Value {
    let definition = server.definition.as_ref();
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
        "definition_hash": definition.map(|d| d.definition_hash()),
        "core_hash": definition.map(|d| d.core_hash()),
    })
}

fn resolve_command(paths: &StorePaths) -> Result<Report> {
    let config = paths.load_config()?;
    let mut snapshots: Vec<_> = source_specs(paths, &config).iter().map(scan).collect();
    validate(&mut snapshots, registry());
    let resolution = resolve(&snapshots, &config.selections);
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
        assert_eq!(value["data"]["created"].as_array().unwrap().len(), 5);
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
