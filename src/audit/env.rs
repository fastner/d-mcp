//! The `execution-env` deny-list: environment variables whose literal value
//! changes what runs (a preloaded library, interpreter options, a module
//! path, a package index or registry, the container daemon, Git's SSH
//! command). Env values stay redacted in every view, the approval view
//! included (ADR 0006), so such a value would otherwise change the approved
//! command unseen. Names are compared case-insensitively.

/// Exact names.
const NAMES: &[&str] = &[
    "PATH",
    "BASH_ENV",
    "ENV",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "PYTHONHOME",
    "PYTHONUSERBASE",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "PIP_FIND_LINKS",
    "PIP_CONFIG_FILE",
    "UV_FIND_LINKS",
    "UV_CONFIG_FILE",
    "UV_PYTHON",
    "PIPX_DEFAULT_PYTHON",
    "YARN_NPM_REGISTRY_SERVER",
    "YARN_REGISTRY",
    "COREPACK_NPM_REGISTRY",
    "BUN_CONFIG_REGISTRY",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "CONTAINER_HOST",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_EXEC_PATH",
];

/// Name prefixes.
const PREFIXES: &[&str] = &["DYLD_", "NPM_CONFIG_", "GIT_CONFIG"];

/// Prefixes whose names count when they contain `INDEX` (`UV_INDEX_URL`,
/// `UV_EXTRA_INDEX_URL`, `PIP_INDEX_URL`, `PIP_EXTRA_INDEX_URL`, ...).
const INDEX_PREFIXES: &[&str] = &["UV_", "PIP_"];

/// Whether a literal value of the variable `name` changes what runs.
pub(super) fn changes_execution(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    NAMES.contains(&name.as_str())
        || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || (INDEX_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) && name.contains("INDEX"))
}

/// The deny-list as documented: exact names, then `PREFIX*` entries.
#[cfg(test)]
pub(super) fn documented() -> Vec<String> {
    NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .chain(PREFIXES.iter().map(|prefix| format!("{prefix}*")))
        .chain(
            INDEX_PREFIXES
                .iter()
                .map(|prefix| format!("{prefix}*INDEX*")),
        )
        .collect()
}
