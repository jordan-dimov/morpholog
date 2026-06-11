//! Diagnostics produced by the parser.
//!
//! Spans are byte offsets into the source string; rendering through
//! `ariadne` turns them into line/column references with caret
//! highlighting. The diagnostic type itself is `ariadne`-free so
//! callers that want plain text (CLI JSON, test assertions) can use
//! it directly without pulling in the rendering library at their
//! call site.

use std::fmt;
use std::ops::Range;

/// Byte-offset range into the parsed source. End is exclusive.
/// Compatible with `ariadne::Span` and `chumsky::span::SimpleSpan`.
pub type Span = Range<usize>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    /// Lint-grade: the finding deserves attention but does not fail
    /// the check (unless promoted). The parser never produces hints;
    /// the CLI renders [`morpholog_core::Lint`] findings at this
    /// severity through the same diagnostic shape.
    Hint,
}

/// One diagnostic emitted by the parser. Multiple diagnostics may
/// be returned from a single `parse_program` call: the parser
/// recovers at the next `predicate` or `program` keyword and keeps
/// going, so a `.morph` file with two malformed declarations yields
/// two diagnostics in one run.
///
/// `secondary_spans` carry related source locations for cases like
/// "predicate `Foo` declared at line 3, also declared at line 8":
/// both spans surface, the primary one carries the message.
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

    /// Render this diagnostic as a human-readable string with
    /// `ariadne`-style line/column markers. The CLI uses this when
    /// emitting to stderr; tests use the unrendered fields for
    /// structural assertions.
    pub fn render(&self, source_name: &str, source: &str) -> String {
        use ariadne::{Color, Label, Report, ReportKind, Source};
        // Lowercase custom kinds, so the rendered header reads
        // `error: ...` / `hint: ...` - the same prefix the plain-line
        // fallback prints when a finding has no source span.
        let (kind, color) = match self.severity {
            Severity::Error => (ReportKind::Custom("error", Color::Red), Color::Red),
            Severity::Hint => (ReportKind::Custom("hint", Color::Yellow), Color::Yellow),
        };
        // The primary label points, it does not repeat: the header
        // already holds the message, and repeating a teaching-length
        // message under the carets doubles the noise.
        let mut report = Report::build(kind, (source_name, self.primary.clone()))
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
        String::from_utf8(buf).unwrap_or_else(|_| {
            format!(
                "{} at bytes {:?}: {}",
                self.severity, self.primary, self.message
            )
        })
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

/// 1-based line and column for a byte offset into `source`. Columns
/// count bytes, which matches what editors and `ariadne` show for
/// ASCII-dominated `.morph` text. Offsets past the end clamp to it.
pub fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let prefix = &source[..offset];
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    (line, offset - line_start + 1)
}
