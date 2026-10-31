//! Pure validation of Git source inputs: remote URLs, refs and commit ids.
//!
//! Nothing here runs `git`; [`super::Git::check_ref_format`] adds the full ref
//! check. Errors name the broken rule only, never the input, so a password or
//! token in a rejected URL is not echoed.

use std::fmt;

use crate::credential::{parse_url_shape, secret_value_heuristic};
use crate::definition::secret_rule;
use crate::error::{Error, Result};
use crate::rule::Rule;

/// Maximum length of a ref, in bytes.
const MAX_REF_LEN: usize = 255;

/// A validated Git remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUrl {
    text: String,
    ssh: bool,
}

impl GitUrl {
    /// A URL that skipped validation, for tests of git's own guards.
    #[cfg(test)]
    pub(crate) fn unchecked(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            ssh: false,
        }
    }

    /// The URL as given.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether the URL uses SSH (`ssh://` or the scp-like form).
    #[must_use]
    pub fn is_ssh(&self) -> bool {
        self.ssh
    }

    /// The source id derived from the last path segment, without a `.git`
    /// suffix and lowercased; `None` when it breaks the name rule.
    #[must_use]
    pub fn default_id(&self) -> Option<String> {
        let segment = self
            .text
            .rsplit(['/', ':'])
            .find(|segment| !segment.is_empty())?;
        let stem = segment.strip_suffix(".git").unwrap_or(segment);
        let id = stem.to_ascii_lowercase();
        crate::definition::is_valid_name(&id).then_some(id)
    }
}

impl fmt::Display for GitUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn invalid<T>(rule: Rule) -> Result<T> {
    Err(Error::SourceUrlInvalid { rule })
}

/// Validates a Git remote URL given as input: [`validate_url_syntax`],
/// and no SSH user name, host or path segment that matches
/// [`secret_value_heuristic`] (its secret rule).
///
/// # Errors
///
/// [`Error::SourceUrlInvalid`] with the broken rule.
pub fn validate_url(text: &str) -> Result<GitUrl> {
    let parts = check_syntax(text)?;
    if let Some(finding) = parts
        .user
        .into_iter()
        .chain([parts.host])
        .chain(parts.path.split(['/', ':']))
        .filter(|segment| !segment.is_empty())
        .find_map(secret_value_heuristic)
    {
        return invalid(secret_rule(finding));
    }
    Ok(GitUrl {
        text: text.to_owned(),
        ssh: parts.ssh,
    })
}

/// The checked parts of a URL.
struct Parts<'a> {
    /// Whether the URL uses SSH.
    ssh: bool,
    /// The SSH user name, if any.
    user: Option<&'a str>,
    /// The host; empty for a local path.
    host: &'a str,
    /// The path.
    path: &'a str,
}

/// Checks the syntax and the transport allow-list of a Git remote URL,
/// without the secret rules. Stored files (`config.toml`, `approvals.toml`)
/// and `refresh` use this: the secret rules run when a URL is entered, so a
/// later, stricter heuristic never turns a stored file corrupt.
///
/// Accepted: `https://host/path`, `ssh://[user@]host[:port]/path`, the
/// scp-like `[user@]host:path`, `file:///path` and an absolute path. A
/// password is rejected everywhere, any user info on `https` and `file`, a
/// query or fragment (`url-query`), another scheme or a `<transport>::`
/// address such as `ext::` (`url-scheme`), a leading `-`, whitespace and
/// control characters (`url-invalid`), and an `@` in the path
/// (`url-userinfo`, it usually means a password containing `/`). A host or
/// SSH user that starts with `-` is `url-invalid`, and so is a `[` or `]`
/// in the host of the scp-like form (`[-oProxyCommand=x]:path`); an IPv6
/// host needs the `ssh://[addr]/path` form.
///
/// # Errors
///
/// [`Error::SourceUrlInvalid`] with the broken rule.
pub fn validate_url_syntax(text: &str) -> Result<GitUrl> {
    let parts = check_syntax(text)?;
    Ok(GitUrl {
        text: text.to_owned(),
        ssh: parts.ssh,
    })
}

/// The checks of [`validate_url_syntax`].
fn check_syntax(text: &str) -> Result<Parts<'_>> {
    if text.is_empty()
        || text.starts_with('-')
        || text.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return invalid(Rule::UrlInvalid);
    }
    if text.contains(['?', '#']) {
        return invalid(Rule::UrlQuery);
    }
    let parts = if text.contains("://") {
        url_form(text)?
    } else if text.starts_with('/') {
        Parts {
            ssh: false,
            user: None,
            host: "",
            path: text,
        }
    } else if text.contains("::") {
        // `ext::`, `fd::` and other remote-helper transports.
        return invalid(Rule::UrlScheme);
    } else {
        scp_form(text)?
    };
    if parts.path.contains('@') {
        return invalid(Rule::UrlUserinfo);
    }
    Ok(parts)
}

/// Checks the `scheme://` form.
fn url_form(text: &str) -> Result<Parts<'_>> {
    let Some(url) = parse_url_shape(text) else {
        // A password with `/` (`user:p/ss@host`) breaks the shape.
        return invalid(if crate::credential::contains_userinfo(text) {
            Rule::UrlUserinfo
        } else {
            Rule::UrlInvalid
        });
    };
    let scheme = url.scheme.to_ascii_lowercase();
    let ssh = match scheme.as_str() {
        "https" | "file" => {
            if url.userinfo.is_some() {
                return invalid(Rule::UrlUserinfo);
            }
            false
        }
        "ssh" => {
            if let Some(user) = url.userinfo {
                if user.contains(':') {
                    return invalid(Rule::UrlUserinfo);
                }
                if user.is_empty() || user.starts_with('-') {
                    return invalid(Rule::UrlInvalid);
                }
            }
            true
        }
        _ => return invalid(Rule::UrlScheme),
    };
    let host_ok = if scheme == "file" {
        url.host.is_empty() && url.port.is_none()
    } else {
        !url.host.is_empty() && !url.host.starts_with('-')
    };
    if !host_ok || url.path.len() < 2 {
        return invalid(Rule::UrlInvalid);
    }
    Ok(Parts {
        ssh,
        user: url.userinfo,
        host: url.host,
        path: url.path,
    })
}

/// Checks the scp-like `[user@]host:path` form.
fn scp_form(text: &str) -> Result<Parts<'_>> {
    let Some((authority, path)) = text.split_once(':') else {
        return invalid(Rule::UrlInvalid);
    };
    if authority.contains(['/', '[', ']']) {
        // A `/` makes git read a relative local path; git strips brackets
        // from the host, which would hide a leading `-` from the check
        // below.
        return invalid(Rule::UrlInvalid);
    }
    if path.contains('@') {
        // `user:password@host:path`.
        return invalid(Rule::UrlUserinfo);
    }
    let (user, host) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    let user_ok = user.is_none_or(|user| !user.is_empty() && !user.starts_with('-'));
    if !user_ok || host.is_empty() || host.starts_with('-') || path.is_empty() {
        return invalid(Rule::UrlInvalid);
    }
    Ok(Parts {
        ssh: true,
        user,
        host,
        path,
    })
}

/// The syntax checks of a ref that need no `git`: not empty, at most 255
/// bytes, no leading `-`, no whitespace or control characters.
///
/// # Errors
///
/// [`Error::SourceRefInvalid`].
pub fn validate_ref_syntax(reference: &str) -> Result<()> {
    if reference.is_empty()
        || reference.len() > MAX_REF_LEN
        || reference.starts_with('-')
        || reference
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::SourceRefInvalid { what: "ref" });
    }
    Ok(())
}

/// A full commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hex characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitId(String);

impl CommitId {
    /// Parses a canonical (lowercase) commit id.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        is_object_id(text).then(|| Self(text.to_owned()))
    }

    /// Parses a commit id given on the command line; hex case is ignored.
    ///
    /// # Errors
    ///
    /// [`Error::SourceRefInvalid`] for anything but 40 or 64 hex characters.
    pub fn from_input(text: &str) -> Result<Self> {
        Self::parse(&text.to_ascii_lowercase()).ok_or(Error::SourceRefInvalid { what: "commit" })
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first 12 characters, for text output.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether text is a full object id: 40 or 64 lowercase hex characters.
#[must_use]
pub fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_of(url: &str) -> Option<Rule> {
        match validate_url(url) {
            Ok(_) => None,
            Err(Error::SourceUrlInvalid { rule }) => Some(rule),
            Err(other) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn accepted_urls() {
        for (url, ssh) in [
            ("https://github.com/me/mcp-defs.git", false),
            ("https://git.example.com:8443/team/defs", false),
            ("ssh://git@github.com/me/defs.git", true),
            ("ssh://github.com:22/me/defs.git", true),
            ("git@github.com:me/mcp-defs.git", true),
            ("github.com:me/defs", true),
            ("file:///srv/git/defs.git", false),
            ("/srv/git/defs.git", false),
            ("ssh://git@[::1]:2222/me/defs.git", true),
            ("deploy-bot@git.internal.example.com:team/defs.git", true),
        ] {
            let parsed = validate_url(url).unwrap_or_else(|error| panic!("{url}: {error}"));
            assert_eq!(parsed.is_ssh(), ssh, "{url}");
            assert_eq!(parsed.as_str(), url);
            assert_eq!(parsed.to_string(), url);
        }
    }

    #[test]
    fn rejected_urls_name_the_rule_only() {
        let cases = [
            ("", Rule::UrlInvalid),
            ("-uhttps://x/y", Rule::UrlInvalid),
            ("--upload-pack=touch /tmp/x", Rule::UrlInvalid),
            ("https://host/a b", Rule::UrlInvalid),
            (
                "https://user:hunter2@github.com/me/defs.git",
                Rule::UrlUserinfo,
            ),
            ("https://me@github.com/me/defs.git", Rule::UrlUserinfo),
            (
                "ssh://git:hunter2@github.com/me/defs.git",
                Rule::UrlUserinfo,
            ),
            ("ssh://git:p/ss@github.com/me/defs.git", Rule::UrlUserinfo),
            ("git:hunter2@github.com:me/defs.git", Rule::UrlUserinfo),
            ("file://me@/srv/defs.git", Rule::UrlUserinfo),
            ("https://host/x@y", Rule::UrlUserinfo),
            ("https://github.com/me/defs.git?token=abc", Rule::UrlQuery),
            ("https://github.com/me/defs.git#frag", Rule::UrlQuery),
            ("http://github.com/me/defs.git", Rule::UrlScheme),
            ("git://github.com/me/defs.git", Rule::UrlScheme),
            ("ext::sh -c touch% /tmp/pwned", Rule::UrlInvalid),
            ("ext::sh", Rule::UrlScheme),
            ("fd::3", Rule::UrlScheme),
            ("file://host/srv/defs.git", Rule::UrlInvalid),
            ("https:///defs.git", Rule::UrlInvalid),
            ("https://-oProxyCommand=x/defs", Rule::UrlInvalid),
            ("-oProxyCommand=x:defs", Rule::UrlInvalid),
            ("git@-oProxyCommand=x:defs", Rule::UrlInvalid),
            ("@host:defs", Rule::UrlInvalid),
            ("host:", Rule::UrlInvalid),
            ("relative/path", Rule::UrlInvalid),
            ("a/b:c", Rule::UrlInvalid),
            // Brackets in the scp-like host would hide a leading `-`.
            ("[-oProxyCommand=id]:x", Rule::UrlInvalid),
            ("git@[-oProxyCommand=id]:x", Rule::UrlInvalid),
            ("[fe80]:defs", Rule::UrlInvalid),
            ("ssh://[-oProxyCommand=id]/defs", Rule::UrlInvalid),
            // A secret in the SSH user name or in the host.
            (
                "ssh://ghp_abcdefghijklmnopqrstuvwxyz0123456789@github.com/me/defs.git",
                Rule::SecretPrefix,
            ),
            (
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789@github.com:me/defs.git",
                Rule::SecretPrefix,
            ),
            (
                "https://ghp_abcdefghijklmnopqrstuvwxyz0123456789.example.com/me/defs.git",
                Rule::SecretPrefix,
            ),
            ("glpat-abcdefghijklmnopqrst:me/defs.git", Rule::SecretPrefix),
            (
                "https://github.com/ghp_abcdefghijklmnopqrstuvwxyz0123456789/defs.git",
                Rule::SecretPrefix,
            ),
        ];
        for (url, rule) in cases {
            assert_eq!(rule_of(url), Some(rule), "{url}");
            if let Err(error) = validate_url(url) {
                assert!(!error.hint().contains("hunter2"));
                assert!(!error.hint().contains("ghp_"));
            }
        }
    }

    #[test]
    fn the_syntax_check_leaves_out_the_secret_rules_only() {
        let secret = "https://github.com/ghp_abcdefghijklmnopqrstuvwxyz0123456789/defs.git";
        assert!(validate_url(secret).is_err());
        assert_eq!(validate_url_syntax(secret).unwrap().as_str(), secret);
        for url in [
            "https://user:hunter2@github.com/me/defs.git",
            "ext::sh",
            "http://h/x",
        ] {
            assert!(validate_url_syntax(url).is_err(), "{url}");
        }
        assert!(
            validate_url_syntax("git@github.com:me/defs.git")
                .unwrap()
                .is_ssh()
        );
    }

    #[test]
    fn default_ids_come_from_the_last_segment() {
        let id = |url: &str| validate_url(url).unwrap().default_id();
        assert_eq!(
            id("git@github.com:me/MCP-defs.git").as_deref(),
            Some("mcp-defs")
        );
        assert_eq!(id("https://host/team/defs/").as_deref(), Some("defs"));
        assert_eq!(id("/srv/git/personal.git").as_deref(), Some("personal"));
        assert_eq!(id("https://host/team/my.defs"), None);
    }

    #[test]
    fn refs_and_commits() {
        assert!(validate_ref_syntax("main").is_ok());
        assert!(validate_ref_syntax("release/v1.2").is_ok());
        for bad in ["", "-x", "a b", "a\nb", &"a".repeat(256)] {
            assert!(validate_ref_syntax(bad).is_err(), "{bad}");
        }
        let sha1 = "a".repeat(40);
        let sha256 = "0123456789abcdef".repeat(4);
        assert_eq!(CommitId::parse(&sha1).unwrap().as_str(), sha1);
        assert_eq!(CommitId::parse(&sha256).unwrap().short(), "0123456789ab");
        assert!(CommitId::parse(&"A".repeat(40)).is_none());
        assert!(CommitId::parse("abc").is_none());
        assert!(CommitId::parse(&"g".repeat(40)).is_none());
        assert_eq!(
            CommitId::from_input(&"ABCDEF0123".repeat(4))
                .unwrap()
                .to_string(),
            "abcdef0123".repeat(4)
        );
        assert!(matches!(
            CommitId::from_input("main"),
            Err(Error::SourceRefInvalid { what: "commit" })
        ));
    }
}
