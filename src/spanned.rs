//! Phase one of the two-phase parsing: an untyped, spanned TOML tree.
//!
//! The TOML text is parsed with `toml_edit`'s immutable [`toml_edit::Document`],
//! which keeps byte spans, and converted into [`Node`] values. Phase two, the
//! d-mcp walkers in [`crate::definition`] and [`crate::config`], validate this
//! tree, track the field path and map every failure to a rule code.
//!
//! No message text from `toml_edit` is ever kept: a syntax error is reduced to
//! its position, so a secret inside a malformed file cannot leak through an
//! error message.

use std::ops::Range;

use toml_edit::{Array, InlineTable, Item, Table, Value};

/// A TOML value with the byte span it was parsed from.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// The value.
    pub kind: NodeKind,
    /// Byte range in the source text, when `toml_edit` reports one.
    pub span: Option<Range<usize>>,
}

/// The untyped TOML value kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    /// A string.
    String(String),
    /// An integer.
    Integer(i64),
    /// A float, possibly NaN or infinite.
    Float(f64),
    /// A boolean.
    Boolean(bool),
    /// A datetime in its RFC 3339 text form, with a `+00:00` offset written
    /// as `Z`. `-00:00` stays distinct, as RFC 3339 gives it its own meaning
    /// (an unknown local offset).
    Datetime(String),
    /// An array or an array of tables.
    Array(Vec<Node>),
    /// A standard or inline table, in source order.
    Table(Vec<Entry>),
}

/// A key-value pair of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The key.
    pub key: String,
    /// Byte range of the key in the source text.
    pub key_span: Option<Range<usize>>,
    /// The value.
    pub value: Node,
}

impl Entry {
    /// The best start offset for a problem with this entry's value: the value
    /// span, falling back to the key span.
    #[must_use]
    pub fn value_offset(&self) -> Option<usize> {
        self.value.offset().or_else(|| self.key_offset())
    }

    /// The start offset of the key.
    #[must_use]
    pub fn key_offset(&self) -> Option<usize> {
        self.key_span.as_ref().map(|span| span.start)
    }
}

impl Node {
    /// The start offset of this node.
    #[must_use]
    pub fn offset(&self) -> Option<usize> {
        self.span.as_ref().map(|span| span.start)
    }

    /// The name of this node's kind, for diagnostics.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            NodeKind::String(_) => "string",
            NodeKind::Integer(_) => "integer",
            NodeKind::Float(_) => "float",
            NodeKind::Boolean(_) => "boolean",
            NodeKind::Datetime(_) => "datetime",
            NodeKind::Array(_) => "array",
            NodeKind::Table(_) => "table",
        }
    }
}

/// Parses TOML text into a spanned root table.
///
/// # Errors
///
/// Returns the byte offset of a syntax error, if `toml_edit` reports one. The
/// error message itself is dropped on purpose.
pub fn parse_document(text: &str) -> Result<Vec<Entry>, Option<usize>> {
    let document =
        toml_edit::Document::parse(text).map_err(|error| error.span().map(|span| span.start))?;
    Ok(table_entries(text, document.as_table()))
}

/// Converts a byte offset into a 1-based line and column (counted in Unicode
/// scalars). Offsets past the end clamp to the end of the text.
#[must_use]
pub fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

fn table_entries(text: &str, table: &Table) -> Vec<Entry> {
    table
        .iter()
        .filter_map(|(key, item)| {
            let value = item_node(text, item)?;
            Some(Entry {
                key: key.to_owned(),
                key_span: table.key(key).and_then(toml_edit::Key::span),
                value,
            })
        })
        .collect()
}

fn inline_entries(text: &str, table: &InlineTable) -> Vec<Entry> {
    table
        .iter()
        .map(|(key, value)| Entry {
            key: key.to_owned(),
            key_span: table.key(key).and_then(toml_edit::Key::span),
            value: value_node(text, value),
        })
        .collect()
}

fn item_node(text: &str, item: &Item) -> Option<Node> {
    match item {
        Item::None => None,
        Item::Value(value) => Some(value_node(text, value)),
        Item::Table(table) => Some(Node {
            kind: NodeKind::Table(table_entries(text, table)),
            span: table.span(),
        }),
        Item::ArrayOfTables(tables) => Some(Node {
            kind: NodeKind::Array(
                tables
                    .iter()
                    .map(|table| Node {
                        kind: NodeKind::Table(table_entries(text, table)),
                        span: table.span(),
                    })
                    .collect(),
            ),
            span: tables.span(),
        }),
    }
}

fn array_nodes(text: &str, array: &Array) -> Vec<Node> {
    array.iter().map(|value| value_node(text, value)).collect()
}

/// Renders a datetime in RFC 3339 form with a zero offset written as `Z`.
///
/// `toml_datetime` parses both `+00:00` and `-00:00` into the same zero
/// offset and prints it as `+00:00`, so the source text decides whether the
/// offset was `-00:00`.
fn datetime_text(rendered: String, raw: Option<&str>) -> String {
    let Some(base) = rendered.strip_suffix("+00:00") else {
        return rendered;
    };
    if raw.is_some_and(|raw| raw.trim_end().ends_with("-00:00")) {
        format!("{base}-00:00")
    } else {
        format!("{base}Z")
    }
}

fn value_node(text: &str, value: &Value) -> Node {
    let kind = match value {
        Value::String(text) => NodeKind::String(text.value().clone()),
        Value::Integer(number) => NodeKind::Integer(*number.value()),
        Value::Float(number) => NodeKind::Float(*number.value()),
        Value::Boolean(flag) => NodeKind::Boolean(*flag.value()),
        Value::Datetime(datetime) => NodeKind::Datetime(datetime_text(
            datetime.value().to_string(),
            value.span().and_then(|span| text.get(span)),
        )),
        Value::Array(array) => NodeKind::Array(array_nodes(text, array)),
        Value::InlineTable(table) => NodeKind::Table(inline_entries(text, table)),
    };
    Node {
        kind,
        span: value.span(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_column_counts_from_one() {
        let text = "a = 1\nbé = 2\n";
        assert_eq!(line_column(text, 0), (1, 1));
        assert_eq!(line_column(text, 6), (2, 1));
        // Offset inside the two-byte `é` clamps to its start.
        assert_eq!(line_column(text, 8), (2, 2));
        assert_eq!(line_column(text, 9), (2, 3));
        assert_eq!(line_column(text, 1_000), (3, 1));
    }

    #[test]
    fn zero_offsets_are_normalized_but_negative_zero_stays_distinct() {
        let datetime = |raw: &str| {
            let entries = parse_document(&format!("d = {raw}\n")).unwrap();
            match &entries[0].value.kind {
                NodeKind::Datetime(text) => text.clone(),
                other => panic!("expected a datetime, got {other:?}"),
            }
        };
        assert_eq!(
            datetime("1979-05-27T07:32:00+00:00"),
            "1979-05-27T07:32:00Z"
        );
        assert_eq!(datetime("1979-05-27T07:32:00Z"), "1979-05-27T07:32:00Z");
        assert_eq!(
            datetime("1979-05-27T07:32:00-00:00"),
            "1979-05-27T07:32:00-00:00"
        );
        assert_eq!(
            datetime("1979-05-27T07:32:00+01:00"),
            "1979-05-27T07:32:00+01:00"
        );
        assert_eq!(datetime("1979-05-27T07:32:00"), "1979-05-27T07:32:00");
        assert_eq!(datetime_text("x".to_owned(), None), "x");
        assert_eq!(datetime_text("t+00:00".to_owned(), None), "tZ");
    }

    #[test]
    fn syntax_error_reports_only_an_offset() {
        let error = parse_document("a = \"ghp_secret\nb = 2").unwrap_err();
        assert!(error.is_some());
    }

    #[test]
    fn converts_every_kind() {
        let entries = parse_document(
            "s = \"x\"\ni = 1\nf = 1.5\nb = true\nd = 1979-05-27T07:32:00Z\n\
             a = [1, 2]\nt = { k = 1 }\n[[aot]]\nk = 1\n[table]\nk = 2\n",
        )
        .unwrap();
        let kinds: Vec<_> = entries
            .iter()
            .map(|entry| entry.value.kind_name())
            .collect();
        assert_eq!(
            kinds,
            [
                "string", "integer", "float", "boolean", "datetime", "array", "table", "array",
                "table"
            ]
        );
        let NodeKind::Datetime(text) = &entries[4].value.kind else {
            panic!("expected a datetime");
        };
        assert_eq!(text, "1979-05-27T07:32:00Z");
        assert!(entries[0].key_offset().is_some());
        assert!(entries[0].value_offset().is_some());
    }
}
