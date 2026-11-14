//! Morpholog surface: `.morph` source to kernel IR.
//!
//! Named more broadly than "parser" so later source-aware tooling can live here too.
//!
//! Entry points: [`parse_program`] returns the [`morpholog_core::Program`];
//! [`parse_program_with_sources`] also returns the [`SourceMap`] that places declarations,
//! statements, and kernel findings back in the source, keyed by [`DeclKind`];
//! [`parse_expression`] parses a standalone proposition and [`parse_value_expr`] a standalone
//! value expression. Failures come back as [`Diagnostic`]s; [`line_col`] turns a byte offset into
//! a line and column.
//!
//! A programme's text is a [`SourceText`]: one file, or the parts a `.parts` manifest lists, read
//! as one text. It parses the whole, places a span back in its file, and renders a diagnostic
//! with `ariadne` carets in the files it points into.
//!
//! [`lexer`] ([`lexer::Token`], [`lexer::lex`]) and [`layout`] are public so tests and tools can
//! drive each pass directly.

mod diagnostics;
pub mod layout;
pub mod lexer;
mod parser;
mod source_map;
mod source_text;

pub use diagnostics::{Diagnostic, Severity, Span, line_col};
pub use parser::{parse_expression, parse_program, parse_program_with_sources, parse_value_expr};
pub use source_map::{DeclKind, SourceMap};
pub use source_text::{Location, SourceText};
