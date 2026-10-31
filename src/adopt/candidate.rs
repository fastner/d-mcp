//! The plan of one adopted server: its origins, the candidate definition
//! built from them, every refusal, and per harness what adoption writes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;

use super::origin::{self, OriginPlan, WriteMode};
use super::{AdoptOptions, Context, EntryRef, Place, Refusal, ScopeKind};
use crate::adapter::{
    self, DiscoveredScope, GapSeverity, Imported, Normalization, RawEntry, Replacement,
    ReplacementKind, ScopeRead, adapter_for,
};
use crate::config::{Config, Harness, HarnessSet, LOCAL_SOURCE_ID, Selection};
use crate::credential::secret_value_heuristic;
use crate::definition::{
    Auth, Definition, EnvValue, Transport, is_valid_name, is_valid_variable_name,
    parse_definition_with_sites, to_toml,
};
use crate::error::Error;
use crate::redact::{EntryView, entry_view, safe_key};
use crate::resolver::{FileEntry, Status};
use crate::source::{load_snapshots, resolve_snapshots};
use crate::state::{ReplacedField, State};
use crate::sync::{Desired, desired_for};
use crate::validate::validate_definition;

/// How the origins of a server relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// One origin.
    Single,
    /// Several origins with the same core and the same replaced literals;
    /// their override tables are merged.
    Duplicate,
    /// Several origins that differ; `--from` chooses the core.
    Conflict,
}

/// The state of a variable that replaces a literal, in the current shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VarStatus {
    /// Set, non-empty and equal to the replaced literal (or, for the client
    /// secret, set and non-empty).
    Ok,
    /// Unset or empty.
    Missing,
    /// Set to another value than the replaced literal.
    Differs,
}

/// One variable of the variable gate. The comparison happened in memory;
/// no value is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VariableCheck {
    /// The variable, as [`safe_key`] shows it.
    pub name: String,
    /// The replaced field, or `auth.client_secret_env`.
    pub field: String,
    /// The origin entry whose literal was compared; `None` for the client
    /// secret.
    pub origin: Option<String>,
    /// The state.
    pub status: VarStatus,
    /// Whether `--allow-missing-env` or `--allow-env-mismatch` accepted a
    /// failed check.
    pub accepted: bool,
}

/// A replaced literal of the adopted definition, without its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplacedLiteral {
    /// The origin entry.
    pub origin: String,
    /// The native field.
    pub field: String,
    /// `env`, `bearer-header` or `header`.
    pub kind: &'static str,
    /// The variable the definition reads instead, as [`safe_key`] shows it.
    pub var: String,
}

/// A declared normalization of an origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OriginNormalization {
    /// The origin entry.
    pub origin: String,
    /// The native field and the kind.
    #[serde(flatten)]
    pub normalization: Normalization,
}

/// The plan of one server name.
pub struct ServerPlan {
    /// The server name.
    pub name: String,
    /// The harnesses whose entries are adopted: the selection.
    pub harnesses: BTreeSet<Harness>,
    /// How the origins relate; `None` when an origin cannot be imported.
    pub verdict: Option<Verdict>,
    /// The origin whose core was chosen with `--from`.
    pub from: Option<Place>,
    /// The index of the origin whose core the definition takes.
    pub(crate) base: usize,
    /// The definition file d-mcp writes.
    pub file: PathBuf,
    /// The redacted view of the adopted definition.
    pub view: Option<EntryView>,
    /// The origins, in command-line order.
    pub origins: Vec<OriginPlan>,
    /// The literals the definition replaces with variables.
    pub replaced: Vec<ReplacedLiteral>,
    /// Env variables whose literal value is kept (`--keep-literal`).
    pub kept: Vec<String>,
    /// The declared normalizations.
    pub normalizations: Vec<OriginNormalization>,
    /// The variable gate.
    pub variables: Vec<VariableCheck>,
    /// The refusals.
    pub refusals: Vec<Refusal>,
    /// Notices: shadowing origins, unrecoverable literals, re-logins.
    pub notices: Vec<String>,
    /// The adopted definition; never printed, only through `redact`.
    pub(crate) definition: Option<Definition>,
    /// The replaced literals with their values, for the variable gate; in
    /// memory only.
    pub(crate) literals: Vec<Replacement>,
    /// A harness CLI a write needs but that is not on `PATH`.
    pub(crate) cli_missing: Option<Error>,
    /// The `--keep-literal` names and `--var` fields this server used.
    pub(crate) used_keep: BTreeSet<String>,
    pub(crate) used_vars: BTreeSet<String>,
}

impl ServerPlan {
    fn new(context: &Context<'_>, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            harnesses: BTreeSet::new(),
            verdict: None,
            from: None,
            base: 0,
            file: context.paths.local_servers.join(format!("{name}.toml")),
            view: None,
            origins: Vec::new(),
            replaced: Vec::new(),
            kept: Vec::new(),
            normalizations: Vec::new(),
            variables: Vec::new(),
            refusals: Vec::new(),
            notices: Vec::new(),
            definition: None,
            literals: Vec::new(),
            cli_missing: None,
            used_keep: BTreeSet::new(),
            used_vars: BTreeSet::new(),
        }
    }

    fn refuse(&mut self, entry: Option<String>, field: impl Into<String>, reason: &str) {
        self.refusals.push(Refusal::new(entry, field, reason));
    }
}

/// Plans one server from its origins (all with the same name).
pub(super) fn plan_server(
    context: &Context<'_>,
    reads: &BTreeMap<Harness, Vec<ScopeRead>>,
    config: &Config,
    state: &State,
    name: &str,
    entries: &[&EntryRef],
) -> ServerPlan {
    let mut server = ServerPlan::new(context, name);
    if !is_valid_name(name) {
        // There is no renaming: the native name must be a neutral name.
        server.refuse(None, "name", "name-rule");
        return server;
    }
    for entry in entries {
        let harness = entry.place.harness;
        if !server.harnesses.insert(harness) {
            server.refuse(Some(entry.to_string()), "", "duplicate-origin");
            continue;
        }
        let scopes = reads.get(&harness).map_or(&[][..], Vec::as_slice);
        let mut refusals = Vec::new();
        let plan = origin::plan_origin(context, scopes, entry, &mut refusals);
        server.refusals.extend(refusals);
        let managed = state.records.keys().any(|key| {
            key.harness == harness && key.scope == crate::adapter::Scope::User && key.name == name
        });
        if managed {
            server.refuse(Some(plan.label()), "", "already-managed");
        }
        server.origins.push(plan);
    }
    if server
        .origins
        .iter()
        .any(|origin| origin.imported.is_none())
    {
        return server;
    }
    let Some(mut definition) = merge(context.options, &mut server) else {
        return server;
    };
    apply_flags(context.options, &mut server, &mut definition);
    let sites = match check_definition(&mut server, &definition) {
        Some(sites) => sites,
        None => return server,
    };
    server.view = Some(crate::redact::definition_view(&definition));
    if std::fs::symlink_metadata(&server.file).is_ok() {
        server.refuse(None, "", "local-exists");
    }
    let wanted = HarnessSet::Only(server.harnesses.clone());
    if config
        .selection(name)
        .is_some_and(|selection| selection.harnesses != wanted)
    {
        server.refuse(None, "", "selection-differs");
    }
    let desired = resolve_with(context, config, &mut server, &definition, sites);
    plan_targets(context, reads, &mut server, &definition, &desired);
    record_replacements(&mut server, &definition);
    variable_gate(context, &mut server, &definition);
    server.definition = Some(definition);
    server
}

/// Builds the candidate from the origins: one origin as it is, duplicates
/// with merged override tables, a conflict with the core chosen by `--from`.
/// Sets the verdict and the replaced literals.
fn merge(options: &AdoptOptions<'_>, server: &mut ServerPlan) -> Option<Definition> {
    let imports: Vec<&Imported> = server
        .origins
        .iter()
        .filter_map(|origin| origin.imported.as_ref())
        .collect();
    let verdict = match imports.as_slice() {
        [_] => Verdict::Single,
        [first, rest @ ..] if rest.iter().all(|other| first.same_core(other)) => Verdict::Duplicate,
        _ => Verdict::Conflict,
    };
    server.verdict = Some(verdict);
    let chosen = match options.from {
        Some(place) => {
            let found = server
                .origins
                .iter()
                .position(|origin| origin.entry.place == place);
            if found.is_some() {
                server.from = Some(place);
            }
            found
        }
        None => None,
    };
    let base = match (verdict, chosen) {
        (_, Some(index)) => index,
        (Verdict::Conflict, None) => {
            server.refuse(None, "", "conflict");
            return None;
        }
        (_, None) => 0,
    };
    server.base = base;
    let imported = server.origins[base].imported.as_ref()?;
    let mut definition = imported.definition.clone();
    for origin in &server.origins {
        for (harness, table) in origin
            .imported
            .iter()
            .flat_map(|imported| &imported.definition.overrides)
        {
            definition.overrides.insert(harness.clone(), table.clone());
        }
    }
    let label = server.origins[base].label();
    server.literals.clone_from(&imported.replacements);
    for origin in &server.origins {
        if let Some(imported) = &origin.imported {
            server
                .normalizations
                .extend(
                    imported
                        .normalizations
                        .iter()
                        .map(|normalization| OriginNormalization {
                            origin: origin.label(),
                            normalization: normalization.clone(),
                        }),
                );
        }
    }
    server.replaced = server
        .literals
        .iter()
        .map(|replacement| ReplacedLiteral {
            origin: label.clone(),
            field: replacement.field.clone(),
            kind: replacement.kind.as_str(),
            var: safe_key(&replacement.var).into_owned(),
        })
        .collect();
    Some(definition)
}

/// The header variable of an http definition, if it has one.
fn header_var(definition: &mut Definition) -> Option<&mut String> {
    match &mut definition.transport {
        Transport::Http {
            auth: Auth::BearerEnv { var, .. } | Auth::HeaderEnv { var, .. },
            ..
        } => Some(var),
        _ => None,
    }
}

fn is_header(kind: ReplacementKind) -> bool {
    matches!(
        kind,
        ReplacementKind::BearerHeader | ReplacementKind::Header
    )
}

/// Whether a variable name may hold a header value in both adapters: a
/// valid name that is shown as it is (it does not look like a secret, see
/// [`safe_key`]), and not one Claude Code reads as empty in headers.
fn header_name_ok(var: &str) -> Option<&'static str> {
    if !is_valid_variable_name(var) || safe_key(var) != var {
        Some("variable-name")
    } else if adapter::claude::is_credential_name(var) {
        Some("credential-name")
    } else {
        None
    }
}

/// Applies `--var`, `--keep-literal`, `--prefer-oauth`, `--oauth-supported`
/// and `--client-secret-env`, and refuses header variable proposals that do
/// not work for both adapters.
fn apply_flags(options: &AdoptOptions<'_>, server: &mut ServerPlan, definition: &mut Definition) {
    for choice in &options.vars {
        let matches = server.origins.iter().any(|origin| {
            origin.imported.iter().any(|imported| {
                imported
                    .replacements
                    .iter()
                    .any(|r| is_header(r.kind) && r.field == choice.field)
            })
        });
        if !matches {
            continue;
        }
        server.used_vars.insert(choice.field.clone());
        if let Some(reason) = header_name_ok(&choice.var) {
            server.refuse(None, choice.field.clone(), reason);
            continue;
        }
        if let Some(var) = header_var(definition) {
            var.clone_from(&choice.var);
        }
        for replacement in server.literals.iter_mut().filter(|r| is_header(r.kind)) {
            replacement.var.clone_from(&choice.var);
        }
        for replaced in server.replaced.iter_mut().filter(|r| r.kind != "env") {
            replaced.var = safe_key(&choice.var).into_owned();
        }
    }
    // A proposal (not chosen with `--var`) must work for both adapters.
    let proposals: Vec<(String, String)> = server
        .literals
        .iter()
        .filter(|r| is_header(r.kind) && !server.used_vars.contains(&r.field))
        .map(|r| (r.field.clone(), r.var.clone()))
        .collect();
    for (field, var) in proposals {
        if let Some(reason) = header_name_ok(&var) {
            let reason = if reason == "variable-name" {
                "proposal-invalid"
            } else {
                "proposal-credential-name"
            };
            server.refuse(None, field, reason);
        }
    }
    for name in &options.keep_literal {
        let Some(position) = server
            .literals
            .iter()
            .position(|r| r.kind == ReplacementKind::Env && &r.var == name)
        else {
            continue;
        };
        server.used_keep.insert(name.clone());
        let replacement = &server.literals[position];
        if secret_value_heuristic(replacement.literal.expose()).is_some() {
            server.refuse(None, replacement.field.clone(), "keep-literal-secret");
            continue;
        }
        if let Transport::Stdio { env, .. } = &mut definition.transport {
            env.insert(
                name.clone(),
                EnvValue::Literal(replacement.literal.expose().to_owned()),
            );
        }
        server.kept.push(name.clone());
        server.literals.remove(position);
        server
            .replaced
            .retain(|r| !(r.kind == "env" && &r.var == name));
    }
    let auth = match &mut definition.transport {
        Transport::Http { auth, .. } => Some(auth),
        Transport::Stdio { .. } => None,
    };
    match (options.prefer_oauth, options.oauth_supported, auth) {
        (true, _, Some(auth @ (Auth::BearerEnv { .. } | Auth::HeaderEnv { .. }))) => {
            *auth = Auth::OAuth {
                client_id: None,
                scopes: Vec::new(),
                client_secret_env: None,
            };
            // The header is gone, and with it its variable.
            server.literals.retain(|r| !is_header(r.kind));
            server.replaced.retain(|r| r.kind == "env");
        }
        (
            false,
            true,
            Some(Auth::BearerEnv {
                oauth_supported, ..
            }),
        ) => *oauth_supported = true,
        (true, _, _) => server.refuse(None, "auth", "prefer-oauth-invalid"),
        (false, true, _) => server.refuse(None, "auth", "oauth-supported-invalid"),
        (false, false, _) => {}
    }
    if let Some(var) = &options.client_secret_env {
        match &mut definition.transport {
            Transport::Http {
                auth:
                    Auth::OAuth {
                        client_id: Some(_),
                        client_secret_env,
                        ..
                    },
                ..
            } if is_valid_variable_name(var) && safe_key(var) == var.as_str() => {
                // Like every name an import carries: a variable that does
                // not look like a name is never taken over.
                *client_secret_env = Some(var.clone());
            }
            _ => server.refuse(None, "auth.client_secret_env", "client-secret-env-invalid"),
        }
    }
}

/// Serializes the candidate and parses it back: it must parse, validate
/// against every adapter and equal the candidate. Returns the override
/// sites for the resolver.
fn check_definition(
    server: &mut ServerPlan,
    definition: &Definition,
) -> Option<crate::definition::OverrideSites> {
    let text = to_toml(definition);
    let (parsed, sites) = match parse_definition_with_sites(&text, &definition.name) {
        Ok(parsed) => parsed,
        Err(error) => {
            server.refuse(None, error.field, error.rule.code());
            return None;
        }
    };
    if parsed != *definition {
        server.refuse(None, "", "round-trip");
        return None;
    }
    if let Err(problem) = validate_definition(&parsed, &sites, adapter::registry()) {
        server.refuse(None, problem.field.unwrap_or_default(), problem.rule.code());
        return None;
    }
    Some(sites)
}

/// Resolves with the candidate in the `local` source and the new selection,
/// in memory. Refuses unless the candidate wins; returns what each harness
/// would get.
fn resolve_with(
    context: &Context<'_>,
    config: &Config,
    server: &mut ServerPlan,
    definition: &Definition,
    sites: crate::definition::OverrideSites,
) -> BTreeMap<Harness, Desired> {
    let mut snapshots = load_snapshots(context.paths, config, &context.inputs);
    let Some(local) = snapshots
        .iter_mut()
        .find(|snapshot| snapshot.id == LOCAL_SOURCE_ID && snapshot.available)
    else {
        server.refuse(None, "", "local-unavailable");
        return BTreeMap::new();
    };
    local
        .files
        .retain(|file| file.key.as_deref() != Some(&server.name));
    local.files.push(FileEntry {
        file: format!("servers/{}.toml", server.name),
        key: Some(server.name.clone()),
        outcome: Ok(definition.clone()),
        override_sites: sites,
        findings: Vec::new(),
        gate: None,
    });
    let mut selections = config.selections.clone();
    selections.retain(|selection| selection.server != server.name);
    selections.push(Selection {
        server: server.name.clone(),
        harnesses: HarnessSet::Only(server.harnesses.clone()),
    });
    let resolution = resolve_snapshots(&mut snapshots, config, &selections, &context.inputs);
    let wins = resolution.servers.iter().any(|resolved| {
        resolved.name == server.name
            && resolved.status == Status::Resolved
            && resolved.source.as_deref() == Some(LOCAL_SOURCE_ID)
    });
    if !wins {
        server.refuse(None, "", "not-winning");
        return BTreeMap::new();
    }
    server
        .harnesses
        .iter()
        .filter_map(|harness| {
            let adapter = adapter_for(*harness)?;
            let desired = desired_for(adapter, &resolution).remove(&server.name)?;
            Some((*harness, desired))
        })
        .collect()
}

/// The user-scope read of a harness.
fn user_read(reads: &BTreeMap<Harness, Vec<ScopeRead>>, harness: Harness) -> Option<&ScopeRead> {
    reads
        .get(&harness)?
        .iter()
        .find(|read| read.scope == DiscoveredScope::User)
}

/// Plans each origin's harness: the write, the views, the gaps, the Claude
/// Code re-login rule and the CLI presence.
fn plan_targets(
    context: &Context<'_>,
    reads: &BTreeMap<Harness, Vec<ScopeRead>>,
    server: &mut ServerPlan,
    definition: &Definition,
    desired: &BTreeMap<Harness, Desired>,
) {
    let options = context.options;
    let as_import = Imported {
        definition: definition.clone(),
        replacements: Vec::new(),
        normalizations: Vec::new(),
    };
    let mut refusals = Vec::new();
    let mut notices = Vec::new();
    let mut cli_missing = None;
    for origin in &mut server.origins {
        let label = Some(origin.label());
        let harness = origin.entry.place.harness;
        let adapter = origin.adapter();
        let Some(Desired::Render { rendered, .. }) = desired.get(&harness) else {
            continue;
        };
        for gap in &rendered.gaps {
            if gap.severity == GapSeverity::Important {
                refusals.push(Refusal::new(label.clone(), gap.field.clone(), "render-gap"));
            }
        }
        origin.gaps = rendered
            .gaps
            .iter()
            .filter(|gap| gap.severity == GapSeverity::Note)
            .cloned()
            .collect();
        let user = user_read(reads, harness);
        let user_entry = user.and_then(|read| read.entries.get(&server.name));
        let write = match origin.entry.place.scope {
            ScopeKind::User if origin.hash.as_deref() == Some(rendered.sent_hash.as_str()) => {
                WriteMode::None
            }
            ScopeKind::User => WriteMode::Update,
            _ => {
                if user_entry.is_some() {
                    refusals.push(Refusal::new(label.clone(), "", "user-entry-exists"));
                }
                if !user.is_some_and(|read| read.present) {
                    refusals.push(Refusal::new(label.clone(), "", "harness-not-present"));
                }
                WriteMode::Add
            }
        };
        origin.write = Some(write);
        let after = adapter::rendered_value(rendered);
        if let Some(raw) = &origin.raw {
            origin.before = Some(entry_view(harness, raw, origin.imported.as_ref()));
            origin.changed = origin::changed_fields(
                &adapter.canonical_entry(raw.value()),
                &adapter.canonical_entry(&after),
            );
        }
        origin.after = Some(entry_view(harness, &RawEntry::new(after), Some(&as_import)));
        let needs_cli = adapter.apply_mode() == adapter::ApplyMode::HarnessCli;
        let binary_found = user
            .and_then(|read| read.read.as_ref())
            .is_some_and(|read| read.location.binary_found);
        if write != WriteMode::None && needs_cli && !binary_found {
            cli_missing.get_or_insert_with(|| origin::cli_missing(harness));
        }
        if let (Harness::Claude, WriteMode::Update, Some(raw)) = (harness, write, &origin.raw)
            && adapter::claude::uses_oauth(raw.value())
        {
            if options.accept_relogin {
                notices.push(format!(
                    "`{}`: rewriting the Claude Code entry clears its stored OAuth tokens and client secret; run `claude mcp login {}` afterwards",
                    server.name, server.name
                ));
            } else {
                refusals.push(Refusal::new(label.clone(), "oauth", "relogin-required"));
            }
            let has_client_id = raw
                .value()
                .get("oauth")
                .and_then(|oauth| oauth.get("clientId"))
                .is_some();
            let needs_secret = matches!(
                &definition.transport,
                Transport::Http {
                    auth: Auth::OAuth {
                        client_id: Some(_),
                        client_secret_env: None,
                        ..
                    },
                    ..
                }
            );
            if has_client_id && needs_secret {
                refusals.push(Refusal::new(
                    label.clone(),
                    "oauth.clientId",
                    "client-secret-env-required",
                ));
            }
        }
        if let (ScopeKind::Project, Some(path)) = (origin.entry.place.scope, &origin.path) {
            let literals = origin
                .imported
                .as_ref()
                .is_some_and(|imported| !imported.replacements.is_empty());
            notices.push(format!(
                "`{}`: the {} project-scope entry in {} is left as it is{}; it shadows the adopted user entry in that project",
                server.name,
                harness.as_str(),
                path.display(),
                if literals { " and still holds its literals" } else { "" }
            ));
        }
    }
    server.refusals.extend(refusals);
    server.notices.extend(notices);
    server.cli_missing = cli_missing;
}

/// The variable a replaced literal of an origin reads in the adopted
/// definition: an env literal its key when the definition forwards it, a
/// header literal the definition's header variable when the definition keeps
/// a header of the same kind (for `header_env`, of the same name). `None`
/// when the definition does not carry the field (another core was chosen,
/// the literal was kept, or `--prefer-oauth` dropped the header).
fn realized_var(definition: &Definition, replacement: &Replacement) -> Option<String> {
    match (&definition.transport, replacement.kind) {
        (Transport::Stdio { env, .. }, ReplacementKind::Env) => {
            matches!(env.get(&replacement.var), Some(EnvValue::Forward))
                .then(|| replacement.var.clone())
        }
        (
            Transport::Http {
                auth: Auth::BearerEnv { var, .. },
                ..
            },
            ReplacementKind::BearerHeader,
        ) => Some(var.clone()),
        (
            Transport::Http {
                auth: Auth::HeaderEnv { header, var },
                ..
            },
            ReplacementKind::Header,
        ) if replacement
            .field
            .split_once('.')
            .is_some_and(|(_, name)| name.eq_ignore_ascii_case(header)) =>
        {
            Some(var.clone())
        }
        _ => None,
    }
}

/// Records per origin the replaced fields the adopted definition realizes:
/// the field now reads the variable the definition forwards. A literal the
/// definition does not carry is not recorded.
fn record_replacements(server: &mut ServerPlan, definition: &Definition) {
    for origin in &mut server.origins {
        let Some(imported) = &origin.imported else {
            continue;
        };
        origin.replaced_fields = imported
            .replacements
            .iter()
            .filter_map(|r| {
                Some(ReplacedField {
                    field: r.field.clone(),
                    var: realized_var(definition, r)?,
                })
            })
            .collect();
        origin.replaced_fields.sort();
    }
}

/// The variable gate: every variable that replaces a literal of any origin
/// must be set, non-empty and equal to that literal in the current shell
/// (with a conflict, the literals of the origins `--from` did not choose
/// too, since their entries are rewritten to read the variable); the client
/// secret variable must be set and non-empty. Compared in memory only. A
/// literal of another origin that the definition does not carry is named in
/// a notice: it is unrecoverable after apply.
fn variable_gate(context: &Context<'_>, server: &mut ServerPlan, definition: &Definition) {
    let options = context.options;
    let mut checks: Vec<(VariableCheck, &'static str)> = Vec::new();
    let mut dropped: Vec<(String, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    {
        let base = server.origins.get(server.base);
        let base_has = |replacement: &Replacement| {
            base.and_then(|origin| origin.imported.as_ref())
                .is_some_and(|imported| {
                    imported.replacements.iter().any(|r| {
                        r.kind == replacement.kind
                            && r.var == replacement.var
                            && r.literal == replacement.literal
                    })
                })
        };
        // The chosen origin first, then the others in command-line order.
        let order = std::iter::once(server.base)
            .chain((0..server.origins.len()).filter(|index| *index != server.base));
        let mut compared: Vec<(String, &crate::adapter::Literal)> = Vec::new();
        for index in order {
            let Some(origin) = server.origins.get(index) else {
                continue;
            };
            let Some(imported) = &origin.imported else {
                continue;
            };
            for replacement in &imported.replacements {
                let Some(var) = realized_var(definition, replacement) else {
                    if index != server.base && !base_has(replacement) {
                        dropped.push((origin.label(), replacement.field.clone()));
                    }
                    continue;
                };
                if compared
                    .iter()
                    .any(|(name, literal)| *name == var && **literal == replacement.literal)
                {
                    continue;
                }
                compared.push((var.clone(), &replacement.literal));
                seen.insert(var.clone());
                let status = match context.lookup(&var) {
                    None => VarStatus::Missing,
                    Some(value) if value.is_empty() => VarStatus::Missing,
                    Some(value) if value.to_str() == Some(replacement.literal.expose()) => {
                        VarStatus::Ok
                    }
                    Some(_) => VarStatus::Differs,
                };
                checks.push((
                    VariableCheck {
                        name: safe_key(&var).into_owned(),
                        field: replacement.field.clone(),
                        origin: Some(origin.label()),
                        status,
                        accepted: false,
                    },
                    "the replaced literal",
                ));
            }
        }
    }
    if let Transport::Http {
        auth: Auth::OAuth {
            client_secret_env: Some(var),
            ..
        },
        ..
    } = &definition.transport
        && seen.insert(var.clone())
    {
        let status = match context.lookup(var) {
            Some(value) if !value.is_empty() => VarStatus::Ok,
            _ => VarStatus::Missing,
        };
        checks.push((
            VariableCheck {
                name: safe_key(var).into_owned(),
                field: "auth.client_secret_env".to_owned(),
                origin: None,
                status,
                accepted: false,
            },
            "the client secret Claude Code stored",
        ));
    }
    for (label, field) in dropped {
        server.notices.push(format!(
            "`{label}`: the adopted definition does not carry the literal of `{field}`; it is unrecoverable after apply, d-mcp never stores it"
        ));
    }
    for (mut check, what) in checks {
        let (allowed, reason) = match check.status {
            VarStatus::Ok => (true, None),
            VarStatus::Missing => (options.allow_missing_env, Some("variable-missing")),
            VarStatus::Differs => (options.allow_env_mismatch, Some("variable-mismatch")),
        };
        if let Some(reason) = reason {
            if allowed {
                check.accepted = true;
                let source = check
                    .origin
                    .as_deref()
                    .map(|origin| format!(" of {origin}"))
                    .unwrap_or_default();
                server.notices.push(format!(
                    "`{}` is {} in this shell and was accepted: {what} of `{}`{source} is unrecoverable after apply, d-mcp never stores it",
                    check.name,
                    if check.status == VarStatus::Missing { "unset or empty" } else { "different" },
                    check.field
                ));
            } else {
                server.refuse(check.origin.clone(), check.field.clone(), reason);
            }
        }
        server.variables.push(check);
    }
}

/// Refusals for `--keep-literal`, `--var` and `--from` values that match no
/// server.
pub(super) fn unmatched_flags(options: &AdoptOptions<'_>, servers: &[ServerPlan]) -> Vec<Refusal> {
    let mut refusals = Vec::new();
    for name in &options.keep_literal {
        if !servers.iter().any(|server| server.used_keep.contains(name)) {
            refusals.push(Refusal::new(
                None,
                format!("env.{}", crate::redact::safe_key(name)),
                "keep-literal-unknown",
            ));
        }
    }
    for choice in &options.vars {
        if !servers
            .iter()
            .any(|server| server.used_vars.contains(&choice.field))
        {
            refusals.push(Refusal::new(
                None,
                crate::redact::safe_key(&choice.field),
                "var-unknown-field",
            ));
        }
    }
    if let Some(place) = options.from
        && !servers.iter().any(|server| server.from == Some(place))
    {
        refusals.push(Refusal::new(None, place.to_string(), "from-unknown"));
    }
    refusals
}
