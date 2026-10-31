//! `approvals.toml`: source-qualified approvals, revoked markers and accepted
//! risks. This is the file layer: the data model, a strict parser, the
//! canonical rendering and the atomic write. The approval and audit gate
//! that consumes it runs at resolve time.
//!
//! ```toml
//! schema_version = 1
//!
//! [[approvals]]
//! source = "personal"
//! server = "github"
//! url = "git@github.com:me/mcp.git"
//! hashes = ["<definition_hash>", "<definition_hash>"]
//!
//! [[revoked]]
//! source = "personal"
//! server = "github"
//! url = "git@github.com:me/mcp.git"
//!
//! [[accepted_risks]]
//! source = "personal"
//! server = "github"
//! url = "git@github.com:me/mcp.git"
//! definition_hash = "<definition_hash>"
//! findings_hash = "<findings hash>"
//! reason = "pinned by digest upstream"
//! accepted_at = 1793462400
//! ```
//!
//! - An approval keeps a *set* of approved `definition_hash` values per
//!   (source, server), so a rollback to an earlier approved commit needs no
//!   re-approval. It applies only while the source's URL matches `url`.
//! - A revoked marker is recorded by `revoke`, which also drops the
//!   approvals and accepted risks of that (source, server); a later approval
//!   clears it. It records the source URL at revoke time, but blocks the
//!   (source, server) whatever the current URL (fail closed: a revoke is
//!   never undone by re-adding the source); a marker whose source is no
//!   longer a registered Git source with that URL is reported as orphaned.
//! - An accepted risk is bound to the source URL, the `definition_hash` and
//!   the findings hash; it does not approve, and it does not clear a revoked
//!   marker (so a revoked server with findings can be accepted, then
//!   approved). An approval under a new URL drops the risks accepted under
//!   another one.
//!
//! The file follows the `config.toml` rules: created on the first write,
//! written atomically under the store lock, parsed strictly. Any deviation
//! makes it corrupt (`approvals-corrupt`, exit 3) and it is never rewritten.
//! URLs are parsed with [`validate_url_syntax`] only: the secret rules run
//! when a URL is entered, so a later, stricter heuristic never turns the
//! file corrupt. [`save`] refuses to write what [`Approvals::parse`] would
//! reject.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use toml_edit::{Array, ArrayOfTables, DocumentMut, Table, value};

use crate::audit::{Finding, findings_hash};
use crate::config::SCHEMA_VERSION;
use crate::definition::is_valid_name;
use crate::error::{Error, Result};
use crate::git::validate_url_syntax;
use crate::resolver::GateVerdict;
use crate::spanned::{self, Entry, Node, NodeKind};
use crate::store::{StoreLock, StorePaths, StoreWarning, write_atomic};

/// Maximum length of a risk-acceptance reason, in characters.
pub const MAX_REASON_CHARS: usize = 500;

/// A (source, server) pair: the unit of approval.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApprovalKey {
    /// The source id.
    pub source: String,
    /// The server name.
    pub server: String,
}

impl ApprovalKey {
    /// A key from its parts.
    #[must_use]
    pub fn new(source: &str, server: &str) -> Self {
        Self {
            source: source.to_owned(),
            server: server.to_owned(),
        }
    }
}

/// The approved definition hashes of a (source, server), bound to a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// The source URL at approval time.
    pub url: String,
    /// The approved `definition_hash` values; never empty.
    pub hashes: BTreeSet<String>,
}

/// An accepted audit risk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRisk {
    /// The source URL at acceptance time.
    pub url: String,
    /// The `definition_hash` the acceptance is bound to.
    pub definition_hash: String,
    /// The hash of the audit findings (rule ids and matched arguments).
    pub findings_hash: String,
    /// The user's reason, at most [`MAX_REASON_CHARS`] characters.
    pub reason: String,
    /// When it was accepted, in Unix epoch seconds.
    pub accepted_at: u64,
}

/// The parsed `approvals.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approvals {
    approvals: BTreeMap<ApprovalKey, Approval>,
    /// Revoked markers, with the source URL at revoke time.
    revoked: BTreeMap<ApprovalKey, String>,
    risks: BTreeMap<(ApprovalKey, String), AcceptedRisk>,
}

/// What an orphaned entry of `approvals.toml` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OrphanKind {
    /// An approval or an accepted risk: it applies to nothing.
    Approval,
    /// A revoked marker: it still blocks its (source, server).
    Revoked,
}

/// An entry of `approvals.toml` whose source is not a registered Git
/// source with the recorded URL.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Orphan {
    /// The (source, server).
    pub key: ApprovalKey,
    /// What the entry is.
    pub kind: OrphanKind,
}

/// Whether text is a 64-character lowercase hex hash.
fn is_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Approvals {
    /// Whether `definition_hash` is approved for `key` under `url`.
    #[must_use]
    pub fn is_approved(&self, key: &ApprovalKey, url: &str, definition_hash: &str) -> bool {
        self.approvals.get(key).is_some_and(|approval| {
            approval.url == url && approval.hashes.contains(definition_hash)
        })
    }

    /// The approval record of `key`, whatever its URL.
    #[must_use]
    pub fn approval(&self, key: &ApprovalKey) -> Option<&Approval> {
        self.approvals.get(key)
    }

    /// Approves `definition_hash` for `key` under `url` and clears a revoked
    /// marker. An approval recorded under another URL is replaced, and risks
    /// accepted under another URL are dropped, so a re-added source with a
    /// new URL starts from scratch. Returns whether the hash was newly
    /// approved.
    pub fn approve(&mut self, key: &ApprovalKey, url: &str, definition_hash: &str) -> bool {
        self.revoked.remove(key);
        self.risks
            .retain(|(risk_key, _), risk| risk_key != key || risk.url == url);
        let approval = self
            .approvals
            .entry(key.clone())
            .or_insert_with(|| Approval {
                url: url.to_owned(),
                hashes: BTreeSet::new(),
            });
        if approval.url != url {
            approval.url = url.to_owned();
            approval.hashes.clear();
        }
        approval.hashes.insert(definition_hash.to_owned())
    }

    /// Whether `key` carries a revoked marker, whatever its URL.
    #[must_use]
    pub fn is_revoked(&self, key: &ApprovalKey) -> bool {
        self.revoked.contains_key(key)
    }

    /// The servers of `source` that carry a revoked marker, whatever its URL.
    pub fn revoked_servers<'a>(&'a self, source: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.revoked
            .keys()
            .filter(move |key| key.source == source)
            .map(|key| key.server.as_str())
    }

    /// Every (source, server) that carries a revoked marker, whatever its
    /// URL, in (source, server) order.
    pub fn revoked_keys(&self) -> impl Iterator<Item = &ApprovalKey> {
        self.revoked.keys()
    }

    /// Revokes `key` under the source URL `url`: drops its approvals and
    /// accepted risks and records a revoked marker. Returns whether anything
    /// was approved before.
    pub fn revoke(&mut self, key: &ApprovalKey, url: &str) -> bool {
        self.risks.retain(|(risk_key, _), _| risk_key != key);
        self.revoked.insert(key.clone(), url.to_owned());
        self.approvals.remove(key).is_some()
    }

    /// The accepted risk of `key` for `definition_hash` under `url`.
    #[must_use]
    pub fn accepted_risk(
        &self,
        key: &ApprovalKey,
        url: &str,
        definition_hash: &str,
    ) -> Option<&AcceptedRisk> {
        self.risks
            .get(&(key.clone(), definition_hash.to_owned()))
            .filter(|risk| risk.url == url)
    }

    /// Whether an accepted risk of `key` matches `definition_hash` under
    /// `url` and the hash of `findings`. No findings need no acceptance.
    #[must_use]
    pub fn risk_accepted(
        &self,
        key: &ApprovalKey,
        url: &str,
        definition_hash: &str,
        findings: &[Finding],
    ) -> bool {
        findings.is_empty()
            || self
                .accepted_risk(key, url, definition_hash)
                .is_some_and(|risk| risk.findings_hash == findings_hash(findings))
    }

    /// The gate verdict on a Git-source definition: `revoked` first, then
    /// `pending-approval` unless the hash is approved under `url`, then
    /// `risk-not-accepted` unless the findings are accepted.
    #[must_use]
    pub fn verdict(
        &self,
        key: &ApprovalKey,
        url: &str,
        definition_hash: &str,
        findings: &[Finding],
    ) -> GateVerdict {
        if self.is_revoked(key) {
            GateVerdict::Revoked
        } else if !self.is_approved(key, url, definition_hash) {
            GateVerdict::PendingApproval
        } else if !self.risk_accepted(key, url, definition_hash, findings) {
            GateVerdict::RiskNotAccepted
        } else {
            GateVerdict::Approved
        }
    }

    /// Records an accepted risk, replacing one for the same definition hash.
    pub fn accept_risk(&mut self, key: &ApprovalKey, risk: AcceptedRisk) {
        self.risks
            .insert((key.clone(), risk.definition_hash.clone()), risk);
    }

    /// The approvals, accepted risks and revoked markers whose source is
    /// not a registered Git source with the recorded URL, one per (key,
    /// kind). `url_of` returns the current URL of a registered Git source.
    #[must_use]
    pub fn orphaned<'a>(&self, url_of: impl Fn(&str) -> Option<&'a str>) -> Vec<Orphan> {
        let approvals = self
            .approvals
            .iter()
            .map(|(key, approval)| (key, approval.url.as_str(), OrphanKind::Approval));
        let risks = self
            .risks
            .iter()
            .map(|((key, _), risk)| (key, risk.url.as_str(), OrphanKind::Approval));
        let revoked = self
            .revoked
            .iter()
            .map(|(key, url)| (key, url.as_str(), OrphanKind::Revoked));
        let orphaned: BTreeSet<Orphan> = approvals
            .chain(risks)
            .chain(revoked)
            .filter(|(key, url, _)| url_of(&key.source) != Some(*url))
            .map(|(key, _, kind)| Orphan {
                key: key.clone(),
                kind,
            })
            .collect();
        orphaned.into_iter().collect()
    }

    /// Parses `approvals.toml` text.
    ///
    /// # Errors
    ///
    /// [`Error::ApprovalsCorrupt`] or [`Error::SchemaUnsupported`], naming
    /// `path`.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        match parse(text) {
            Ok(approvals) => Ok(approvals),
            Err(Ok(found)) => Err(Error::SchemaUnsupported {
                path: path.to_path_buf(),
                found,
            }),
            Err(Err(Corrupt { field, offset })) => {
                let (line, column) = spanned::line_column(text, offset.unwrap_or(0));
                Err(Error::ApprovalsCorrupt {
                    path: path.to_path_buf(),
                    line,
                    column,
                    field,
                })
            }
        }
    }

    /// Renders the canonical text, every list sorted by source and server.
    #[must_use]
    pub fn render(&self) -> String {
        let mut document = DocumentMut::new();
        document["schema_version"] = value(SCHEMA_VERSION);
        let key_table = |key: &ApprovalKey| {
            let mut table = Table::new();
            table["source"] = value(key.source.as_str());
            table["server"] = value(key.server.as_str());
            table
        };
        if !self.approvals.is_empty() {
            let mut array = ArrayOfTables::new();
            for (key, approval) in &self.approvals {
                let mut table = key_table(key);
                table["url"] = value(approval.url.as_str());
                let mut hashes = Array::new();
                for hash in &approval.hashes {
                    hashes.push(hash.as_str());
                }
                table["hashes"] = value(hashes);
                array.push(table);
            }
            document["approvals"] = toml_edit::Item::ArrayOfTables(array);
        }
        if !self.revoked.is_empty() {
            let mut array = ArrayOfTables::new();
            for (key, url) in &self.revoked {
                let mut table = key_table(key);
                table["url"] = value(url.as_str());
                array.push(table);
            }
            document["revoked"] = toml_edit::Item::ArrayOfTables(array);
        }
        if !self.risks.is_empty() {
            let mut array = ArrayOfTables::new();
            for ((key, _), risk) in &self.risks {
                let mut table = key_table(key);
                table["url"] = value(risk.url.as_str());
                table["definition_hash"] = value(risk.definition_hash.as_str());
                table["findings_hash"] = value(risk.findings_hash.as_str());
                table["reason"] = value(risk.reason.as_str());
                table["accepted_at"] = value(i64::try_from(risk.accepted_at).unwrap_or(i64::MAX));
                array.push(table);
            }
            document["accepted_risks"] = toml_edit::Item::ArrayOfTables(array);
        }
        document.to_string()
    }
}

/// A rule break: the field path and the byte offset.
struct Corrupt {
    field: String,
    offset: Option<usize>,
}

type Parse<T> = std::result::Result<T, Corrupt>;

fn corrupt<T>(field: impl Into<String>, offset: Option<usize>) -> Parse<T> {
    Err(Corrupt {
        field: field.into(),
        offset,
    })
}

fn find<'e>(entries: &'e [Entry], key: &str) -> Option<&'e Entry> {
    entries.iter().find(|entry| entry.key == key)
}

/// One table of an array of tables, with its field path.
struct Item<'e> {
    base: String,
    entries: &'e [Entry],
    offset: Option<usize>,
}

impl<'e> Item<'e> {
    fn field(&self, key: &str) -> String {
        format!("{}.{key}", self.base)
    }

    fn check_keys(&self, allowed: &[&str]) -> Parse<()> {
        match self
            .entries
            .iter()
            .find(|entry| !allowed.contains(&entry.key.as_str()))
        {
            Some(entry) => corrupt(self.base.clone(), entry.key_offset()),
            None => Ok(()),
        }
    }

    fn entry(&self, key: &str) -> Parse<&'e Entry> {
        find(self.entries, key).map_or_else(|| corrupt(self.field(key), self.offset), Ok)
    }

    fn string(&self, key: &str, valid: impl Fn(&str) -> bool) -> Parse<&'e str> {
        let entry = self.entry(key)?;
        match &entry.value.kind {
            NodeKind::String(text) if valid(text) => Ok(text),
            _ => corrupt(self.field(key), entry.value_offset()),
        }
    }

    fn key(&self) -> Parse<ApprovalKey> {
        Ok(ApprovalKey::new(
            self.string("source", is_valid_name)?,
            self.string("server", is_valid_name)?,
        ))
    }

    fn url(&self) -> Parse<&'e str> {
        self.string("url", |url| validate_url_syntax(url).is_ok())
    }
}

/// The tables of an optional array of tables.
fn items<'e>(root: &'e [Entry], name: &str) -> Parse<Vec<Item<'e>>> {
    let Some(entry) = find(root, name) else {
        return Ok(Vec::new());
    };
    let NodeKind::Array(nodes) = &entry.value.kind else {
        return corrupt(name, entry.value_offset());
    };
    nodes
        .iter()
        .enumerate()
        .map(|(position, node)| match &node.kind {
            NodeKind::Table(entries) => Ok(Item {
                base: format!("{name}[{position}]"),
                entries,
                offset: node.offset(),
            }),
            _ => corrupt(format!("{name}[{position}]"), node.offset()),
        })
        .collect()
}

/// `Err(Ok(version))` for an unsupported schema version.
fn parse(text: &str) -> std::result::Result<Approvals, std::result::Result<i64, Corrupt>> {
    let root = spanned::parse_document(text).map_err(|offset| {
        Err(Corrupt {
            field: String::new(),
            offset,
        })
    })?;
    let Some(version) = find(&root, "schema_version") else {
        return Err(corrupt("schema_version", None));
    };
    let NodeKind::Integer(found) = version.value.kind else {
        return Err(corrupt("schema_version", version.value_offset()));
    };
    if found != SCHEMA_VERSION {
        return Err(Ok(found));
    }
    parse_tables(&root).map_err(Err)
}

fn parse_tables(root: &[Entry]) -> Parse<Approvals> {
    const TOP: [&str; 4] = ["schema_version", "approvals", "revoked", "accepted_risks"];
    if let Some(entry) = root.iter().find(|entry| !TOP.contains(&entry.key.as_str())) {
        return corrupt("", entry.key_offset());
    }
    let mut approvals = Approvals::default();
    for item in items(root, "approvals")? {
        item.check_keys(&["source", "server", "url", "hashes"])?;
        let key = item.key()?;
        let url = item.url()?;
        let hashes_entry = item.entry("hashes")?;
        let NodeKind::Array(nodes) = &hashes_entry.value.kind else {
            return corrupt(item.field("hashes"), hashes_entry.value_offset());
        };
        let mut hashes = BTreeSet::new();
        for node in nodes {
            match &node.kind {
                NodeKind::String(hash) if is_hash(hash) && hashes.insert(hash.clone()) => {}
                _ => return corrupt(item.field("hashes"), node.offset()),
            }
        }
        if hashes.is_empty() {
            return corrupt(item.field("hashes"), hashes_entry.value_offset());
        }
        let approval = Approval {
            url: url.to_owned(),
            hashes,
        };
        if approvals.approvals.insert(key, approval).is_some() {
            return corrupt(item.base, item.offset);
        }
    }
    for item in items(root, "revoked")? {
        item.check_keys(&["source", "server", "url"])?;
        let key = item.key()?;
        let url = item.url()?.to_owned();
        // A revoked key has no approvals: `revoke` drops them and `approve`
        // clears the marker.
        if approvals.approvals.contains_key(&key) || approvals.revoked.insert(key, url).is_some() {
            return corrupt(item.base, item.offset);
        }
    }
    for item in items(root, "accepted_risks")? {
        item.check_keys(&[
            "source",
            "server",
            "url",
            "definition_hash",
            "findings_hash",
            "reason",
            "accepted_at",
        ])?;
        let key = item.key()?;
        let risk = AcceptedRisk {
            url: item.url()?.to_owned(),
            definition_hash: item.string("definition_hash", is_hash)?.to_owned(),
            findings_hash: item.string("findings_hash", is_hash)?.to_owned(),
            reason: item
                .string("reason", |reason| {
                    !reason.is_empty() && reason.chars().count() <= MAX_REASON_CHARS
                })?
                .to_owned(),
            accepted_at: accepted_at(&item)?,
        };
        let slot = (key, risk.definition_hash.clone());
        if approvals.risks.insert(slot, risk).is_some() {
            return corrupt(item.base, item.offset);
        }
    }
    Ok(approvals)
}

fn accepted_at(item: &Item<'_>) -> Parse<u64> {
    let entry = item.entry("accepted_at")?;
    match &entry.value {
        Node {
            kind: NodeKind::Integer(time),
            ..
        } => u64::try_from(*time).map_or_else(
            |_| corrupt(item.field("accepted_at"), entry.value_offset()),
            Ok,
        ),
        _ => corrupt(item.field("accepted_at"), entry.value_offset()),
    }
}

/// Loads `approvals.toml`; a missing file has no approvals.
///
/// # Errors
///
/// [`Error::ApprovalsCorrupt`] (also for a file that is not UTF-8),
/// [`Error::SchemaUnsupported`] and [`Error::Io`].
pub fn load(paths: &StorePaths) -> Result<Approvals> {
    let bytes = match std::fs::read(&paths.approvals) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Approvals::default());
        }
        Err(error) => return Err(Error::io("read", &paths.approvals, &error)),
    };
    let text = String::from_utf8(bytes).map_err(|_| Error::ApprovalsCorrupt {
        path: paths.approvals.clone(),
        line: 1,
        column: 1,
        field: String::new(),
    })?;
    Approvals::parse(&text, &paths.approvals)
}

/// Writes `approvals.toml` atomically. Call only while holding the lock.
/// The rendered text must parse back to `approvals`: content the parser
/// would reject (such as an invalid name) is never written, so a write can
/// never turn the file corrupt.
///
/// # Errors
///
/// [`Error::ApprovalsCorrupt`] (naming the would-be position) when the
/// rendered text does not round-trip, with nothing written; [`Error::Io`]
/// when the write fails; the old file then stays intact.
pub fn save(
    paths: &StorePaths,
    _lock: &StoreLock,
    approvals: &Approvals,
) -> Result<Option<StoreWarning>> {
    let text = approvals.render();
    match Approvals::parse(&text, &paths.approvals) {
        Ok(parsed) if parsed == *approvals => {}
        Ok(_) => {
            return Err(Error::ApprovalsCorrupt {
                path: paths.approvals.clone(),
                line: 1,
                column: 1,
                field: String::new(),
            });
        }
        Err(error) => return Err(error),
    }
    write_atomic(&paths.approvals, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "git@github.com:me/mcp.git";

    fn hash(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn risk(definition: char) -> AcceptedRisk {
        AcceptedRisk {
            url: URL.to_owned(),
            definition_hash: hash(definition),
            findings_hash: hash('f'),
            reason: "pinned upstream by digest".to_owned(),
            accepted_at: 1_793_462_400,
        }
    }

    fn field_of(text: &str) -> String {
        match Approvals::parse(text, Path::new("/s/approvals.toml")) {
            Err(Error::ApprovalsCorrupt { field, .. }) => field,
            other => panic!("expected approvals-corrupt, got {other:?}"),
        }
    }

    #[test]
    fn approve_revoke_and_accept() {
        let key = ApprovalKey::new("personal", "github");
        let mut approvals = Approvals::default();
        assert!(!approvals.is_approved(&key, URL, &hash('a')));
        assert!(approvals.approve(&key, URL, &hash('a')));
        assert!(approvals.approve(&key, URL, &hash('b')));
        assert!(!approvals.approve(&key, URL, &hash('b')));
        // A rollback to an earlier approved hash stays approved.
        assert!(approvals.is_approved(&key, URL, &hash('a')));
        // The approval is bound to the URL.
        assert!(!approvals.is_approved(&key, "https://other/x.git", &hash('a')));
        approvals.accept_risk(&key, risk('a'));
        assert!(approvals.accepted_risk(&key, URL, &hash('a')).is_some());
        assert!(
            approvals
                .accepted_risk(&key, "https://other/x.git", &hash('a'))
                .is_none()
        );
        assert!(approvals.accepted_risk(&key, URL, &hash('b')).is_none());
        assert!(approvals.revoke(&key, URL));
        assert!(approvals.is_revoked(&key));
        assert!(approvals.approval(&key).is_none());
        assert!(approvals.accepted_risk(&key, URL, &hash('a')).is_none());
        assert!(!approvals.revoke(&key, URL));
        assert_eq!(
            approvals.revoked_servers("personal").collect::<Vec<_>>(),
            ["github"]
        );
        assert_eq!(approvals.revoked_servers("other").count(), 0);
        // A later approval clears the marker; a new URL starts from scratch.
        approvals.approve(&key, "https://new/x.git", &hash('c'));
        assert!(!approvals.is_revoked(&key));
        assert_eq!(approvals.approval(&key).unwrap().hashes.len(), 1);
        approvals.approve(&key, URL, &hash('d'));
        assert!(!approvals.is_approved(&key, URL, &hash('c')));
    }

    #[test]
    fn verdicts() {
        use crate::audit::audit_command;
        let key = ApprovalKey::new("personal", "fetch");
        let findings = audit_command("uvx", &["fetch".to_owned()]);
        let other = audit_command("uvx", &["other".to_owned()]);
        let mut approvals = Approvals::default();
        let verdict = |approvals: &Approvals, findings: &[Finding]| {
            approvals.verdict(&key, URL, &hash('a'), findings)
        };
        assert_eq!(verdict(&approvals, &[]), GateVerdict::PendingApproval);
        approvals.approve(&key, URL, &hash('a'));
        assert_eq!(verdict(&approvals, &[]), GateVerdict::Approved);
        assert_eq!(verdict(&approvals, &findings), GateVerdict::RiskNotAccepted);
        approvals.accept_risk(
            &key,
            AcceptedRisk {
                findings_hash: findings_hash(&findings),
                ..risk('a')
            },
        );
        assert_eq!(verdict(&approvals, &findings), GateVerdict::Approved);
        // Changed findings invalidate the acceptance.
        assert_eq!(verdict(&approvals, &other), GateVerdict::RiskNotAccepted);
        assert!(!approvals.risk_accepted(&key, "https://h/x.git", &hash('a'), &findings));
        approvals.revoke(&key, URL);
        assert_eq!(verdict(&approvals, &[]), GateVerdict::Revoked);
        // The marker blocks whatever the current URL (fail closed).
        assert_eq!(
            approvals.verdict(&key, "https://new/x.git", &hash('a'), &[]),
            GateVerdict::Revoked
        );
        // A risk accepted after the revoke keeps the marker until approval.
        approvals.accept_risk(
            &key,
            AcceptedRisk {
                findings_hash: findings_hash(&findings),
                ..risk('a')
            },
        );
        assert!(approvals.is_revoked(&key));
        let parsed = Approvals::parse(&approvals.render(), Path::new("x")).unwrap();
        assert_eq!(parsed, approvals);
        approvals.approve(&key, URL, &hash('a'));
        assert_eq!(verdict(&approvals, &findings), GateVerdict::Approved);
    }

    #[test]
    fn round_trips_and_reports_orphans() {
        let mut approvals = Approvals::default();
        let github = ApprovalKey::new("personal", "github");
        let linear = ApprovalKey::new("team", "linear");
        let gone = ApprovalKey::new("gone", "x");
        approvals.approve(&github, URL, &hash('a'));
        approvals.approve(&github, URL, &hash('b'));
        approvals.approve(&linear, "https://h/team/defs.git", &hash('c'));
        approvals.accept_risk(&gone, risk('e'));
        let slack = ApprovalKey::new("personal", "slack");
        approvals.revoke(&slack, URL);
        let old = ApprovalKey::new("team", "old");
        approvals.revoke(&old, "https://h/team/defs.git");
        let text = approvals.render();
        assert!(text.contains("[[revoked]]\nsource = \"personal\"\nserver = \"slack\"\nurl = "));
        let parsed = Approvals::parse(&text, Path::new("x")).unwrap();
        assert_eq!(parsed, approvals);
        assert_eq!(parsed.render(), text);
        let orphaned = parsed.orphaned(|source| match source {
            "personal" => Some(URL),
            "team" => Some("https://h/team/other.git"),
            _ => None,
        });
        let orphan = |key: &ApprovalKey, kind| Orphan {
            key: key.clone(),
            kind,
        };
        // The marker of `personal` matches its URL; the one of `team` does
        // not and is reported, as a revoked marker.
        assert_eq!(
            orphaned,
            [
                orphan(&gone, OrphanKind::Approval),
                orphan(&linear, OrphanKind::Approval),
                orphan(&old, OrphanKind::Revoked),
            ]
        );
        // A removed source orphans its marker too.
        let orphaned =
            parsed.orphaned(|source| (source == "team").then_some("https://h/team/defs.git"));
        assert!(orphaned.contains(&orphan(&slack, OrphanKind::Revoked)));
        assert!(!orphaned.contains(&orphan(&old, OrphanKind::Revoked)));
        assert_eq!(Approvals::default().render(), "schema_version = 1\n");
    }

    #[test]
    fn strict_parse() {
        let v = "schema_version = 1\n";
        let a = hash('a');
        let approval = format!(
            "[[approvals]]\nsource = \"p\"\nserver = \"g\"\nurl = \"{URL}\"\nhashes = [\"{a}\"]\n"
        );
        assert!(Approvals::parse(&format!("{v}{approval}"), Path::new("x")).is_ok());
        let risk = format!(
            "[[accepted_risks]]\nsource = \"p\"\nserver = \"g\"\nurl = \"{URL}\"\n\
             definition_hash = \"{a}\"\nfindings_hash = \"{a}\"\nreason = \"r\"\naccepted_at = 1\n"
        );
        assert!(Approvals::parse(&format!("{v}{risk}"), Path::new("x")).is_ok());
        let revoked = format!("[[revoked]]\nsource = \"p\"\nserver = \"g\"\nurl = \"{URL}\"\n");
        assert!(Approvals::parse(&format!("{v}{revoked}"), Path::new("x")).is_ok());
        let cases = [
            (format!("{v}x = 1\n"), ""),
            ("approvals = []\n".to_owned(), "schema_version"),
            ("schema_version = true\n".to_owned(), "schema_version"),
            ("[x\n".to_owned(), ""),
            (format!("{v}approvals = 1\n"), "approvals"),
            (format!("{v}revoked = [1]\n"), "revoked[0]"),
            (
                format!("{v}{}", approval.replace("url", "uri")),
                "approvals[0]",
            ),
            (
                format!("{v}{}", approval.replace("\"p\"", "\"P\"")),
                "approvals[0].source",
            ),
            (
                format!("{v}{}", approval.replace(URL, "https://u:p@h/x")),
                "approvals[0].url",
            ),
            (
                format!("{v}{}", approval.replace(&a, "abc")),
                "approvals[0].hashes",
            ),
            (
                format!("{v}{}", approval.replace(&format!("[\"{a}\"]"), "[]")),
                "approvals[0].hashes",
            ),
            (
                format!("{v}{}", approval.replace(&format!("[\"{a}\"]"), "\"x\"")),
                "approvals[0].hashes",
            ),
            (
                format!(
                    "{v}{}",
                    approval.replace(&format!("[\"{a}\"]"), &format!("[\"{a}\", \"{a}\"]"))
                ),
                "approvals[0].hashes",
            ),
            (format!("{v}{approval}{approval}"), "approvals[1]"),
            (
                format!("{v}{}", approval.replace("server = \"g\"\n", "")),
                "approvals[0].server",
            ),
            (format!("{v}{approval}{revoked}"), "revoked[0]"),
            (format!("{v}{revoked}{revoked}"), "revoked[1]"),
            (format!("{v}{revoked}x = 1\n"), "revoked[0]"),
            (
                format!("{v}[[revoked]]\nsource = \"p\"\nserver = \"g\"\n"),
                "revoked[0].url",
            ),
            (
                format!("{v}{}", revoked.replace(URL, "ext::sh")),
                "revoked[0].url",
            ),
            (
                format!("{v}{}", revoked.replace("\"g\"", "\"Bad Name\"")),
                "revoked[0].server",
            ),
            (
                format!("{v}{}", risk.replace("reason = \"r\"", "reason = \"\"")),
                "accepted_risks[0].reason",
            ),
            (
                format!(
                    "{v}{}",
                    risk.replace(
                        "reason = \"r\"",
                        &format!("reason = \"{}\"", "r".repeat(501))
                    )
                ),
                "accepted_risks[0].reason",
            ),
            (
                format!("{v}{}", risk.replace("accepted_at = 1", "accepted_at = -1")),
                "accepted_risks[0].accepted_at",
            ),
            (
                format!(
                    "{v}{}",
                    risk.replace("accepted_at = 1", "accepted_at = \"1\"")
                ),
                "accepted_risks[0].accepted_at",
            ),
            (format!("{v}{risk}{risk}"), "accepted_risks[1]"),
        ];
        // A risk accepted after a revoke keeps the marker.
        assert!(Approvals::parse(&format!("{v}{revoked}{risk}"), Path::new("x")).is_ok());
        // Stored URLs get the syntax check only: a URL the secret rules
        // would now reject on input still parses.
        let secretish = "https://h/ghp_abcdefghijklmnopqrstuvwxyz0123456789/defs.git";
        assert!(crate::git::validate_url(secretish).is_err());
        assert!(
            Approvals::parse(
                &format!("{v}{}", approval.replace(URL, secretish)),
                Path::new("x")
            )
            .is_ok()
        );
        for (text, field) in cases {
            assert_eq!(field_of(&text), field, "{text}");
        }
        assert!(matches!(
            Approvals::parse("schema_version = 3\n", Path::new("x")),
            Err(Error::SchemaUnsupported { found: 3, .. })
        ));
    }

    #[test]
    fn approving_under_a_new_url_drops_risks_of_the_old_one() {
        let key = ApprovalKey::new("personal", "fetch");
        let mut approvals = Approvals::default();
        approvals.approve(&key, URL, &hash('a'));
        approvals.accept_risk(&key, risk('a'));
        approvals.accept_risk(&key, risk('b'));
        let other = ApprovalKey::new("personal", "other");
        approvals.accept_risk(&other, risk('a'));
        // The same URL keeps them.
        approvals.approve(&key, URL, &hash('b'));
        assert!(approvals.accepted_risk(&key, URL, &hash('a')).is_some());
        // A new URL drops them, so nothing is left to orphan.
        let new = "https://h/new/defs.git";
        approvals.approve(&key, new, &hash('a'));
        assert!(approvals.accepted_risk(&key, URL, &hash('a')).is_none());
        assert!(approvals.accepted_risk(&key, URL, &hash('b')).is_none());
        assert!(approvals.accepted_risk(&other, URL, &hash('a')).is_some());
        let orphaned = approvals.orphaned(|_| Some(new));
        assert_eq!(
            orphaned,
            [Orphan {
                key: other,
                kind: OrphanKind::Approval
            }]
        );
    }

    #[test]
    fn save_refuses_what_parse_would_reject() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        let lock = StoreLock::acquire(&paths).unwrap();
        let mut approvals = Approvals::default();
        approvals.approve(&ApprovalKey::new("p", "g"), URL, &hash('1'));
        save(&paths, &lock, &approvals).unwrap();
        let before = std::fs::read(&paths.approvals).unwrap();
        for bad in [
            ApprovalKey::new("p", "Bad Name"),
            ApprovalKey::new("Bad Source", "g"),
        ] {
            let mut broken = approvals.clone();
            broken.revoke(&bad, URL);
            assert!(matches!(
                save(&paths, &lock, &broken),
                Err(Error::ApprovalsCorrupt { .. })
            ));
            assert_eq!(std::fs::read(&paths.approvals).unwrap(), before);
        }
        let mut broken = approvals.clone();
        broken.approve(&ApprovalKey::new("p", "h"), "ext::sh", &hash('2'));
        assert!(save(&paths, &lock, &broken).is_err());
        let mut broken = approvals.clone();
        broken.accept_risk(
            &ApprovalKey::new("p", "g"),
            AcceptedRisk {
                reason: String::new(),
                ..risk('1')
            },
        );
        assert!(save(&paths, &lock, &broken).is_err());
        assert_eq!(load(&paths).unwrap(), approvals);
    }

    #[test]
    fn load_and_save() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StorePaths::new(temp.path().to_path_buf());
        assert_eq!(load(&paths).unwrap(), Approvals::default());
        let lock = StoreLock::acquire(&paths).unwrap();
        let mut approvals = Approvals::default();
        approvals.approve(&ApprovalKey::new("p", "g"), URL, &hash('1'));
        save(&paths, &lock, &approvals).unwrap();
        assert_eq!(load(&paths).unwrap(), approvals);
        std::fs::write(&paths.approvals, b"\xff").unwrap();
        assert!(matches!(load(&paths), Err(Error::ApprovalsCorrupt { .. })));
        std::fs::remove_file(&paths.approvals).unwrap();
        std::fs::create_dir(&paths.approvals).unwrap();
        assert!(matches!(load(&paths), Err(Error::Io { .. })));
    }
}
