//! Output: the JSON envelope, text rendering and warnings on stderr.
//!
//! With `--json`, stdout carries exactly one JSON document, the envelope:
//!
//! ```json
//! { "schema_version": 1, "ok": true, "data": {}, "problems": [], "error": null }
//! ```
//!
//! Warnings always go to stderr, so stdout stays valid JSON. Text output is
//! plain and does not depend on colour. Control characters in text output
//! (from file names, for example) are escaped `escape_debug`-style, so a
//! name cannot inject terminal escape sequences; JSON output escapes them
//! itself.

use std::io::{self, Write};

use serde_json::{Value, json};

use crate::error::{EXIT_FAILURE, Error};
use crate::resolver::Problem;

/// Schema version of the JSON envelope.
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// The successful result of a command.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// The command's `data` document.
    pub data: Value,
    /// Problems to report next to the data.
    pub problems: Vec<Problem>,
    /// The text rendering of `data` (without problems).
    pub text: String,
}

impl Report {
    /// A report without problems.
    #[must_use]
    pub fn new(data: Value, text: String) -> Self {
        Self {
            data,
            problems: Vec::new(),
            text,
        }
    }
}

/// Builds the JSON envelope.
#[must_use]
pub fn envelope(data: Option<&Value>, problems: &[Problem], error: Option<&Error>) -> Value {
    json!({
        "schema_version": ENVELOPE_SCHEMA_VERSION,
        "ok": error.is_none(),
        "data": data.cloned().unwrap_or(Value::Null),
        "problems": problems,
        "error": error.map(|error| json!({ "code": error.code(), "hint": error.hint() })),
    })
}

/// Escapes control characters `escape_debug`-style (`\u{1b}`, `\n`, ...),
/// except the ones in `keep`.
#[must_use]
pub fn escape_control(text: &str, keep: &[char]) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() && !keep.contains(&c) {
            escaped.extend(c.escape_debug());
        } else {
            escaped.push(c);
        }
    }
    escaped
}

/// Renders a problem as one text line, with control characters escaped.
#[must_use]
pub fn problem_line(problem: &Problem) -> String {
    let code = serde_json::to_value(problem.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut line = format!("{code}: {}", problem.message);
    if let (Some(line_number), Some(column)) = (problem.line, problem.column) {
        line.push_str(&format!(" (line {line_number}, column {column}"));
        if let Some(field) = &problem.field {
            line.push_str(&format!(", field `{field}`"));
        }
        line.push(')');
    }
    escape_control(&line, &[])
}

/// The terminal: stdout, stderr and the output mode.
pub struct Ui<'a> {
    json: bool,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
    write_failed: bool,
}

impl<'a> Ui<'a> {
    /// Creates a UI over the given streams.
    pub fn new(json: bool, out: &'a mut dyn Write, err: &'a mut dyn Write) -> Self {
        Self {
            json,
            out,
            err,
            write_failed: false,
        }
    }

    fn record(&mut self, result: io::Result<()>) {
        if let Err(error) = result
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            self.write_failed = true;
        }
    }

    /// Writes a warning line to stderr, in both modes. Control characters
    /// are escaped.
    pub fn warn(&mut self, message: &str) {
        let result = writeln!(self.err, "warning: {}", escape_control(message, &[]));
        self.record(result);
    }

    /// Prints a successful report and returns the exit code: 0, or 1 when
    /// stdout could not be written for a reason other than a broken pipe.
    pub fn success(&mut self, report: &Report) -> u8 {
        let result = if self.json {
            let document = envelope(Some(&report.data), &report.problems, None);
            writeln!(self.out, "{document:#}")
        } else {
            self.print_text(report)
        };
        self.record(result);
        if self.write_failed { EXIT_FAILURE } else { 0 }
    }

    fn print_text(&mut self, report: &Report) -> io::Result<()> {
        if !report.text.is_empty() {
            // Line breaks and tab separators belong to the layout.
            writeln!(self.out, "{}", escape_control(&report.text, &['\n', '\t']))?;
        }
        if !report.problems.is_empty() {
            writeln!(self.out, "problems:")?;
            for problem in &report.problems {
                writeln!(self.out, "  {}", problem_line(problem))?;
            }
        }
        Ok(())
    }

    /// Prints an error and returns its exit code. With `--json` the envelope
    /// goes to stdout; otherwise the message goes to stderr.
    pub fn failure(&mut self, error: &Error) -> u8 {
        let result = if self.json {
            let document = envelope(None, &[], Some(error));
            writeln!(self.out, "{document:#}")
        } else {
            writeln!(
                self.err,
                "error: {} [{}]",
                escape_control(&error.hint(), &[]),
                error.code()
            )
        };
        self.record(result);
        error.exit_code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::ProblemCode;

    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn problem() -> Problem {
        Problem {
            code: ProblemCode::InvalidDefinition,
            server: Some("a".into()),
            source: Some("local".into()),
            file: Some("servers/a.toml".into()),
            rule: Some(crate::rule::Rule::SecretHex),
            field: Some("env.X.value".into()),
            line: Some(3),
            column: Some(5),
            message: "servers/a.toml in source `local` breaks rule `secret-hex`".into(),
        }
    }

    #[test]
    fn envelope_shape() {
        let ok = envelope(Some(&json!({"x": 1})), &[], None);
        assert_eq!(
            ok,
            json!({"schema_version": 1, "ok": true, "data": {"x": 1}, "problems": [], "error": null})
        );
        let error = Error::HomeUnset;
        let failed = envelope(None, &[], Some(&error));
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["data"], Value::Null);
        assert_eq!(failed["error"]["code"], "home-unset");
    }

    #[test]
    fn text_and_json_rendering() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut report = Report::new(json!({}), "done".into());
        report.problems.push(problem());
        let mut ui = Ui::new(false, &mut out, &mut err);
        ui.warn("careful");
        assert_eq!(ui.success(&report), 0);
        assert_eq!(ui.failure(&Error::HomeUnset), 4);
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("done\nproblems:\n  invalid-definition: "));
        assert!(out.contains("(line 3, column 5, field `env.X.value`)"));
        let err = String::from_utf8(err).unwrap();
        assert!(err.starts_with("warning: careful\nerror: HOME is unset"));
        assert!(err.contains("[home-unset]"));

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut ui = Ui::new(true, &mut out, &mut err);
        assert_eq!(ui.success(&Report::new(json!({"a": 1}), String::new())), 0);
        let parsed: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed["data"]["a"], 1);
    }

    #[test]
    fn write_failures() {
        let mut out = Failing(io::ErrorKind::StorageFull);
        let mut err = Vec::new();
        let mut ui = Ui::new(false, &mut out, &mut err);
        assert_eq!(ui.success(&Report::new(json!({}), "x".into())), 1);
        let mut out = Failing(io::ErrorKind::BrokenPipe);
        let mut ui = Ui::new(true, &mut out, &mut err);
        assert_eq!(ui.success(&Report::new(json!({}), String::new())), 0);
        assert_eq!(ui.failure(&Error::PriorityInvalid), 1);
    }

    #[test]
    fn control_characters_are_escaped_in_text_output() {
        let mut p = problem();
        p.file = Some("servers/\u{1b}[31mred.toml".into());
        p.message = "servers/\u{1b}[31mred.toml in source `local` breaks rule `secret-hex`".into();
        let line = problem_line(&p);
        assert!(!line.contains('\u{1b}'), "{line}");
        assert!(line.contains("servers/\\u{1b}[31mred.toml"), "{line}");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut report = Report::new(json!({}), "a\tb\nc\u{7}d".into());
        report.problems.push(p.clone());
        let mut ui = Ui::new(false, &mut out, &mut err);
        ui.warn("bad\u{1b}name\nnext");
        assert_eq!(ui.success(&report), 0);
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("a\tb\nc\\u{7}d\n"), "{out:?}");
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: bad\\u{1b}name\\nnext\n"
        );
        // JSON output keeps the real value; serde_json escapes it.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut ui = Ui::new(true, &mut out, &mut err);
        let mut report = Report::new(json!({}), String::new());
        report.problems.push(p);
        assert_eq!(ui.success(&report), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\u{1b}'));
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["problems"][0]["file"], "servers/\u{1b}[31mred.toml");
    }

    #[test]
    fn problem_line_without_position() {
        let mut p = problem();
        p.line = None;
        assert_eq!(
            problem_line(&p),
            "invalid-definition: servers/a.toml in source `local` breaks rule `secret-hex`"
        );
    }
}
