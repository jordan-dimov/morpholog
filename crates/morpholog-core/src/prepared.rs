//! `PreparedProgram`: a validated programme with the preparation worth
//! keeping across proposals - by-name lookups and each invariant's impact
//! plan. Light borrowed views, such as the definition table, are built on
//! demand. A cache, not an identity: the [`Program`] it owns is what the
//! canonical hash covers, and everything here can be recomputed from it
//! without changing any decision.
//!
//! [`Program`] lookups are linear scans; these are indexed once.
//!
//! It does not replace [`ValidatedProgram`], the cheap borrowed
//! proof-of-validity handle the analysis API takes. `PreparedProgram`
//! owns the programme and hands one out via [`PreparedProgram::validated`].
//!
//! The indices store positions, not references, because a struct holding
//! references into its own fields would be self-referential.

use std::collections::HashMap;
use std::hash::Hash;

use crate::admission::Admission;
use crate::definitions::DefinitionTable;
use crate::eval::EvalError;
use crate::explain::Explanation;
use crate::impact::ImpactPlan;
use crate::ir::{
    Definition, DefinitionName, DerivedClaim, IntentDecl, IntentName, Invariant, InvariantName,
    PredicateDecl, PredicateName, Program, Transformation, TransformationName,
};
use crate::propose::{Outcome, StagedDelta, SubjectSource, TracedProposal, Transition};
use crate::state::State;
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

/// A validated programme with its lookups and impact plans built once.
/// See the module documentation for the relationship to
/// [`ValidatedProgram`].
#[derive(Debug, Clone)]
pub struct PreparedProgram {
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
    /// The canonical hash of `program`, computed once.
    model_hash: String,
}

impl PreparedProgram {
    /// Validate the programme, then index it. The error is the same
    /// `Vec<ValidationError>` [`Program::validate`] returns.
    ///
    /// Each accessor returns the first declaration with that name, like
    /// the `Program::*` lookups.
    pub fn new(program: Program) -> Result<Self, Vec<ValidationError>> {
        program.validate()?;
        let impact = program
            .invariants
            .iter()
            .map(|inv| ImpactPlan::with_definitions(inv, &program.definitions))
            .collect();
        Ok(Self {
            impact,
            model_hash: crate::format::canonical_hash(&program),
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

    /// The programme's semantic identity, [`crate::format::canonical_hash`]
    /// of the whole programme, computed once. Every commit records it.
    pub fn model_hash(&self) -> &str {
        &self.model_hash
    }

    /// The rules a transition is admitted under, with the plans built
    /// at construction.
    /// One impact plan per invariant, in programme order: the plans
    /// admission applies, for a report that must not drift from them.
    pub fn impact_plans(&self) -> &[ImpactPlan] {
        &self.impact
    }

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

    /// Propose `transition`: run the transformation it names against
    /// `pre_state`, then check the invariants over the cases its change
    /// could affect. `Ok(None)` when no transformation has that name.
    pub fn propose(
        &self,
        transition: &Transition,
        pre_state: &State,
        subjects: &mut dyn SubjectSource,
    ) -> Result<Option<Outcome>, EvalError> {
        let Some(transformation) = self.transformation(&transition.transformation_name) else {
            return Ok(None);
        };
        crate::propose::propose_with(
            transformation,
            transition,
            pre_state,
            &self.admission(),
            subjects,
        )
        .map(Some)
    }

    /// [`Self::propose`], recording the execution trace. `None` when no
    /// transformation has that name.
    pub fn propose_with_trace(
        &self,
        transition: &Transition,
        pre_state: &State,
        subjects: &mut dyn SubjectSource,
    ) -> Option<TracedProposal> {
        let transformation = self.transformation(&transition.transformation_name)?;
        Some(crate::propose::propose_with_trace(
            transformation,
            transition,
            pre_state,
            &self.program.invariants,
            &self.program.definitions,
            subjects,
        ))
    }

    /// Run only the transformation body, stopping before the invariants;
    /// [`crate::finish_staged_delta_with`] with [`Self::admission`]
    /// completes it. `Ok(None)` when no transformation has that name.
    pub fn stage_delta(
        &self,
        transition: &Transition,
        pre_state: &State,
        subjects: &mut dyn SubjectSource,
    ) -> Result<Option<StagedDelta>, EvalError> {
        let Some(transformation) = self.transformation(&transition.transformation_name) else {
            return Ok(None);
        };
        crate::propose::propose_stage_delta(
            transformation,
            transition,
            pre_state,
            &self.program.definitions,
            subjects,
        )
        .map(Some)
    }

    /// Why `transition` would or would not be admitted against
    /// `pre_state`. An unknown transformation is a rejection in the
    /// explanation itself.
    pub fn explain(
        &self,
        transition: &Transition,
        pre_state: &State,
        subjects: &mut dyn SubjectSource,
    ) -> Explanation {
        crate::explain::explain(&self.program, transition, pre_state, subjects)
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
