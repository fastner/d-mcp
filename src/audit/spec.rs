//! The pin rules of a package spec, a VCS or URL spec and a container
//! image.

use super::AuditRule;
use super::tables::Family;

/// The rule a spec breaks, or `None` when it is pinned or not audited.
pub(super) fn check_spec(family: Family, spec: &str) -> Option<AuditRule> {
    if spec.is_empty() {
        return Some(family.rule());
    }
    let package_pinned: fn(&str) -> bool = match family {
        Family::Container => {
            return (!image_pinned(spec)).then_some(AuditRule::UnpinnedContainerImage);
        }
        Family::Npm => npm_pinned,
        Family::Python => python_pinned,
    };
    if is_local_path(spec) {
        return None;
    }
    if is_vcs_or_url(family, spec) {
        return (!has_commit(family, spec)).then_some(AuditRule::UnpinnedVcsSpec);
    }
    (!package_pinned(spec)).then_some(family.rule())
}

fn is_local_path(spec: &str) -> bool {
    spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with('~')
        || spec.starts_with("file:")
}

/// The prefixes of an npm Git spec.
const NPM_GIT_PREFIXES: &[&str] = &["git+", "git:", "github:", "gitlab:", "bitbucket:", "gist:"];

fn is_vcs_or_url(family: Family, spec: &str) -> bool {
    const URL_PREFIXES: &[&str] = &["http:", "https:", "ssh:"];
    if spec.contains("://")
        || NPM_GIT_PREFIXES
            .iter()
            .chain(URL_PREFIXES)
            .any(|prefix| spec.starts_with(prefix))
    {
        return true;
    }
    match family {
        Family::Npm => is_npm_shorthand(spec),
        Family::Python => spec.contains("@ git+") || spec.contains("@git+"),
        Family::Container => false,
    }
}

/// npm reads `org/repo` (not `@scope/pkg`) as a GitHub shorthand.
fn is_npm_shorthand(spec: &str) -> bool {
    let spec = spec.strip_prefix("npm:").unwrap_or(spec);
    !spec.starts_with('@') && !spec.contains(':') && spec.contains('/')
}

/// A full 40- or 64-character hex commit id.
fn is_commit(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether a VCS or URL spec is pinned by a commit of a Git transport in
/// ref position. A plain `http(s)` archive is never pinned: a hex run
/// elsewhere in it (a fragment, a path segment) proves nothing. (A
/// `#sha256=<64 hex>` fragment would not get here: the secret rules reject
/// it.)
fn has_commit(family: Family, spec: &str) -> bool {
    match family {
        Family::Npm => npm_commit(spec),
        Family::Python => python_commit(spec),
        Family::Container => false,
    }
}

/// `github:org/repo#<commit>`, `git+https://h/r.git#<commit>`,
/// `org/repo#<commit>`: the commit after the last `#`, up to the end.
fn npm_commit(spec: &str) -> bool {
    let git = NPM_GIT_PREFIXES
        .iter()
        .any(|prefix| spec.starts_with(prefix))
        || is_npm_shorthand(spec);
    git && spec
        .rsplit_once('#')
        .is_some_and(|(_, reference)| is_commit(reference))
}

/// `git+https://h/org/repo@<commit>` (optionally `name @ ` before it and a
/// `#egg=`/`#subdirectory=` fragment after it): the commit after the last
/// `@` of the URL path.
fn python_commit(spec: &str) -> bool {
    // Environment markers do not change the pin.
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    let Some(rest) = python_url(spec).strip_prefix("git+") else {
        return false;
    };
    let rest = rest.split('#').next().unwrap_or(rest);
    let Some((_, location)) = rest.split_once("://") else {
        return false;
    };
    let path = location.find('/').map_or("", |slash| &location[slash..]);
    path.rsplit_once('@')
        .is_some_and(|(_, reference)| is_commit(reference))
}

/// The URL of a Python spec: after `name @ ` or `name@`, when there is one.
fn python_url(spec: &str) -> &str {
    match spec.find("://") {
        Some(scheme_end) => {
            let start = spec[..scheme_end]
                .rfind([' ', '@'])
                .map_or(0, |separator| separator + 1);
            spec[start..].trim()
        }
        None => spec,
    }
}

/// `pkg@x.y.z` or `@scope/pkg@x.y.z` with a full SemVer version.
fn npm_pinned(spec: &str) -> bool {
    let spec = spec.strip_prefix("npm:").unwrap_or(spec);
    let (scope_offset, rest) = match spec.strip_prefix('@') {
        Some(rest) => (1, rest),
        None => (0, spec),
    };
    if scope_offset == 1 && !rest.contains('/') {
        return false;
    }
    match rest.rfind('@') {
        Some(at) => {
            let name = &rest[..at];
            !name.is_empty() && !name.ends_with('/') && is_semver(&rest[at + 1..])
        }
        None => false,
    }
}

/// A full SemVer version: `MAJOR.MINOR.PATCH` with an optional
/// pre-release and build part.
fn is_semver(version: &str) -> bool {
    let core = match version.split_once('+') {
        Some((core, build)) if is_identifier_list(build) => core,
        Some(_) => return false,
        None => version,
    };
    let (numbers, pre) = core
        .split_once('-')
        .map_or((core, None), |(n, p)| (n, Some(p)));
    if pre.is_some_and(|pre| !is_identifier_list(pre)) {
        return false;
    }
    let parts: Vec<&str> = numbers.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_identifier_list(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// `pkg==v`, `pkg===v` or uv's `pkg@v` with an exact PEP 440 version and
/// a plain package name (with optional extras) before it.
fn python_pinned(spec: &str) -> bool {
    // Environment markers do not change the pin.
    let spec = spec.split(';').next().unwrap_or(spec).trim();
    let (name, version) = if let Some(split) = spec.split_once("===") {
        split
    } else if let Some(split) = spec.split_once("==") {
        split
    } else if let Some(split) = spec.split_once('@') {
        split
    } else {
        return false;
    };
    is_python_name(name.trim()) && is_pep440_exact(version.trim())
}

/// A PEP 508 package name with an optional `[extra, ...]` list: anything
/// else before the version (`evil,dep==1.0`) is not one pinned package.
fn is_python_name(name: &str) -> bool {
    let (name, extras) = match name.split_once('[') {
        Some((name, extras)) => match extras.strip_suffix(']') {
            Some(extras) => (name.trim_end(), Some(extras)),
            None => return false,
        },
        None => (name, None),
    };
    let identifier = |text: &str| {
        text.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && text
                .bytes()
                .last()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    };
    identifier(name)
        && extras.is_none_or(|extras| extras.split(',').all(|extra| identifier(extra.trim())))
}

/// An exact PEP 440 version: it starts with a digit, has no wildcard and
/// uses only version characters.
fn is_pep440_exact(version: &str) -> bool {
    version
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_digit())
        && version.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'!' | b'+' | b'_' | b'-')
        })
}

/// `image@sha256:<64 hex>`, or a tag other than `latest` on the last path
/// component (a registry port is not a tag).
fn image_pinned(image: &str) -> bool {
    if let Some((_, digest)) = image.split_once("@sha256:") {
        return digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    let last = image.rsplit('/').next().unwrap_or(image);
    match last.split_once(':') {
        Some((name, tag)) => !name.is_empty() && !tag.is_empty() && tag != "latest",
        None => false,
    }
}
