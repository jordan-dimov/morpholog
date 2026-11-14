//! Diagnostics produced by the parser.
//!
//! Spans are byte offsets into the source. The type itself does not depend on `ariadne`, so
//! plain-text callers (CLI JSON, tests) can use it; [`crate::SourceText::render`] draws the carets.

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
