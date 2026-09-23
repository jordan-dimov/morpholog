//! `CompiledProgram`: a validated programme with its by-name lookups
//! indexed once.
//!
//! [`Program`] lookups are linear scans. `CompiledProgram` owns a
//! validated programme and indexes those lookups once.
//!
//! It does not replace [`ValidatedProgram`], the cheap borrowed
//! proof-of-validity handle the analysis API takes. `CompiledProgram`
//! owns the programme and hands one out via [`CompiledProgram::validated`].
//!
//! The indices store positions, not references, because a struct holding
//! references into its own fields would be self-referential.

use std::collections::HashMap;
use std::hash::Hash;

use crate::admission::Admission;
use crate::definitions::DefinitionTable;
use crate::impact::ImpactPlan;
use crate::ir::{
    Definition, DefinitionName, DerivedClaim, IntentDecl, IntentName, Invariant, InvariantName,
    PredicateDecl, PredicateName, Program, Transformation, TransformationName,
};
use crate::validate::{ValidatedProgram, ValidationError};

/// Index each item to the position of the *first* occurrence of its key,
/// so a lookup matches the `iter().find()` semantics the `Program::*`
/// lookups have, whether or not validation rejects the duplicate.
fn position_index<T, K: Eq + Hash>(items: &[T], key: impl Fn(&T) -> K) -> HashMap<K, usize> {
    let mut map = HashMap::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        map.entry(key(item)).or_insert(i);
    }
    map
}

/// A validated programme with its by-name lookups indexed once. See the
/// module documentation for the relationship to [`ValidatedProgram`].
#[derive(Debug, Clone)]
pub struct CompiledProgram {
    program: Program,
    transformations: HashMap<TransformationName, usize>,
    invariants: HashMap<InvariantName, usize>,
    predicates: HashMap<PredicateName, usize>,
    intents: HashMap<IntentName, usize>,
    /// Derived claims are keyed by their output predicate, matching
    /// [`Program::derived_claim`].
    derived_claims: HashMap<PredicateName, usize>,
    /// One impact plan per invariant, in order, built once here so the
    /// commit path plans nothing per proposal.
    impact: Vec<ImpactPlan>,
}

impl CompiledProgram {
    /// Validate the programme, then index it. The error is the same
    /// `Vec<ValidationError>` [`Program::validate`] returns.
    ///
    /// Each accessor returns the first declaration with that name, like
    /// the `Program::*` lookups.
    pub fn new(program: Program) -> Result<Self, Vec<ValidationError>> {
        program.validate()?;
        let impact = program.invariants.iter().map(ImpactPlan::new).collect();
        Ok(Self {
            impact,
            transformations: position_index(&program.transformations, |t| t.name.clone()),
            invariants: position_index(&program.invariants, |i| i.name.clone()),
            predicates: position_index(&program.predicates, |p| p.name.clone()),
            intents: position_index(&program.intents, |i| i.name.clone()),
            derived_claims: position_index(&program.derived_claims, |d| d.predicate.clone()),
            program,
        })
    }

    /// Borrow the underlying validated programme.
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// The rules a transition is admitted under, with the plans built
    /// at construction.
    pub fn admission(&self) -> Admission<'_> {
        Admission::with_plans(
            &self.program.invariants,
            &self.program.definitions,
            &self.impact,
        )
    }

    /// A borrowed proof-of-validity view, for the analysis API that
    /// takes [`ValidatedProgram`]. Sound because `self.program` was
    /// validated at construction.
    pub fn validated(&self) -> ValidatedProgram<'_> {
        ValidatedProgram::from_validated(&self.program)
    }

    /// The transformation with this name, or `None`. O(1).
    pub fn transformation(&self, name: &TransformationName) -> Option<&Transformation> {
        self.transformations
            .get(name)
            .map(|&i| &self.program.transformations[i])
    }

    /// The invariant with this name, or `None`. O(1).
    pub fn invariant(&self, name: &InvariantName) -> Option<&Invariant> {
        self.invariants
            .get(name)
            .map(|&i| &self.program.invariants[i])
    }

    /// The definition with this name, or `None`.
    ///
    /// Uses the same definition table as every walker, so there is one
    /// answer to "which definition is this". Programmes have few
    /// definitions, so the scan is cheap.
    pub fn definition(&self, name: &DefinitionName) -> Option<&Definition> {
        self.definition_table().get(name)
    }

    /// The predicate declaration with this name, or `None`. O(1).
    pub fn predicate(&self, name: &PredicateName) -> Option<&PredicateDecl> {
        self.predicates
            .get(name)
            .map(|&i| &self.program.predicates[i])
    }

    /// The intent declaration with this name, or `None`. O(1).
    pub fn intent(&self, name: &IntentName) -> Option<&IntentDecl> {
        self.intents.get(name).map(|&i| &self.program.intents[i])
    }

    /// The derived claim with this output predicate, or `None`. O(1).
    pub fn derived_claim(&self, predicate: &PredicateName) -> Option<&DerivedClaim> {
        self.derived_claims
            .get(predicate)
            .map(|&i| &self.program.derived_claims[i])
    }

    /// The definition table over this programme's definitions. Built on
    /// demand (it is a pointer copy) because caching it would be
    /// self-referential.
    pub(crate) fn definition_table(&self) -> DefinitionTable<'_> {
        DefinitionTable::new(&self.program.definitions)
    }
}
