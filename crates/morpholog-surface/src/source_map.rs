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
#[derive(Debug, Default)]
pub struct SourceMap {
    decls: HashMap<DeclKind, HashMap<String, Span>>,
    statements: HashMap<String, Vec<Span>>,
}

impl SourceMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert_decl(&mut self, kind: DeclKind, name: &str, span: Span) {
        self.decls
            .entry(kind)
            .or_default()
            .insert(name.to_string(), span);
    }

    pub(crate) fn insert_statements(&mut self, transformation: &str, spans: Vec<Span>) {
        self.statements.insert(transformation.to_string(), spans);
    }

    /// The span of a declaration, if this programme declared it in
    /// source. Generated names (discipline invariants) are absent.
    pub fn decl_span(&self, kind: DeclKind, name: &str) -> Option<Span> {
        self.decls.get(&kind)?.get(name).cloned()
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
            | ValidationError::UnorderedExtremum { context, .. }
            | ValidationError::DerivedInRule { context, .. }
            | ValidationError::NoArithRule { context, .. }
            | ValidationError::CondBranchKindMismatch { context, .. }
            | ValidationError::PeriodSpanNotPositive { context, .. }
            | ValidationError::BuiltinArity { context, .. }
            | ValidationError::BuiltinKind { context, .. }
            | ValidationError::AbsKind { context, .. }
            | ValidationError::RoundQuantumNotPositive { context, .. }
            | ValidationError::EqualityKindMismatch { context, .. }
            | ValidationError::VariableKindConflict { context, .. }
            | ValidationError::ActorNotAvailable { context }
            | ValidationError::NestingTooDeep { context }
            | ValidationError::UnboundVariable { context, .. }
            | ValidationError::UnresolvedDefinitionCall { context, .. }
            | ValidationError::PreNotAvailable { context }
            | ValidationError::CalendarSpanEscapesExpression { context, .. }
            | ValidationError::RetractsAppendOnly { context, .. }
            // Anchored on the top-level statement the later duplicate sits
            // in, which for one inside a `for` is the `for` itself - the
            // statement index never reaches into a nested body.
            | ValidationError::DuplicateRuleName { context, .. } => self.context_span(context),
            ValidationError::DuplicateDecl { vocabulary, name } => {
                let kind = match vocabulary {
                    VocabularyKind::Predicate => DeclKind::Predicate,
                    VocabularyKind::Intent => DeclKind::Intent,
                    VocabularyKind::Definition => DeclKind::Definition,
                };
                self.decl_span(kind, name)
            }
            // The clause is on the invariant's own declaration line, which
            // is what the author can act on - the unknown predicate has no
            // declaration to point at, that being the complaint.
            // On the predicate, which is where `partial` is written.
            ValidationError::PartialContradictsTotality { predicate, .. } => {
                self.decl_span(DeclKind::Predicate, predicate)
            }
            ValidationError::UnknownTotalityTarget { invariant, .. } => {
                self.decl_span(DeclKind::Invariant, invariant)
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
            // Anchored on the declaration, with its discipline siblings:
            // the clause the author wrote is there, not in the invariant
            // the lowering would have generated.
            ValidationError::MultipleEffectiveClauses { predicate }
            | ValidationError::EffectiveDateIsAKey { predicate, .. }
            | ValidationError::EffectiveDateNotATime { predicate, .. }
            | ValidationError::DisciplineOnDerived { predicate }
            | ValidationError::DisciplineUnknownField { predicate, .. }
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
            // Unreachable from parsed source: the surface has no
            // spelling for declaring a CalendarSpan argument. Anchored
            // on the declaration defensively for hand-built IR routed
            // through a source map.
            ValidationError::CalendarSpanNotDeclarable { declaration, .. } => self
                .decl_span(DeclKind::Predicate, declaration)
                .or_else(|| self.decl_span(DeclKind::Intent, declaration)),
        }
    }

    /// Resolve a lint to the source span it concerns.
    pub fn span_for_lint(&self, lint: &Lint) -> Option<Span> {
        match lint {
            Lint::GateVsInvariant { invariant, .. }
            | Lint::UnsuppliedAntecedent { invariant, .. }
            | Lint::GoverningSelectionWithoutTotality { invariant, .. } => {
                self.decl_span(DeclKind::Invariant, invariant)
            }
            // Anchored on the PREDICATE: the omission is the missing
            // companion, and the declaration that opted into
            // effective-dating is the line the author can act on.
            Lint::EffectiveWithoutDeclaredTotality { predicate } => {
                self.decl_span(DeclKind::Predicate, predicate)
            }
        }
    }

    fn context_span(&self, context: &ValidationContext) -> Option<Span> {
        match context {
            ValidationContext::Invariant { name } => self.decl_span(DeclKind::Invariant, name),
            ValidationContext::Transformation { name, statement } => statement
                .and_then(|index| self.statement_span(name, index))
                .or_else(|| self.decl_span(DeclKind::Transformation, name)),
            ValidationContext::DerivedClaim { predicate } => {
                self.decl_span(DeclKind::DerivedClaim, predicate)
            }
            ValidationContext::Definition { name } => self.decl_span(DeclKind::Definition, name),
        }
    }
}
