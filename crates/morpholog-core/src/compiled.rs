//! `CompiledProgram`: a validated programme with its by-name lookups
//! indexed once.
//!
//! A [`Program`] is parsed, then validated, then read from - and callers
//! reach into it with linear scans (`Program::transformation(name)` and
//! friends). `CompiledProgram` owns a validated programme and builds
//! those by-name lookups once, so the orchestration layer has a single,
//! indexed model object to source from.
//!
//! It does **not** replace [`ValidatedProgram`]. That stays the cheap,
//! borrowed proof-of-validity handle the analysis API consumes;
//! `CompiledProgram` is the owned home that hands one out via
//! [`CompiledProgram::validated`]. One owns and indexes; the other is a
//! borrowed view with the same validity guarantee.
//!
//! The indices map a name to a **position** in the owned vectors, never
//! a reference into them: an owned struct holding `&Transformation` into
//! its own field would be self-referential. Accessors resolve the
//! position against `self.program` on demand.

use std::collections::HashMap;
use std::hash::Hash;

use crate::definitions::DefinitionTable;
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
}

impl CompiledProgram {
    /// Validate the programme, then index it. Constructing a
    /// `CompiledProgram` *is* the validation gate: the error case is the
    /// same `Vec<ValidationError>` [`Program::validate`] returns.
    ///
    /// Each accessor resolves to the first declaration with that name,
    /// matching the `iter().find()` semantics the `Program::*` lookups
    /// have today. Validation rejects duplicate predicates, intents, and
    /// definitions; for the rest the first-occurrence rule is what the
    /// index guarantees.
    pub fn new(program: Program) -> Result<Self, Vec<ValidationError>> {
        program.validate()?;
        Ok(Self {
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
    /// Routed through the same definition table every walker uses,
    /// rather than a second index of its own. A programme names a
    /// handful of definitions, so the scan is not the cost the map was
    /// paying for - and one authority for "which definition is this"
    /// is worth more than a lookup nothing measures.
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

    /// The definition table over this programme's definitions - the
    /// one handle every walker and the accessor above share. It
    /// borrows the definitions slice, so it is constructed on demand
    /// rather than cached (a cached `DefinitionTable<'_>` would be
    /// self-referential); construction is a pointer copy.
    pub(crate) fn definition_table(&self) -> DefinitionTable<'_> {
        DefinitionTable::new(&self.program.definitions)
    }
}
