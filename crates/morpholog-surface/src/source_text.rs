//! A programme's text: one file, or the parts a `.parts` manifest lists, read as one text.
//!
//! Parts are joined in order into one string, so every span stays a byte range into one text and
//! the parser never sees a file boundary. Each part keeps its base offset, which places a span
//! back in the file and line a reader sees. The join is refused where a construct could run from
//! one part into the next: an open parenthesis, a missing final newline, an indented start.

use crate::diagnostics::{Diagnostic, Severity, Span, line_col};
use crate::lexer::{Token, lex};
use crate::source_map::SourceMap;
use morpholog_core::Program;

/// The text of one programme and the files it came from, in programme order.
#[derive(Debug, Clone)]
pub struct SourceText {
    text: String,
    files: Vec<File>,
}

#[derive(Debug, Clone)]
struct File {
    name: String,
    base: usize,
}

/// Where an offset into a [`SourceText`] falls: its file, and the byte offset, 1-based line and
/// column within that file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location<'a> {
    pub file: &'a str,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl SourceText {
    /// A programme in one file.
    pub fn file(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            files: vec![File {
                name: name.into(),
                base: 0,
            }],
        }
    }

    /// A programme in parts, `(name, text)` in programme order; `None` when there are none.
    pub fn parts(parts: impl IntoIterator<Item = (String, String)>) -> Option<Self> {
        let mut text = String::new();
        let mut files = Vec::new();
        for (name, part) in parts {
            files.push(File {
                name,
                base: text.len(),
            });
            text.push_str(&part);
        }
        (!files.is_empty()).then_some(Self { text, files })
    }

    /// The whole text the parser reads, every span an offset into it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The file names, in programme order.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|f| f.name.as_str())
    }

    /// Parse the programme. Parts are first held to their boundaries, then parsed as one text.
    pub fn parse(&self) -> Result<(Program, SourceMap), Vec<Diagnostic>> {
        let refused = self.boundary_refusals();
        if !refused.is_empty() {
            return Err(refused);
        }
        crate::parse_program_with_sources(&self.text)
    }

    /// The file an offset falls in, with its local offset, line and column. Offsets past the end
    /// clamp to it.
    pub fn locate(&self, offset: usize) -> Location<'_> {
        let offset = offset.min(self.text.len());
        let i = self.index_of(offset);
        let local = offset - self.files[i].base;
        let (line, column) = line_col(self.part(i), local);
        Location {
            file: &self.files[i].name,
            offset: local,
            line,
            column,
        }
    }

    /// A span as its file and the range within it. A span is placed by its start; its end is
    /// held to that file.
    pub fn local_span(&self, span: &Span) -> (&str, Span) {
        let start = self.locate(span.start);
        let i = self.index_of(span.start.min(self.text.len()));
        let end = span.end.clamp(span.start, self.end_of(i)) - self.files[i].base;
        (start.file, start.offset..end.max(start.offset))
    }

    /// Render a diagnostic as text with `ariadne` carets, each label in its own file, in colour
    /// only when `colour` is set: ariadne colours each character, so uncoloured text is the only
    /// kind a pipe or a log can read.
    pub fn render(&self, d: &Diagnostic, colour: bool) -> String {
        use ariadne::{Color, Config, Label, Report, ReportKind};
        // Lowercase, to match the `error:` / `hint:` prefix printed for findings with no span.
        let (name, color) = match d.severity {
            Severity::Error => ("error", Color::Red),
            Severity::Hint => ("hint", Color::Yellow),
        };
        let at = |span: &Span| {
            let (file, local) = self.local_span(span);
            (file.to_string(), local)
        };
        // The header already shows the message; repeating it under the carets is noise.
        let mut report = Report::build(ReportKind::Custom(name, color), at(&d.primary))
            .with_config(Config::default().with_color(colour))
            .with_message(&d.message)
            .with_label(
                Label::new(at(&d.primary))
                    .with_message("here")
                    .with_color(color),
            );
        for (span, note) in &d.secondary {
            report = report.with_label(
                Label::new(at(span))
                    .with_message(note)
                    .with_color(Color::Yellow),
            );
        }
        let sources = (0..self.files.len()).map(|i| (self.files[i].name.clone(), self.part(i)));
        let mut buf = Vec::new();
        let written = report.finish().write(ariadne::sources(sources), &mut buf);
        let Some(rendered) = written.ok().and_then(|()| String::from_utf8(buf).ok()) else {
            return format!("{} at bytes {:?}: {}", d.severity, d.primary, d.message);
        };
        if colour {
            return rendered;
        }
        // ariadne colours a custom kind's header whatever the config says, so
        // put back the plain one.
        let header = format!("{name}:");
        rendered.replacen(&ariadne::Fmt::fg(&header, color).to_string(), &header, 1)
    }

    fn part(&self, i: usize) -> &str {
        &self.text[self.files[i].base..self.end_of(i)]
    }

    fn end_of(&self, i: usize) -> usize {
        self.files.get(i + 1).map_or(self.text.len(), |f| f.base)
    }

    /// The last file starting at or before `offset`, so an empty part never claims an offset.
    fn index_of(&self, offset: usize) -> usize {
        self.files
            .partition_point(|f| f.base <= offset)
            .saturating_sub(1)
    }

    /// Where a construct would run from one part into the next. A single file has no boundary,
    /// and the parser alone judges it.
    fn boundary_refusals(&self) -> Vec<Diagnostic> {
        if self.files.len() < 2 {
            return Vec::new();
        }
        // A comment on an unterminated last line would swallow the next part's first line.
        let unterminated: Vec<Diagnostic> = (0..self.files.len() - 1)
            .filter_map(|i| {
                let part = self.part(i);
                let (last, _) = part.char_indices().last()?;
                (!part.ends_with('\n')).then(|| {
                    let base = self.files[i].base;
                    Diagnostic::error(
                        format!(
                            "`{}` does not end with a newline, so its last line would run into \
                             the next part",
                            self.files[i].name
                        ),
                        base + last..self.end_of(i),
                    )
                })
            })
            .collect();
        if !unterminated.is_empty() {
            return unterminated;
        }
        // The parser reports a text that does not lex.
        let Ok(tokens) = lex(&self.text) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut open: Vec<Span> = Vec::new();
        let mut current: Option<usize> = None;
        for (token, span) in &tokens {
            let i = self.index_of(span.start);
            if let Some(prev) = current.filter(|&prev| prev != i) {
                if let Some(paren) = open.first() {
                    out.push(Diagnostic::error(
                        format!(
                            "this parenthesis is still open at the end of `{}`; a part closes \
                             every parenthesis it opens",
                            self.files[prev].name
                        ),
                        paren.clone(),
                    ));
                }
                open.clear();
                let line_start = self.text[..span.start].rfind('\n').map_or(0, |n| n + 1);
                if line_start < span.start {
                    out.push(Diagnostic::error(
                        format!(
                            "`{}` starts indented, so its first lines would continue the block \
                             the part before it ends with; a part starts at column 1",
                            self.files[i].name
                        ),
                        line_start..span.start,
                    ));
                }
            }
            let header = *token == Token::KwProgram;
            if current.is_none() && (i != 0 || !header) {
                out.push(Diagnostic::error(
                    format!(
                        "the first part listed, `{}`, must begin with `program <name>`",
                        self.files[0].name
                    ),
                    span.clone(),
                ));
            } else if current.is_some() && header && i > 0 {
                out.push(Diagnostic::error(
                    format!(
                        "`{}` holds a `program` header; only the first part listed holds one",
                        self.files[i].name
                    ),
                    span.clone(),
                ));
            }
            current = Some(i);
            if *token == Token::LParen {
                open.push(span.clone());
            } else if *token == Token::RParen {
                open.pop();
            }
        }
        out
    }
}
