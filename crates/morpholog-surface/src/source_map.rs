//! Source locations that survive parsing.
//!
//! The kernel IR carries no byte offsets, since a `Program` need not come from source. The
//! [`SourceMap`] remembers where each declaration and each top-level transformation statement
//! came from, so a [`ValidationError`] or [`Lint`] can be shown against the `.morph` text.
//!
//! A statement nested in a `for` gets the `for`'s span; there are no sub-expression spans.
//! Findings the map cannot place, such as a generated discipline invariant, resolve to `None`.

use std::collections::HashMap;

use morpholog_core::{Lint, ValidationContext, ValidationError, VocabularyKind};

use crate::diagnostics::Span;

/// The kind of top-level declaration a span belongs to. Names are unique per kind, not across
/// kinds: an invariant and a transformation can share a name.
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

    /// Resolve a validation error to the source span it concerns, through its context or the
    /// declaration it names. `None` when it has no place in the source, such as a generated
    /// invariant or a name this programme never declared.
    pub fn span_for_error(&self, error: &ValidationError) -> Option<Span> {
        match error {
            ValidationError::Undeclared { context, .. }
            | ValidationError::ArityMismatch { context, .. }
            | ValidationError::ArgKindMismatch { context, .. }
            | ValidationError::OperandKindMismatch { context, .. }
            | ValidationError::UnorderedExtremum { context, .. }
            | ValidationError::WildcardAsValue { context }
            | ValidationError::InvalidValueExtraction { context, .. }
            | ValidationError::DerivedValueNotAKey { context, .. }
            | ValidationError::EmptySumUntyped { context, .. }
            | ValidationError::DerivedInRule { context, .. }
            | ValidationError::NoArithRule { context, .. }
            | ValidationError::CondBranchKindMismatch { context, .. }
            | ValidationError::PeriodSpanNotPositive { context, .. }
            | ValidationError::PeriodIndexNotWhole { context, .. }
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
            // Points at the top-level statement holding the duplicate; inside a `for`, the `for`.
            | ValidationError::DuplicateRuleName { context, .. } => self.context_span(context),
            ValidationError::DuplicateDecl { vocabulary, name } => {
                let kind = match vocabulary {
                    VocabularyKind::Predicate => DeclKind::Predicate,
                    VocabularyKind::Intent => DeclKind::Intent,
                    VocabularyKind::Definition => DeclKind::Definition,
                    VocabularyKind::Derived => DeclKind::DerivedClaim,
                };
                self.decl_span(kind, name)
            }
            // On the predicate, which is where `partial` is written.
            ValidationError::PartialContradictsTotality { predicate, .. } => {
                self.decl_span(DeclKind::Predicate, predicate)
            }
            // On the invariant: the unknown predicate has no declaration to point at.
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
            ValidationError::DuplicateArgName {
                vocabulary, name, ..
            } => {
                let kind = match vocabulary {
                    VocabularyKind::Intent => DeclKind::Intent,
                    _ => DeclKind::Predicate,
                };
                self.decl_span(kind, name)
            }
            // On the predicate, where the author wrote the clause, not the generated invariant.
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
            // Only hand-built IR can declare a CalendarSpan argument; point at the declaration.
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
            // On the predicate: its effective-dating clause is the line the author can act on.
            Lint::EffectiveWithoutDeclaredTotality { predicate } => {
                self.decl_span(DeclKind::Predicate, predicate)
            }
            // On this programme's transformation; the message names the other programme.
            Lint::SharedWriter { transformation, .. } => {
                self.decl_span(DeclKind::Transformation, transformation)
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
