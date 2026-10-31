//! The launcher audit: table-driven rules that find unpinned package
//! launchers, opaque commands, launchers it cannot parse and execution-
//! changing environment values in a neutral definition. Pure: no I/O.
//!
//! Only the neutral definition is audited (its `command`, `args` and the
//! names of its literal `env` values); the adapters' override allow-lists
//! hold no key that runs a command, which a guard test keeps true. Arbitrary
//! binaries and scripts and remote URLs are not audited.
//!
//! - **Runners** (the `RUNNERS` table) are matched on the command's basename and, for
//!   runners behind a subcommand (`npm exec`, `docker run`), on the
//!   subcommand after the global options. When a global option the table
//!   does not know may have taken the next word as its value and the
//!   subcommand follows later, the launcher is `unparsed-launcher`.
//! - **Options** before the package spec are parsed against the runner
//!   family's lists: flags, options that take a value, options that name
//!   the package (`npx -p`, `uvx --from`, `pipx run --spec`: their value is
//!   the spec and the first positional argument the command it runs),
//!   options that add a package (`uvx --with`, checked as a spec as well)
//!   and shell modes (`npx -c`, opaque). An option the lists do not know,
//!   and a short option cluster with a letter that takes a value
//!   (`docker run -dp 80:80 img`), leave the spec unknown: the launcher is
//!   `unparsed-launcher` (fail closed, never skipped).
//! - **Exact version:** an npm spec needs a full SemVer version
//!   (`pkg@1.2.3`, `@scope/pkg@1.2.3`); a Python spec an exact PEP 440
//!   version (`pkg==1.2.3`, `pkg===1.2.3` or uv's `pkg@1.2.3`). Ranges and
//!   dist-tags (`@^1`, `@1`, `@latest`, `@next`, `>=`) are unpinned.
//! - **VCS and URL specs** (`github:`, `git+https://`, `org/repo`, tarball
//!   URLs) are pinned only by a commit of a Git transport in ref position: a
//!   40- or 64-character hex commit after the last `#` of an npm Git spec,
//!   or after the last `@` of the path of a Python `git+` URL. A plain
//!   `http(s)` archive is always unpinned. Local paths (`./x`, `/x`, `~/x`,
//!   `file:`) are not audited.
//! - **Containers** are pinned by `@sha256:<64 hex>` or a tag other than
//!   `latest`.
//! - **Opaque commands:** a shell (`sh`, `bash`, `zsh`, `dash`, `ksh`,
//!   `fish` and others) with `-c` among its leading options (also in a
//!   cluster such as `-lc`, after value options such as `-o pipefail`), a
//!   runner's shell mode (`npx -c`), and `env -S`.
//! - **Wrappers:** `env` and the pass-through wrappers `timeout`, `nice`,
//!   `nohup`, `stdbuf`, `setsid` and `corepack` are unwrapped one level:
//!   their options (and `env`'s `NAME=value` assignments) are skipped and
//!   the command they run is audited by the same rules. A wrapper inside a
//!   wrapper, and a wrapper option the table does not know, is opaque.
//! - **Execution environment:** a literal `env` value (or an `env`
//!   wrapper assignment) for a name on the deny-list (`NODE_OPTIONS`,
//!   `LD_PRELOAD`, `DYLD_*`, `PYTHONPATH`, `NPM_CONFIG_*`, package index
//!   variables, ...) is `execution-env`. The finding names the variable,
//!   never its value.
//!
//! A finding names its rule and the matched arguments. The findings hash
//! (domain tag [`FINDINGS_DOMAIN_TAG`]) covers exactly the rule ids and the
//! matched arguments, so a new rule or a changed argument invalidates a risk
//! acceptance.

mod env;
mod runner;
mod spec;
mod tables;
mod wrapper;

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::json;

use crate::canonical::tagged_hash;
use crate::definition::{Definition, EnvValue, Transport};

use self::tables::RUNNERS;
use self::wrapper::Unwrapped;

/// Domain tag of the findings hash.
pub const FINDINGS_DOMAIN_TAG: &str = "d-mcp-findings-v1";

/// An audit rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditRule {
    /// An npm-family launcher without an exact version.
    UnpinnedNpmPackage,
    /// A Python launcher without an exact version.
    UnpinnedPythonPackage,
    /// A VCS or URL spec without a commit.
    UnpinnedVcsSpec,
    /// A container image without a digest or a non-`latest` tag.
    UnpinnedContainerImage,
    /// A command the audit cannot see into (a shell `-c` script, a runner's
    /// shell mode, `env -S`, a nested wrapper, an unknown wrapper option).
    OpaqueCommand,
    /// A runner whose options the audit cannot parse, so its package spec is
    /// unknown (an unknown option, a short option cluster with a letter
    /// that takes a value, an unknown global option before the subcommand).
    UnparsedLauncher,
    /// A literal environment value for a variable that changes what runs
    /// (`NODE_OPTIONS`, `LD_PRELOAD`, a package index, ...).
    ExecutionEnv,
}

impl AuditRule {
    /// Every rule, in id order of the reference table.
    pub const ALL: [AuditRule; 7] = [
        AuditRule::UnpinnedNpmPackage,
        AuditRule::UnpinnedPythonPackage,
        AuditRule::UnpinnedVcsSpec,
        AuditRule::UnpinnedContainerImage,
        AuditRule::OpaqueCommand,
        AuditRule::UnparsedLauncher,
        AuditRule::ExecutionEnv,
    ];

    /// The stable rule id.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuditRule::UnpinnedNpmPackage => "unpinned-npm-package",
            AuditRule::UnpinnedPythonPackage => "unpinned-python-package",
            AuditRule::UnpinnedVcsSpec => "unpinned-vcs-spec",
            AuditRule::UnpinnedContainerImage => "unpinned-container-image",
            AuditRule::OpaqueCommand => "opaque-command",
            AuditRule::UnparsedLauncher => "unparsed-launcher",
            AuditRule::ExecutionEnv => "execution-env",
        }
    }
}

/// One audit finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Finding {
    /// The rule.
    pub rule: AuditRule,
    /// The launcher as matched, such as `npx`, `npm exec`, `docker run`,
    /// `sh -c`, `env`, or `[env]` for the definition's env table.
    pub launcher: String,
    /// The matched arguments: the unpinned spec or image, the shell option
    /// and its script, the option that could not be parsed, or the name
    /// (never the value) of an execution-changing variable; empty when a
    /// launcher has no spec at all.
    pub args: Vec<String>,
}

impl Finding {
    fn new(rule: AuditRule, launcher: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            rule,
            launcher: launcher.into(),
            args,
        }
    }
}

/// Audits a definition: only stdio commands are audited. Findings are
/// sorted and deduplicated.
#[must_use]
pub fn audit(definition: &Definition) -> Vec<Finding> {
    match &definition.transport {
        Transport::Stdio { command, args, env } => {
            let mut findings = Vec::new();
            audit_into(command, args, false, &mut findings);
            audit_env(env, &mut findings);
            findings.sort();
            findings.dedup();
            findings
        }
        Transport::Http { .. } => Vec::new(),
    }
}

/// Audits a command line.
#[must_use]
pub fn audit_command(command: &str, args: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    audit_into(command, args, false, &mut findings);
    findings.sort();
    findings.dedup();
    findings
}

/// The `execution-env` findings of the literal values of an env table; a
/// forwarded variable carries the user's own value and is not a finding.
fn audit_env(env: &BTreeMap<String, EnvValue>, findings: &mut Vec<Finding>) {
    for (name, value) in env {
        if matches!(value, EnvValue::Literal(_)) && env::changes_execution(name) {
            findings.push(Finding::new(
                AuditRule::ExecutionEnv,
                "[env]",
                vec![name.clone()],
            ));
        }
    }
}

/// The findings hash: SHA-256 over the canonical JSON of the sorted
/// `[{rule, args}]` list.
#[must_use]
pub fn findings_hash(findings: &[Finding]) -> String {
    let mut items: Vec<(&str, &[String])> = findings
        .iter()
        .map(|finding| (finding.rule.as_str(), finding.args.as_slice()))
        .collect();
    items.sort_unstable();
    items.dedup();
    let list: Vec<serde_json::Value> = items
        .into_iter()
        .map(|(rule, args)| json!({ "rule": rule, "args": args }))
        .collect();
    tagged_hash(FINDINGS_DOMAIN_TAG, &serde_json::Value::Array(list))
}

/// The basename of a command, without a Windows `.exe` or `.cmd` suffix.
fn basename(command: &str) -> &str {
    let base = command.rsplit(['/', '\\']).next().unwrap_or(command);
    [".exe", ".cmd"]
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base)
}

/// Audits `command` with `args`; `wrapped` is set inside a wrapper, where
/// another wrapper is opaque.
fn audit_into(command: &str, args: &[String], wrapped: bool, findings: &mut Vec<Finding>) {
    let base = basename(command);
    if let Some(spec) = wrapper::wrapper(base) {
        if wrapped {
            findings.push(Finding::new(AuditRule::OpaqueCommand, base, Vec::new()));
            return;
        }
        match wrapper::unwrap(spec, args, findings) {
            Unwrapped::Opaque(option) => {
                findings.push(Finding::new(AuditRule::OpaqueCommand, base, vec![option]))
            }
            Unwrapped::Nested(position) => {
                let nested = args[position].as_str();
                // `corepack pnpm@9.1.0 dlx ...` runs `pnpm`.
                let nested = if spec.versioned_command {
                    nested.split('@').next().unwrap_or(nested)
                } else {
                    nested
                };
                audit_into(nested, &args[position + 1..], true, findings);
            }
            Unwrapped::None => {}
        }
        return;
    }
    if let Some(shell) = wrapper::shell(base) {
        if let Some(found) = wrapper::shell_script(shell, args) {
            findings.push(Finding::new(
                AuditRule::OpaqueCommand,
                format!("{base} -c"),
                found,
            ));
        }
        return;
    }
    for runner in RUNNERS.iter().filter(|runner| runner.program == base) {
        let launcher = || {
            std::iter::once(runner.program)
                .chain(runner.subcommand.iter().copied())
                .collect::<Vec<_>>()
                .join(" ")
        };
        match runner::after_subcommand(runner, args) {
            runner::Subcommand::At(rest) => {
                runner::audit_runner(runner.family, &launcher(), &args[rest..], findings);
                return;
            }
            runner::Subcommand::Unparsed(option) => {
                findings.push(Finding::new(
                    AuditRule::UnparsedLauncher,
                    launcher(),
                    vec![option],
                ));
                return;
            }
            runner::Subcommand::NotInvoked => {}
        }
    }
}

#[cfg(test)]
mod tests;
