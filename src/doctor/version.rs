//! The harness version probe of `doctor`: `<program> --version` through
//! [`ProcessRunner`], with the normal timeout, from a fresh temporary
//! directory (neutral: no project configuration of d-mcp's working
//! directory), with `DISABLE_AUTOUPDATER=1` and the harness environment
//! d-mcp resolved (`HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`).
//!
//! Child output is parsed only. A version is shown only when it has the
//! shape of a verified version (the same text around a numeric version
//! token, such as `2.1.312 (Claude Code)` next to `2.1.310 (Claude Code)`);
//! any other output is reported as unrecognized and never printed.

use crate::adapter::Adapter;
use crate::process::{Exit, Invocation, ProcessRunner, RunError, find_program};
use crate::store::Environment;

/// The longest version line d-mcp shows.
const MAX_VERSION_LEN: usize = 64;

/// What `<program> --version` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Probe {
    /// The program is not on `PATH`.
    NotFound,
    /// A version line with the shape of a verified version.
    Version(String),
    /// The program ran, but its output has no recognizable version.
    Unrecognized,
    /// The program failed: the exit status, `timeout`, or `could not be
    /// started`; never its output.
    Failed(String),
}

/// Whether `token` is a numeric version: digits and dots, starting and
/// ending with a digit, optionally followed by `-` or `+` and ASCII
/// letters, digits and dots.
fn is_version_token(token: &str) -> bool {
    let (core, rest) = match token.split_once(['-', '+']) {
        Some((_, "")) => return false,
        Some((core, rest)) => (core, rest),
        None => (token, ""),
    };
    core.split('.').count() >= 2
        && core
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
}

/// The version line, when it has the shape of one of the `verified`
/// versions: the same prefix and suffix around a version token.
pub(super) fn recognize(line: &str, verified: &[&str]) -> Option<String> {
    if line.len() > MAX_VERSION_LEN {
        return None;
    }
    verified.iter().find_map(|known| {
        let token = known.split(' ').find(|word| is_version_token(word))?;
        let start = known.find(token)?;
        let (prefix, suffix) = (&known[..start], &known[start + token.len()..]);
        let middle = line.strip_prefix(prefix)?.strip_suffix(suffix)?;
        is_version_token(middle).then(|| line.to_owned())
    })
}

/// Runs `<program> --version` for an adapter.
pub(super) fn probe(adapter: &dyn Adapter, env: &Environment, runner: &ProcessRunner) -> Probe {
    let Some(program) = find_program(env.path.as_deref(), adapter.program().as_ref()) else {
        return Probe::NotFound;
    };
    let Ok(dir) = tempfile::Builder::new().prefix("d-mcp-").tempdir() else {
        return Probe::Failed("no temporary directory".to_owned());
    };
    let mut invocation = Invocation::new(program)
        .args(["--version"])
        .current_dir(dir.path())
        .env("DISABLE_AUTOUPDATER", "1");
    if let Ok(home) = env.home() {
        invocation = invocation.env("HOME", home);
    }
    if let Ok(codex_home) = env.codex_home() {
        invocation = invocation.env("CODEX_HOME", codex_home);
    }
    invocation = match env.claude_config_dir() {
        Ok(Some(config_dir)) => invocation.env("CLAUDE_CONFIG_DIR", config_dir),
        _ if env.claude_config_dir_empty() => invocation.env("CLAUDE_CONFIG_DIR", ""),
        _ => invocation.env_remove("CLAUDE_CONFIG_DIR"),
    };
    match runner.run(&invocation, env.path.as_deref()) {
        Ok(output) if output.success() && !output.truncated => {
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text.lines().next().unwrap_or_default().trim();
            recognize(line, adapter.verified_versions()).map_or(Probe::Unrecognized, Probe::Version)
        }
        Ok(output) => Probe::Failed(match output.exit {
            _ if output.truncated => "oversized output".to_owned(),
            Exit::Code(code) => format!("exit status {code}"),
            Exit::Signal(signal) => format!("terminated by signal {signal}"),
            Exit::Unknown => "unknown exit status".to_owned(),
        }),
        Err(RunError::NotFound { .. }) => Probe::NotFound,
        Err(RunError::Timeout { .. }) => Probe::Failed("timeout".to_owned()),
        Err(RunError::Spawn { .. }) => Probe::Failed("could not be started".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &[&str] = &["2.1.310 (Claude Code)"];
    const CODEX: &[&str] = &["codex-cli 0.165.0"];

    #[test]
    fn only_version_shaped_lines_are_recognized() {
        assert_eq!(
            recognize("2.1.312 (Claude Code)", CLAUDE).as_deref(),
            Some("2.1.312 (Claude Code)")
        );
        assert_eq!(
            recognize("codex-cli 0.166.0-alpha.1", CODEX).as_deref(),
            Some("codex-cli 0.166.0-alpha.1")
        );
        assert_eq!(
            recognize("codex-cli 0.165.0", CODEX).as_deref(),
            Some("codex-cli 0.165.0")
        );
        for line in [
            "CANARY_FAKE (Claude Code)",
            "2.1 secret (Claude Code)",
            "codex-cli ghp_secret",
            "codex-cli 1.",
            "codex-cli .1.2",
            "codex-cli 1.2-",
            "2.1.312",
            "",
        ] {
            assert_eq!(recognize(line, CLAUDE), None, "{line}");
            assert_eq!(recognize(line, CODEX), None, "{line}");
        }
        assert_eq!(
            recognize(&format!("codex-cli 1.{}", "1".repeat(80)), CODEX),
            None
        );
    }

    #[test]
    fn a_missing_program_is_not_found() {
        let env = Environment::default();
        assert_eq!(
            probe(
                &crate::adapter::codex::CodexAdapter,
                &env,
                &ProcessRunner::default()
            ),
            Probe::NotFound
        );
    }
}
