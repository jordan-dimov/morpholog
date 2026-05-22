//! Morpholog surface: source-to-IR pipeline.
//!
//! v0 fragment: a `.morph` file may contain a `program` header and
//! zero or more `predicate` declarations. Invariants, transformations,
//! and derived claims are not yet recognised; that surface lands in
//! subsequent PRs.
//!
//! The crate name is deliberately broader than "parser". The surface
//! concept covers the parser today; in time it will also cover any
//! formatter that emits canonical `.morph` text, source-mapping
//! helpers, and (eventually) LSP-shaped tooling. Keeping these under
//! one crate name avoids a crate split each time a new piece of
//! source-aware tooling lands.
//!
//! Entry point: [`parse_program`]. Returns either the parsed
//! [`morpholog_core::Program`] or a list of diagnostics rich enough
//! to render via `ariadne`.

mod diagnostics;
mod lexer;
mod parser;

pub use diagnostics::{Diagnostic, Severity, Span};
pub use parser::parse_program;
