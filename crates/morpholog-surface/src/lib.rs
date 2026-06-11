//! Morpholog surface: source-to-IR pipeline.
//!
//! A `.morph` file today carries the `program` header, `predicate`
//! declarations, `invariant` declarations, and `transformation`
//! declarations with `require` / `bind` / `let` (including
//! `let new Subject()`) body statements. State-mutating statements
//! (`admit`, `retract`, `emit`, `for x in coll:`) are reserved at
//! the lexer but parser-rejected for now; derived claims are not
//! yet recognised. The full v0 surface arrives over the remaining
//! parser PRs - see `docs/roadmap.md`.
//!
//! The crate name is deliberately broader than "parser". The
//! surface concept covers the parser today; in time it will also
//! cover any formatter that emits canonical `.morph` text,
//! source-mapping helpers, and (eventually) LSP-shaped tooling.
//! Keeping these under one crate name avoids a crate split each
//! time a new piece of source-aware tooling lands.
//!
//! Entry points: [`parse_program`] returns the full
//! [`morpholog_core::Program`]; [`parse_expression`] returns a
//! standalone proposition, [`parse_value_expr`] a standalone value
//! expression. All return diagnostics rich enough to render via
//! `ariadne`.
//!
//! Module layout:
//!
//! - `diagnostics` (private): the [`Diagnostic`] type, exported
//!   via `pub use`.
//! - [`lexer`]: tokens + character-level lexing. Public so
//!   integration tests can drive lexing directly; the public
//!   surface here is [`lexer::Token`] and [`lexer::lex`].
//! - [`layout`]: the indentation-aware normalisation pass that
//!   sits between lexer and parser. Public for the same reason
//!   as `lexer` and because future tooling (formatter, source-
//!   mapper) will want to drive it.
//! - `parser` (private): the structural parser. Its public
//!   entry points are re-exported below.

mod diagnostics;
pub mod layout;
pub mod lexer;
mod parser;
mod source_map;

pub use diagnostics::{Diagnostic, Severity, Span, line_col};
pub use parser::{parse_expression, parse_program, parse_program_with_sources, parse_value_expr};
pub use source_map::{DeclKind, SourceMap};
