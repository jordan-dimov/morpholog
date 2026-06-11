//! Source locations that survive parsing.
//!
//! The kernel IR is deliberately source-agnostic: a `Program` can be
//! hand-built, deserialised, or parsed, and nothing downstream of the
//! parser carries byte offsets. The [`SourceMap`] is the surface-side
//! companion that remembers where each declaration (and each top-level
//! transformation-body statement) came from, so a finding produced
//! over the IR - a [`ValidationError`], a [`Lint`] - can be rendered
//! with a caret against the original `.morph` text.
//!
//! Granularity is declaration + top-level statement. Sub-expression
//! spans are a later tier; a statement nested inside a `for` inherits
//! the `for`'s span. Findings the map cannot place (a generated
//! discipline invariant, a hand-built name with no source) resolve to
//! `None` and render as plain text.

use std::collections::HashMap;

use morpholog_core::{Lint, ValidationContext, ValidationError, VocabularyKind};

use crate::diagnostics::Span;

/// Which declaration table a span lives in. Mirrors the top-level
/// declaration kinds of a programme; names are unique per kind, not
/// across kinds (a definition may not collide with a predicate, but
/// an invariant and a transformation can share a name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclKind {
    Predicate,
    Intent,
    Definition,
    Invariant,
    Transformation,
    DerivedClaim,
}

/// Byte-offset spans for one parsed programme, keyed the way findings
/// refer back to source: by declaration kind and name, plus the
/// position of each top-level statement within a transformation body.
#[derive(Debug, Clone)]
pub struct SourceMap {
    decls: HashMap<(DeclKind, String), Span>,
    statements: HashMap<String, Vec<Span>>,
    line_starts: Vec<usize>,
}

impl SourceMap {
    pub(crate) fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self {
            decls: HashMap::new(),
            statements: HashMap::new(),
            line_starts,
        }
    }

    pub(crate) fn insert_decl(&mut self, kind: DeclKind, name: &str, span: Span) {
        self.decls.insert((kind, name.to_string()), span);
    }

    pub(crate) fn insert_statements(&mut self, transformation: &str, spans: Vec<Span>) {
        self.statements.insert(transformation.to_string(), spans);
    }

    /// The span of a declaration, if this programme declared it in
    /// source. Generated names (discipline invariants) are absent.
    pub fn decl_span(&self, kind: DeclKind, name: &str) -> Option<Span> {
        self.decls.get(&(kind, name.to_string())).cloned()
    }

    /// The span of the `index`-th (0-based) top-level statement in a
    /// transformation's body. A statement nested in a `for` is covered
    /// by the `for`'s own span.
    pub fn statement_span(&self, transformation: &str, index: usize) -> Option<Span> {
        self.statements.get(transformation)?.get(index).cloned()
    }

    /// Resolve a validation error to the source span it concerns.
    /// Context-carrying errors resolve through their context;
    /// declaration-naming errors resolve by name. `None` when the
    /// finding has no source anchor (a generated invariant, a name
    /// this programme never declared).
    pub fn span_for_error(&self, error: &ValidationError) -> Option<Span> {
        match error {
            ValidationError::Undeclared { context, .. }
            | ValidationError::ArityMismatch { context, .. }
            | ValidationError::ArgKindMismatch { context, .. }
            | ValidationError::OperandKindMismatch { context, .. }
            | ValidationError::NoArithRule { context, .. }
            | ValidationError::EqualityKindMismatch { context, .. }
            | ValidationError::VariableKindConflict { context, .. }
            | ValidationError::ActorNotAvailable { context }
            | ValidationError::NestingTooDeep { context }
            | ValidationError::UnboundVariable { context, .. }
            | ValidationError::UnresolvedDefinitionCall { context, .. }
            | ValidationError::PreNotAvailable { context }
            | ValidationError::RetractsAppendOnly { context, .. } => self.context_span(context),
            ValidationError::DuplicateDecl { vocabulary, name } => {
                let kind = match vocabulary {
                    VocabularyKind::Predicate => DeclKind::Predicate,
                    VocabularyKind::Intent => DeclKind::Intent,
                    VocabularyKind::Definition => DeclKind::Definition,
                };
                self.decl_span(kind, name)
            }
            ValidationError::DefinitionNameCollision { name } => {
                self.decl_span(DeclKind::Definition, name)
            }
            ValidationError::DefinitionCycle { names } => names
                .iter()
                .find_map(|n| self.decl_span(DeclKind::Definition, n)),
            ValidationError::ParameterNotReferenced { definition, .. }
            | ValidationError::DuplicateParameter { definition, .. } => {
                self.decl_span(DeclKind::Definition, definition)
            }
            ValidationError::DisciplineUnknownField { predicate, .. }
            | ValidationError::DisciplineVacuousKeys { predicate }
            | ValidationError::DisciplineDuplicateClause { predicate }
            | ValidationError::DisciplinePointerCannotBeAppendOnly { predicate }
            | ValidationError::DisciplineSupersededWithoutPointer { predicate }
            | ValidationError::DisciplineNotLowered { predicate, .. } => {
                self.decl_span(DeclKind::Predicate, predicate)
            }
            ValidationError::DisciplineLineageUnfit { pointer, .. } => {
                self.decl_span(DeclKind::Predicate, pointer)
            }
        }
    }

    /// Resolve a lint to the source span it concerns.
    pub fn span_for_lint(&self, lint: &Lint) -> Option<Span> {
        match lint {
            Lint::GateVsInvariant { invariant, .. } => {
                self.decl_span(DeclKind::Invariant, invariant)
            }
        }
    }

    /// 1-based line and column for a byte offset, computed against the
    /// source this map was built from. Columns count bytes, which
    /// matches what editors and `ariadne` show for ASCII-dominated
    /// `.morph` text.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        (line + 1, offset - self.line_starts[line] + 1)
    }

    fn context_span(&self, context: &ValidationContext) -> Option<Span> {
        match context {
            ValidationContext::Invariant { name } => self.decl_span(DeclKind::Invariant, name),
            ValidationContext::Transformation { name } => {
                self.decl_span(DeclKind::Transformation, name)
            }
            ValidationContext::DerivedClaim { predicate } => {
                self.decl_span(DeclKind::DerivedClaim, predicate)
            }
            ValidationContext::Definition { name } => self.decl_span(DeclKind::Definition, name),
        }
    }
}
