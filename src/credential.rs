//! Secret heuristics for definition values.
//!
//! The names [`secret_name_heuristic`] and [`secret_value_heuristic`] are
//! deliberately distinct from the Claude Code adapter's credential-name list, so the
//! two cannot be confused. Both functions are pure: they never log, print or
//! store the values they inspect, and their results carry no part of the
//! value.
//!
//! The heuristics produce false positives and false negatives. Only an `env`
//! literal has a remedy for a false positive: turning it into an environment
//! forward. In `args`, `transport.url` and override strings a false positive
//! is a hard rejection; there is deliberately no "allow literal" escape.

/// Name segments that mark a secret, compared case-insensitively.
const SECRET_NAME_SEGMENTS: [&str; 10] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASS",
    "APIKEY",
    "CREDENTIAL",
    "CREDENTIALS",
    "COOKIE",
    "AUTH",
];

/// Known secret prefixes, compared case-sensitively at the start of a token.
const SECRET_PREFIXES: [&str; 15] = [
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "sk-",
    "sk-ant-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "xoxs-",
    "AKIA",
    "AIza",
];

/// Names that mark the left side of a `NAME=value` token as a secret even
/// when the value is a plain word (libpq `password=hunter2`), compared
/// case-insensitively against the whole name. `pwd` is listed here although
/// the name heuristic itself does not match it.
const BARE_SECRET_NAMES: [&str; 5] = ["password", "passwd", "pwd", "secret", "token"];

/// Maximum length of a plain word on the right side of a bare `NAME=value`.
const PLAIN_WORD_MAX: usize = 31;

/// Minimum length of a hex run that counts as a secret.
const HEX_RUN_MIN: usize = 32;
/// Minimum length, in Unicode scalars, for the entropy rule.
const ENTROPY_MIN_LEN: usize = 32;
/// Minimum Shannon entropy, in bits per Unicode scalar, for the entropy rule.
const ENTROPY_MIN_BITS: f64 = 4.0;

/// The rule a value matched in [`secret_value_heuristic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretFinding {
    /// A token starts with a known secret prefix.
    Prefix,
    /// A token looks like a JSON Web Token.
    Jwt,
    /// A run of at least 32 hex characters outside a digest context.
    Hex,
    /// At least 32 characters with an entropy of at least 4.0 bits each.
    Entropy,
    /// A URL query key or the name of a `NAME=value` token matches
    /// [`secret_name_heuristic`].
    Name,
    /// A URL-shaped value or a Go-style DSN carries user info.
    Userinfo,
}

/// Returns whether a name looks like it holds a secret.
///
/// The name is split at `_` and `-`. It matches when one segment equals a
/// known secret word (case-insensitively) or when the segment pair `API`
/// `KEY` occurs.
#[must_use]
pub fn secret_name_heuristic(name: &str) -> bool {
    let segments: Vec<String> = name
        .split(['_', '-'])
        .map(str::to_ascii_uppercase)
        .collect();
    segments
        .iter()
        .any(|segment| SECRET_NAME_SEGMENTS.contains(&segment.as_str()))
        || segments
            .windows(2)
            .any(|pair| pair[0] == "API" && pair[1] == "KEY")
}

/// Returns the first secret rule a value matches, if any.
///
/// The value is split at whitespace and every token is checked:
///
/// - A token is rejected for user info as described in [`contains_userinfo`].
/// - A URL-shaped token (`scheme://...`) is exempt from the entropy rule as a
///   whole; its path segments, query values and fragment are checked one by
///   one, and its query keys are checked with [`secret_name_heuristic`].
/// - A token that is not URL-shaped but contains `scheme://` (such as
///   `jdbc:postgresql://db/app?password=x` or `--url=https://...`) gets the
///   same URL checks on the substring starting at the inner scheme, in
///   addition to the rules below.
/// - A compound token containing `=`, `;` or `,` is split there. The left
///   side of `NAME=value` is checked with [`secret_name_heuristic`] when the
///   token starts with `-` (a flag). For a bare `NAME=value` it is checked
///   only when the value is not a plain word (`[A-Za-z0-9_.-]{1,31}`). A
///   whole name of `password`, `passwd`, `pwd`, `secret` or `token`
///   (case-insensitively) is rejected whatever the value. The right side is checked as a token of its own,
///   and every piece for the known prefixes and the JWT shape.
/// - A token containing `/` is checked as a whole only when it is standard
///   base64 (`[A-Za-z0-9+/]+` with up to two trailing `=`), contains at least
///   one digit or `+`, and does not start with `/`, `./`, `../`, `~/` or `@`.
///   Any other token containing `/` is path-like: every `/`-separated
///   segment is checked one by one.
/// - Any other token is checked as a whole.
///
/// The per-token rules are the known prefixes, the JWT shape, hex runs of at
/// least 32 characters outside digest contexts, and the entropy rule.
#[must_use]
pub fn secret_value_heuristic(value: &str) -> Option<SecretFinding> {
    value.split_whitespace().find_map(check_token)
}

/// Returns whether a value uses variable syntax: `$VAR`, `${VAR}`,
/// `${env:VAR}` or `{env:VAR}`.
#[must_use]
pub fn contains_variable_syntax(value: &str) -> bool {
    if value.contains("{env:") {
        return true;
    }
    let bytes = value.as_bytes();
    bytes.windows(2).any(|pair| {
        pair[0] == b'$' && (pair[1].is_ascii_alphabetic() || pair[1] == b'_' || pair[1] == b'{')
    })
}

/// The parts of a URL-shaped value. Only the structure d-mcp checks is
/// parsed; this is not a full RFC 3986 parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlShape<'a> {
    /// The scheme, as written.
    pub scheme: &'a str,
    /// User info before `@` in the authority, if any.
    pub userinfo: Option<&'a str>,
    /// The host, without brackets for IPv6 literals.
    pub host: &'a str,
    /// The port after the host, if one is written (possibly empty, as in
    /// `host:`).
    pub port: Option<&'a str>,
    /// The path, starting with `/` or empty.
    pub path: &'a str,
    /// The query after `?`, if any.
    pub query: Option<&'a str>,
    /// The fragment after `#`, if any.
    pub fragment: Option<&'a str>,
}

/// Parses a value of the form `scheme://authority[/path][?query][#fragment]`.
///
/// Returns `None` when the value is not URL-shaped: no valid scheme before
/// `://`, whitespace or control characters anywhere, an unclosed `[` in the
/// host, characters after the closing `]` other than a port, or a port that
/// is not a number from 0 to 65535. An empty port (`host:`) is allowed, as in
/// RFC 3986.
#[must_use]
pub fn parse_url_shape(value: &str) -> Option<UrlShape<'_>> {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, rest) = value.split_once("://")?;
    if !is_scheme(scheme) {
        return None;
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (rest, None),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((before, query)) => (before, Some(query)),
        None => (rest, None),
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    let (userinfo, host_port) = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => (Some(userinfo), host_port),
        None => (None, authority),
    };
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, after) = bracketed.split_once(']')?;
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?)
        };
        (host, port)
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    if let Some(port) = port
        && !port.is_empty()
        && !(port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok())
    {
        return None;
    }
    Some(UrlShape {
        scheme,
        userinfo,
        host,
        port,
        path,
        query,
        fragment,
    })
}

/// Whether a scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`.
fn is_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Returns whether a token carries URL user info anywhere.
///
/// For every `scheme://` in the token (the scheme is the longest run of
/// scheme characters directly before `://`), let the rest be the text after
/// `://` and the authority the rest up to the first `/`, `?` or `#`. The
/// token carries user info when the authority contains `@`, or when the rest
/// has a `:` inside the authority whose following text up to that delimiter
/// is not a port (digits only, at most 65535; an empty port counts as a port)
/// and an `@` follows later. The second form catches passwords that contain
/// `/`, `?` or `#` (`user:p/ss@host`), while `https://host:443/@me` and
/// colons in a query (`?t=10:00&e=a@b`) do not match.
///
/// A Go-style DSN, `user:password@tcp(host:port)/db`, where `@` is followed by
/// a protocol name and `(` and preceded by a `:`, with no `/` before the `@`,
/// also counts.
///
/// Known false negative: an all-digit password followed by `/`, `?` or `#`
/// (`user:1234/x@host`) reads as a port and is not caught.
#[must_use]
pub fn contains_userinfo(token: &str) -> bool {
    for start in inner_scheme_starts(token) {
        let Some(offset) = token[start..].find("://") else {
            continue;
        };
        let rest = &token[start + offset + 3..];
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if authority.contains('@') {
            return true;
        }
        // The colons of a bracketed IPv6 host are not a user-info separator.
        let host_start = if authority.starts_with('[') {
            authority.find(']').map_or(0, |end| end + 1)
        } else {
            0
        };
        if let Some(colon) = authority[host_start..].find(':').map(|c| host_start + c)
            && !is_port(&authority[colon + 1..])
            && rest[authority_end..].contains('@')
        {
            return true;
        }
    }
    token.match_indices('@').any(|(index, _)| {
        let after = &token[index + 1..];
        let protocol = after
            .find(|c: char| !c.is_ascii_alphanumeric())
            .map_or(after, |end| &after[..end]);
        let before = &token[..index];
        protocol.starts_with(|c: char| c.is_ascii_alphabetic())
            && after[protocol.len()..].starts_with('(')
            && before.contains(':')
            && !before.contains('/')
    })
}

/// Returns the byte offsets where a valid scheme directly before a `://`
/// starts, one per `://` in the token.
///
/// The scheme is the longest run of scheme characters directly before
/// `://`, with a leading digit or sign skipped.
fn inner_scheme_starts(token: &str) -> impl Iterator<Item = usize> + '_ {
    token.match_indices("://").filter_map(|(index, _)| {
        let before = &token[..index];
        let run_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |position| position + 1);
        // A leading digit or sign is not part of a scheme; skip it.
        let skipped = before[run_start..]
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(before.len() - run_start);
        let start = run_start + skipped;
        is_scheme(&before[start..]).then_some(start)
    })
}

/// Whether text is a port: digits only and at most 65535, or empty.
fn is_port(text: &str) -> bool {
    text.is_empty()
        || (text.bytes().all(|byte| byte.is_ascii_digit()) && text.parse::<u16>().is_ok())
}

fn check_token(token: &str) -> Option<SecretFinding> {
    if token.is_empty() {
        return None;
    }
    if contains_userinfo(token) {
        return Some(SecretFinding::Userinfo);
    }
    if let Some(url) = parse_url_shape(token) {
        return check_url(&url);
    }
    if let Some(finding) = check_embedded_urls(token) {
        return Some(finding);
    }
    if let Some(finding) = check_compound(token) {
        return Some(finding);
    }
    if is_path_like(token) {
        return token.split('/').find_map(check_atom);
    }
    check_atom(token)
}

/// Runs the URL checks on every URL-shaped substring that starts at an
/// inner scheme of a token that is not URL-shaped as a whole.
fn check_embedded_urls(token: &str) -> Option<SecretFinding> {
    inner_scheme_starts(token)
        .filter(|&start| start > 0)
        .filter_map(|start| parse_url_shape(&token[start..]))
        .find_map(|url| check_url(&url))
}

/// Checks a token containing `=`, `;` or `,`: the name heuristic on the left
/// side of `NAME=value` (see [`secret_value_heuristic`] for when), the right
/// side as a token of its own, and the prefix and JWT rules on every piece.
fn check_compound(token: &str) -> Option<SecretFinding> {
    if !token.contains(['=', ';', ',']) {
        return None;
    }
    if let Some((name, value)) = token.split_once('=') {
        let flag = name.starts_with('-');
        let name = name.trim_start_matches('-');
        let listed = BARE_SECRET_NAMES
            .iter()
            .any(|listed| name.eq_ignore_ascii_case(listed));
        if listed
            || ((flag || !is_plain_word(value))
                && is_name_shaped(name)
                && secret_name_heuristic(name))
        {
            return Some(SecretFinding::Name);
        }
        if let Some(finding) = check_token(value) {
            return Some(finding);
        }
    }
    token.split(['=', ';', ',']).find_map(check_shape)
}

/// Whether a string looks like a variable or option name.
fn is_name_shaped(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Whether a value is an identifier-like plain word, `[A-Za-z0-9_.-]{1,31}`.
fn is_plain_word(value: &str) -> bool {
    (1..=PLAIN_WORD_MAX).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Whether a token is checked segment by segment instead of as a whole.
///
/// A token containing `/` is checked as a whole only when it is standard
/// base64 with at least one digit or `+` and no path prefix; everything else
/// containing `/` is path-like.
fn is_path_like(token: &str) -> bool {
    if !token.contains('/') {
        return false;
    }
    let path_prefix = ["/", "./", "../", "~/", "@"]
        .iter()
        .any(|prefix| token.starts_with(prefix));
    path_prefix || !is_slashed_base64(token)
}

/// Whether a token is standard base64, `[A-Za-z0-9+/]+={0,2}`, and contains at
/// least one digit or `+`.
fn is_slashed_base64(token: &str) -> bool {
    let body = token.trim_end_matches('=');
    token.len() - body.len() <= 2
        && !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        && body.chars().any(|c| c.is_ascii_digit() || c == '+')
}

fn check_url(url: &UrlShape<'_>) -> Option<SecretFinding> {
    if url.userinfo.is_some() {
        return Some(SecretFinding::Userinfo);
    }
    if let Some(finding) = url.path.split('/').find_map(check_atom) {
        return Some(finding);
    }
    if let Some(query) = url.query {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if secret_name_heuristic(key) {
                return Some(SecretFinding::Name);
            }
            if let Some(finding) = check_compound(value).or_else(|| check_atom(value)) {
                return Some(finding);
            }
        }
    }
    url.fragment.and_then(check_atom)
}

/// Checks a single token or segment against the per-token rules.
fn check_atom(atom: &str) -> Option<SecretFinding> {
    if let Some(finding) = check_shape(atom) {
        return Some(finding);
    }
    let (secret_hex, residual) = scan_hex_runs(atom);
    if secret_hex {
        return Some(SecretFinding::Hex);
    }
    // Exempt digests are left out of the entropy input, so a pinned digest
    // does not trip the entropy rule either.
    if residual.chars().count() >= ENTROPY_MIN_LEN && shannon_entropy(&residual) >= ENTROPY_MIN_BITS
    {
        return Some(SecretFinding::Entropy);
    }
    None
}

/// The shape rules: a known prefix at the start, or the JWT shape.
fn check_shape(atom: &str) -> Option<SecretFinding> {
    if atom.is_empty() {
        return None;
    }
    if SECRET_PREFIXES
        .iter()
        .any(|prefix| atom.len() > prefix.len() && atom.starts_with(prefix))
    {
        return Some(SecretFinding::Prefix);
    }
    if atom.starts_with("eyJ") && atom.matches('.').count() >= 2 {
        return Some(SecretFinding::Jwt);
    }
    None
}

/// Scans the maximal runs of hex characters in a token.
///
/// Returns whether a run of at least 32 hex characters lies outside a digest
/// context, and the token with every exempt digest run removed. Digest
/// contexts are a run directly after `sha256:` (which covers `@sha256:`), and
/// a 40- or 64-character run directly after `@` or `#`, the commit position of
/// a VCS spec such as `repo@<commit>` or `repo#<commit>`.
fn scan_hex_runs(atom: &str) -> (bool, String) {
    let bytes = atom.as_bytes();
    let mut residual = String::with_capacity(atom.len());
    let mut secret = false;
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_hexdigit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_hexdigit() {
            index += 1;
        }
        let length = index - start;
        if length >= HEX_RUN_MIN {
            if is_digest_context(&atom[..start], length) {
                residual.push_str(&atom[copied..start]);
                copied = index;
            } else {
                secret = true;
            }
        }
    }
    residual.push_str(&atom[copied..]);
    (secret, residual)
}

fn is_digest_context(before: &str, length: usize) -> bool {
    if before.ends_with("sha256:") {
        return true;
    }
    (length == 40 || length == 64) && (before.ends_with('@') || before.ends_with('#'))
}

/// Shannon entropy in bits per Unicode scalar.
fn shannon_entropy(text: &str) -> f64 {
    let mut counts: std::collections::HashMap<char, u32> = std::collections::HashMap::new();
    let mut total: u32 = 0;
    for c in text.chars() {
        *counts.entry(c).or_insert(0) += 1;
        total = total.saturating_add(1);
    }
    let total = f64::from(total);
    counts
        .values()
        .map(|&count| {
            let p = f64::from(count) / total;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_heuristic_matches_segments_case_insensitively() {
        for name in [
            "GITHUB_TOKEN",
            "token",
            "db-password",
            "PASSWD",
            "MY_PASS",
            "apikey",
            "aws_credentials",
            "CREDENTIAL",
            "session_cookie",
            "x-auth",
            "API_KEY",
            "my-api-key",
            "client_secret",
        ] {
            assert!(secret_name_heuristic(name), "{name}");
        }
        for name in ["LOG_LEVEL", "passage", "tokenizer", "author", "api", "KEYS"] {
            assert!(!secret_name_heuristic(name), "{name}");
        }
    }

    #[test]
    fn value_heuristic_rules() {
        assert_eq!(
            secret_value_heuristic("ghp_abcdef"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("Bearer sk-ant-xyz"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(secret_value_heuristic("sk-"), None);
        assert_eq!(
            secret_value_heuristic("eyJhbGciOi.eyJzdWIi.c2ln"),
            Some(SecretFinding::Jwt)
        );
        assert_eq!(
            secret_value_heuristic("0123456789abcdef0123456789abcdef"),
            Some(SecretFinding::Hex)
        );
        assert_eq!(
            secret_value_heuristic("aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0zLk"),
            Some(SecretFinding::Entropy)
        );
        assert_eq!(secret_value_heuristic("info"), None);
        assert_eq!(secret_value_heuristic(""), None);
    }

    #[test]
    fn digest_contexts_are_exempt() {
        let sha = "0123456789abcdef".repeat(4);
        assert_eq!(sha.len(), 64);
        assert_eq!(
            secret_value_heuristic(&format!("ghcr.io/org/img@sha256:{sha}")),
            None
        );
        assert_eq!(secret_value_heuristic(&format!("sha256:{sha}")), None);
        let commit = &sha[..40];
        assert_eq!(
            secret_value_heuristic(&format!("git+https://github.com/o/r@{commit}")),
            None
        );
        assert_eq!(
            secret_value_heuristic(&format!("github:o/r#{commit}")),
            None
        );
        // A 48-character run after `@` is not a commit.
        assert_eq!(
            secret_value_heuristic(&format!("pkg@{}", &sha[..48])),
            Some(SecretFinding::Hex)
        );
    }

    #[test]
    fn paths_and_urls_are_exempt_only_as_a_whole() {
        assert_eq!(
            secret_value_heuristic("@modelcontextprotocol/server-github@1.2.3"),
            None
        );
        assert_eq!(
            secret_value_heuristic("/usr/local/lib/node_modules/server/dist/index.js"),
            None
        );
        assert_eq!(
            secret_value_heuristic("/srv/aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0zLk/x"),
            Some(SecretFinding::Entropy)
        );
        assert_eq!(
            secret_value_heuristic("https://example.com/a/ghp_abc"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("https://example.com/a?token=x"),
            Some(SecretFinding::Name)
        );
        assert_eq!(
            secret_value_heuristic("https://example.com/a?q=ghp_x"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("https://example.com/a?flag#ghp_x"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("postgres://user:pw@db.internal/app"),
            Some(SecretFinding::Userinfo)
        );
        assert_eq!(
            secret_value_heuristic("https://mcp.linear.app/mcp?x=1#top"),
            None
        );
    }

    #[test]
    fn userinfo_is_found_anywhere_in_a_token() {
        for value in [
            "DATABASE_URL=postgres://user:hunter2@db/app",
            "-dsn=postgres://user:hunter2@db/app",
            "postgres://user:p/ss@db/app",
            "postgres://user:pa#ss@db/app",
            "postgres://user:pa?ss@db/app",
            "user:hunter2@tcp(db:3306)/app",
            "-dsn=user:hunter2@unix(/tmp/mysql.sock)/app",
            "https://user@host.example.com/x",
            "see https://a.example.com/?next=https://u:p@b.example.com",
        ] {
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Userinfo),
                "{value}"
            );
        }
        for value in [
            "git+https://github.com/o/r@0123456789abcdef0123456789abcdef01234567",
            "https://example.com/@scope/pkg",
            "user@example.com",
            "mailto:user@example.com",
            "pkg@tcp",
            "://u:p@x",
        ] {
            assert_ne!(
                secret_value_heuristic(value),
                Some(SecretFinding::Userinfo),
                "{value}"
            );
        }
    }

    #[test]
    fn base64_tokens_with_slashes_are_checked_whole() {
        assert_eq!(
            secret_value_heuristic("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEYxxAB"),
            Some(SecretFinding::Entropy)
        );
        assert_eq!(
            secret_value_heuristic("--secret-key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEYxxAB"),
            Some(SecretFinding::Name)
        );
        // Path-like tokens are still split into segments.
        for value in [
            "./node_modules/server/dist/index.js",
            "../aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0/x",
            "~/servers/aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0",
            "@scope/aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0",
            "github:org/aB3dE5gH7jK9mN1pQ2rS4tU6vW8xY0",
            "org/repo",
        ] {
            assert_eq!(secret_value_heuristic(value), None, "{value}");
        }
    }

    #[test]
    fn compound_tokens_are_split_at_equals_semicolon_and_comma() {
        // A plain-word value leaves a bare name unchecked; the prefix rule
        // still catches the value.
        assert_eq!(
            secret_value_heuristic("GITHUB_TOKEN=ghp_abcdef"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("host=db password=hunter2"),
            Some(SecretFinding::Name)
        );
        assert_eq!(
            secret_value_heuristic("key=ghp_abcdef"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("a,ghp_abcdef"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("a;xoxb-123"),
            Some(SecretFinding::Prefix)
        );
        assert_eq!(
            secret_value_heuristic("x=y;session=eyJa.eyJb.c2ln"),
            Some(SecretFinding::Jwt)
        );
        assert_eq!(
            secret_value_heuristic("ENDPOINT=https://h.example.com/?token=x"),
            Some(SecretFinding::Name)
        );
        assert_eq!(
            secret_value_heuristic("https://h.example.com/?scopes=a,ghp_x"),
            Some(SecretFinding::Prefix)
        );
        for value in [
            "--port=8080",
            "a=b,c=d",
            "level=info;mode=fast",
            "QUJD==",
            "=",
        ] {
            assert_eq!(secret_value_heuristic(value), None, "{value}");
        }
    }

    #[test]
    fn bare_name_equals_value_checks_the_name_only_for_secret_candidates() {
        for value in ["AUTH_MODE=oauth", "TOKEN_TYPE=bearer", "pass_through=1"] {
            assert_eq!(secret_value_heuristic(value), None, "{value}");
        }
        for value in [
            "password=hunter2",
            "PASSWD=x",
            "pwd=x",
            "Secret=x",
            "token=abc",
            "host=db password=hunter2",
            "GITHUB_TOKEN=hunter2!",
            "AUTH_MODE=has:colon",
            "AUTH_MODE=aVeryLongValueThatIsNotAPlainWord0",
            "AUTH_MODE=",
        ] {
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Name),
                "{value}"
            );
        }
        assert_eq!(
            secret_value_heuristic("GITHUB_TOKEN=ghp_abc123"),
            Some(SecretFinding::Prefix)
        );
        // A flag keeps the name heuristic whatever the value.
        for value in ["--auth-mode=oauth", "-password=x", "--token=short"] {
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Name),
                "{value}"
            );
        }
    }

    #[test]
    fn ports_and_query_colons_are_not_userinfo() {
        for value in [
            "https://mastodon.example.com:443/@me/mcp",
            "--registry=https://npm.example.com:4873/@scope/pkg",
            "http://localhost:8080/pkg@1.0",
            "https://h.example.com/x?t=10:00&e=a@b",
            "https://h.example.com/search?q=a:b&mail=x@y",
            "https://h.example.com:/x@y",
            "http://[::1]:8080/pkg@1.0",
            "https://h.example.com#a:b@c",
            // Known false negative: an all-digit password reads as a port.
            "postgres://user:1234/x@db/app",
        ] {
            assert_eq!(secret_value_heuristic(value), None, "{value}");
        }
        for value in [
            "postgres://user:65536/x@db/app",
            "postgres://user:12a/x@db/app",
            "http://[::1]:pw/x@h",
        ] {
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Userinfo),
                "{value}"
            );
        }
    }

    #[test]
    fn go_dsn_requires_no_slash_before_at() {
        assert!(contains_userinfo("user:hunter2@tcp(db:3306)/app"));
        assert!(!contains_userinfo("/srv/a:b@tcp(x)"));
        assert!(!contains_userinfo("dir/user:pw@unix(sock)"));
    }

    #[test]
    fn slashed_tokens_are_whole_only_when_base64_with_digit_or_plus() {
        for value in [
            "GoogleCloudPlatform/kubernetes-engine-samples",
            "Azure-Samples/azure-openai-mcp-server-quickstart",
            "awslabs/aws-documentation-mcp-server",
            "src/github/index",
            "--root=Documents/Projects/x",
        ] {
            assert_eq!(secret_value_heuristic(value), None, "{value}");
        }
        // Known false positives: these are path-like, but a single segment of
        // at least 32 characters reaches the entropy threshold on its own.
        for value in [
            "src/ModelContextProtocolServerGithubReferenceImpl/index",
            "--root=Documents/ProjectsArchiveAndOtherLongFolderNames/x",
        ] {
            assert!(is_path_like(value.trim_start_matches("--root=")), "{value}");
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Entropy),
                "{value}"
            );
        }
        for value in [
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEYxxAB",
            "wJalrXUtnFEMIK/MDENGbPxRfiCYEXAMPLEKEYxxAB+x==",
        ] {
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Entropy),
                "{value}"
            );
        }
        assert!(is_slashed_base64("ab/c1=="));
        assert!(!is_slashed_base64("ab/c1==="));
        assert!(!is_slashed_base64("ab/cd"));
        assert!(!is_slashed_base64("a-b/c1"));
        assert!(!is_slashed_base64("=="));
    }

    #[test]
    fn embedded_urls_get_the_url_checks() {
        for value in [
            "jdbc:postgresql://db/app?user=u&password=hunter2",
            "--url=jdbc:postgresql://db/app?password=hunter2",
            "jdbc:mysql://db/app?session_token=x",
        ] {
            assert_eq!(
                secret_value_heuristic(value),
                Some(SecretFinding::Name),
                "{value}"
            );
        }
        assert_eq!(
            secret_value_heuristic("jdbc:postgresql://db/app?q=ghp_abc"),
            Some(SecretFinding::Prefix)
        );
        for value in [
            "jdbc:postgresql://db/app?user=u&ssl=true",
            "jdbc:postgresql://db:5432/app",
        ] {
            assert_eq!(secret_value_heuristic(value), None, "{value}");
        }
    }

    #[test]
    fn url_shape_rejects_malformed_authorities() {
        for value in [
            "https://mcp.example.com/a b",
            "https://mcp.example.com/a\u{7}b",
            "https://[::1/mcp",
            "https://[::1]x/mcp",
            "https://mcp.example.com:abc/mcp",
            "https://mcp.example.com:65536/mcp",
            "https://mcp.example.com:-1/mcp",
            "https://mcp.example.com:+80/mcp",
        ] {
            assert_eq!(parse_url_shape(value), None, "{value:?}");
        }
        for value in [
            "https://mcp.example.com:443/mcp",
            "https://mcp.example.com:/mcp",
            "http://[::1]:65535/mcp",
            "http://[::1]/mcp",
        ] {
            assert!(parse_url_shape(value).is_some(), "{value}");
        }
    }

    #[test]
    fn variable_syntax_forms() {
        for value in ["$VAR", "a${VAR}b", "${env:VAR}", "{env:VAR}", "$_x"] {
            assert!(contains_variable_syntax(value), "{value}");
        }
        for value in ["$", "price $5", "a$ b", "{env}", "plain"] {
            assert!(!contains_variable_syntax(value), "{value}");
        }
    }

    #[test]
    fn url_shape_parsing() {
        let url = parse_url_shape("https://u:p@[::1]:8080/a/b?x=1#f").unwrap();
        assert_eq!(url.scheme, "https");
        assert_eq!(url.userinfo, Some("u:p"));
        assert_eq!(url.host, "::1");
        assert_eq!(url.path, "/a/b");
        assert_eq!(url.query, Some("x=1"));
        assert_eq!(url.fragment, Some("f"));
        let url = parse_url_shape("http://localhost:3000").unwrap();
        assert_eq!(url.host, "localhost");
        assert_eq!(url.path, "");
        assert_eq!(parse_url_shape("not a url"), None);
        assert_eq!(parse_url_shape("1http://x"), None);
        assert_eq!(parse_url_shape("ht tp://x"), None);
        assert_eq!(parse_url_shape("://x"), None);
    }
}
