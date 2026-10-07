//! Diagnostics produced by the parser.
//!
//! Spans are byte offsets into the source. The type itself does not depend on `ariadne`, so
//! plain-text callers (CLI JSON, tests) can use it; [`Diagnostic::render`] draws the carets.

use std::fmt;
use std::ops::Range;

/// Byte-offset range into the parsed source. End is exclusive.
/// Compatible with `ariadne::Span` and `chumsky::span::SimpleSpan`.
pub type Span = Range<usize>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    /// Worth attention but does not fail the check unless promoted. The parser never produces
    /// hints; the CLI uses this for [`morpholog_core::Lint`] findings.
    Hint,
}

/// One diagnostic emitted by the parser. One `parse_program` call can return several: the
/// parser skips to the next top-level declaration after an error and keeps going.
///
/// `secondary` holds related locations, such as the earlier declaration of a duplicate name.
/// The primary span carries the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub primary: Span,
    pub secondary: Vec<(Span, String)>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>, primary: Span) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            primary,
            secondary: Vec::new(),
        }
    }

    pub fn hint(message: impl Into<String>, primary: Span) -> Self {
        Self {
            severity: Severity::Hint,
            message: message.into(),
            primary,
            secondary: Vec::new(),
        }
    }

    pub fn with_secondary(mut self, span: Span, note: impl Into<String>) -> Self {
        self.secondary.push((span, note.into()));
        self
    }

    /// Render this diagnostic as human-readable text with `ariadne` line/column markers, in
    /// colour only when `colour` is set: ariadne colours each character, so uncoloured text is
    /// the only kind a pipe or a log can read.
    pub fn render(&self, source_name: &str, source: &str, colour: bool) -> String {
        use ariadne::{Color, Config, Label, Report, ReportKind, Source};
        // Lowercase, to match the `error:` / `hint:` prefix printed for findings with no span.
        let (name, color) = match self.severity {
            Severity::Error => ("error", Color::Red),
            Severity::Hint => ("hint", Color::Yellow),
        };
        let kind = ReportKind::Custom(name, color);
        // The header already shows the message; repeating it under the carets is noise.
        let mut report = Report::build(kind, (source_name, self.primary.clone()))
            .with_config(Config::default().with_color(colour))
            .with_message(&self.message)
            .with_label(
                Label::new((source_name, self.primary.clone()))
                    .with_message("here")
                    .with_color(color),
            );
        for (span, note) in &self.secondary {
            report = report.with_label(
                Label::new((source_name, span.clone()))
                    .with_message(note)
                    .with_color(Color::Yellow),
            );
        }
        let mut buf = Vec::new();
        if report
            .finish()
            .write((source_name, Source::from(source)), &mut buf)
            .is_err()
        {
            return format!(
                "{} at bytes {:?}: {}",
                self.severity, self.primary, self.message
            );
        }
        let Ok(rendered) = String::from_utf8(buf) else {
            return format!(
                "{} at bytes {:?}: {}",
                self.severity, self.primary, self.message
            );
        };
        if colour {
            return rendered;
        }
        // ariadne colours a custom kind's header whatever the config says, so
        // put back the plain one.
        let header = format!("{name}:");
        rendered.replacen(&ariadne::Fmt::fg(&header, color).to_string(), &header, 1)
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Hint => write!(f, "hint"),
        }
    }
}

/// 1-based line and column for a byte offset into `source`. Columns count bytes, which matches
/// editors and `ariadne` for mostly-ASCII `.morph` text. Offsets past the end clamp to it.
///
/// Scans the prefix on every call. That is fine for a handful of findings over a small file; an
/// editor making many lookups would want a precomputed line index.
pub fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let prefix = &source[..offset];
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    (line, offset - line_start + 1)
}
