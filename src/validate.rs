//! Adapter-specific override validation, between the source scan and the
//! resolver.
//!
//! `parse_definition` stays adapter-agnostic. This step walks the adapter
//! registry and turns a valid definition whose `harness.<id>` table breaks
//! that adapter's allow-list (`override-key`), a per-key type or range
//! (`override-type`) or a per-key transport restriction
//! (`override-transport`) into a per-file problem. The resolver then reports
//! it as `invalid-definition`, and as `invalid-winner` for a selected server,
//! which holds the server for every harness (fail closed). Because `resolve`
//! and `sync` both run this step, they always agree, and a value the harness
//! would refuse to load is never written.
//!
//! The type check reads the TOML kind recorded at parse time, so a datetime
//! is never accepted as a string. A key that is not on the allow-list is not
//! echoed unless it is one of the adapter's deliberately excluded keys.

use crate::adapter::{
    Adapter, MAX_PORT, MAX_TIMEOUT_MILLISECONDS, MAX_TIMEOUT_SECONDS, MIN_PORT,
    MIN_TIMEOUT_MILLISECONDS, OverrideType,
};
use crate::definition::{Definition, OverrideKind, OverrideSite, OverrideSites, Transport};
use crate::resolver::{FileProblem, SourceSnapshot};
use crate::rule::Rule;

/// Validates every valid definition of the snapshots against the adapters.
pub fn validate(snapshots: &mut [SourceSnapshot], adapters: &[&dyn Adapter]) {
    for snapshot in snapshots {
        for file in &mut snapshot.files {
            if let Ok(definition) = &file.outcome
                && let Err(problem) =
                    validate_definition(definition, &file.override_sites, adapters)
            {
                file.outcome = Err(problem);
            }
        }
    }
}

/// Validates one definition's override tables, given the override sites
/// returned by [`crate::definition::parse_definition_with_sites`].
///
/// # Errors
///
/// The first broken rule, in adapter order and then file order.
pub fn validate_definition(
    definition: &Definition,
    sites: &OverrideSites,
    adapters: &[&dyn Adapter],
) -> Result<(), FileProblem> {
    for adapter in adapters {
        let harness = adapter.harness().as_str();
        for site in sites.of(harness) {
            check_site(*adapter, definition, harness, site)?;
        }
    }
    Ok(())
}

fn problem(rule: Rule, field: String, site: &OverrideSite) -> FileProblem {
    FileProblem {
        rule,
        field: Some(field),
        line: Some(site.line),
        column: Some(site.column),
    }
}

fn check_site(
    adapter: &dyn Adapter,
    definition: &Definition,
    harness: &str,
    site: &OverrideSite,
) -> Result<(), FileProblem> {
    let table = format!("harness.{harness}");
    let Some(allowed) = adapter
        .override_keys()
        .iter()
        .find(|key| key.key == site.key)
    else {
        let field = if adapter
            .excluded_override_keys()
            .contains(&site.key.as_str())
        {
            format!("{table}.{}", site.key)
        } else {
            table
        };
        return Err(problem(Rule::OverrideKey, field, site));
    };
    let value = definition
        .overrides
        .get(harness)
        .and_then(|values| values.get(&site.key));
    let max = f64::from(MAX_TIMEOUT_SECONDS);
    let valid = match allowed.kind {
        OverrideType::AbsolutePath => {
            matches!(site.kind, OverrideKind::String)
                && value
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|path| path.starts_with('/'))
        }
        OverrideType::Boolean => matches!(site.kind, OverrideKind::Boolean),
        OverrideType::Seconds => match site.kind {
            OverrideKind::Integer(seconds) => {
                (0..=i64::from(MAX_TIMEOUT_SECONDS)).contains(&seconds)
            }
            OverrideKind::Float(seconds) => seconds.is_finite() && (0.0..=max).contains(&seconds),
            _ => false,
        },
        OverrideType::StringArray => {
            matches!(site.kind, OverrideKind::Array { all_strings: true })
        }
        OverrideType::NonEmptyStringArray => {
            matches!(site.kind, OverrideKind::Array { all_strings: true })
                && value
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|items| !items.is_empty())
        }
        OverrideType::Milliseconds => matches!(
            site.kind,
            OverrideKind::Integer(ms) if (MIN_TIMEOUT_MILLISECONDS..=MAX_TIMEOUT_MILLISECONDS).contains(&ms)
        ),
        OverrideType::Port => matches!(
            site.kind,
            OverrideKind::Integer(port) if (MIN_PORT..=MAX_PORT).contains(&port)
        ),
    };
    let field = format!("{table}.{}", site.key);
    if !valid {
        return Err(problem(Rule::OverrideType, field, site));
    }
    if allowed.stdio_only && !matches!(definition.transport, Transport::Stdio { .. }) {
        return Err(problem(Rule::OverrideTransport, field, site));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::registry;
    use crate::definition::parse_definition_with_sites;

    fn check_text(text: &str) -> Result<(), FileProblem> {
        let (definition, sites) = parse_definition_with_sites(text, "a").unwrap();
        validate_definition(&definition, &sites, registry())
    }

    fn check(overrides: &str) -> Result<(), FileProblem> {
        check_text(&format!(
            "name = \"a\"\n[transport]\ntype = \"stdio\"\ncommand = \"x\"\n[harness.codex]\n{overrides}"
        ))
    }

    fn check_http(overrides: &str) -> Result<(), FileProblem> {
        check_text(&format!(
            "name = \"a\"\n[transport]\ntype = \"http\"\nurl = \"https://x.test/mcp\"\n[harness.codex]\n{overrides}"
        ))
    }

    #[test]
    fn allow_list_and_types() {
        assert!(check("cwd = \"/srv\"\nenabled = true\nrequired = false\n").is_ok());
        assert!(check("startup_timeout_sec = 10\ntool_timeout_sec = 2.5\n").is_ok());
        assert!(check("enabled_tools = [\"a\"]\ndisabled_tools = []\n").is_ok());
        let unknown = check("knob = 1\n").unwrap_err();
        assert_eq!(unknown.rule, Rule::OverrideKey);
        assert_eq!(unknown.field.as_deref(), Some("harness.codex"));
        assert_eq!((unknown.line, unknown.column), (Some(6), Some(1)));
        let excluded = check("http_headers_helper = \"/bin/x\"\n").unwrap_err();
        assert_eq!(
            excluded.field.as_deref(),
            Some("harness.codex.http_headers_helper")
        );
        for bad in [
            "cwd = 1979-05-27T07:32:00Z\n",
            "cwd = \"relative\"\n",
            "cwd = \"~/x\"\n",
            "enabled = \"yes\"\n",
            "startup_timeout_sec = -1\n",
            "tool_timeout_sec = -0.5\n",
            "tool_timeout_sec = \"5\"\n",
            "enabled_tools = [1]\n",
            "enabled_tools = []\n",
            "disabled_tools = \"x\"\n",
            "startup_timeout_sec = 1e300\n",
            "startup_timeout_sec = 31536001\n",
            "tool_timeout_sec = 31536000.5\n",
            "tool_timeout_sec = 9223372036854775807\n",
        ] {
            assert_eq!(check(bad).unwrap_err().rule, Rule::OverrideType, "{bad}");
        }
        assert!(check("startup_timeout_sec = 31536000\ntool_timeout_sec = 0.0\n").is_ok());
    }

    #[test]
    fn cwd_is_stdio_only() {
        assert!(check_http("enabled = false\nstartup_timeout_sec = 5\n").is_ok());
        let cwd = check_http("cwd = \"/srv\"\n").unwrap_err();
        assert_eq!(cwd.rule, Rule::OverrideTransport);
        assert_eq!(cwd.field.as_deref(), Some("harness.codex.cwd"));
        assert_eq!((cwd.line, cwd.column), (Some(6), Some(1)));
        // A wrong type is reported before the transport restriction.
        assert_eq!(
            check_http("cwd = \"relative\"\n").unwrap_err().rule,
            Rule::OverrideType
        );
    }

    fn check_claude(overrides: &str) -> Result<(), FileProblem> {
        check_text(&format!(
            "name = \"a\"\n[transport]\ntype = \"http\"\nurl = \"https://x.test/mcp\"\n[harness.claude]\n{overrides}"
        ))
    }

    #[test]
    fn claude_overrides_have_an_allow_list() {
        assert!(check_claude("timeout = 1000\noauth_callback_port = 1024\n").is_ok());
        assert!(check_claude("timeout = 31536000000\noauth_callback_port = 65535\n").is_ok());
        let unknown = check_claude("anything = 1\n").unwrap_err();
        assert_eq!(unknown.rule, Rule::OverrideKey);
        assert_eq!(unknown.field.as_deref(), Some("harness.claude"));
        for excluded in ["headersHelper", "headers_helper", "env"] {
            let problem = check_claude(&format!("{excluded} = \"x\"\n")).unwrap_err();
            assert_eq!(problem.rule, Rule::OverrideKey);
            assert_eq!(
                problem.field.as_deref(),
                Some(format!("harness.claude.{excluded}").as_str())
            );
        }
        for bad in [
            "timeout = 999\n",
            "timeout = 31536000001\n",
            "timeout = 1500.0\n",
            "timeout = \"1000\"\n",
            "oauth_callback_port = 80\n",
            "oauth_callback_port = 65536\n",
            "oauth_callback_port = 8080.0\n",
            "oauth_callback_port = true\n",
        ] {
            assert_eq!(
                check_claude(bad).unwrap_err().rule,
                Rule::OverrideType,
                "{bad}"
            );
        }
    }
}
