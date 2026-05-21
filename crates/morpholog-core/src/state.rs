//! Runtime state types and the in-memory state store.
//!
//! `EvalValue` is the runtime form of `Value` (the IR literal type);
//! `ClaimInstance` and `IntentInstance` are the grounded resolved forms
//! of `Claim` and `Intent`. `State` holds the set of admitted claims
//! plus the indexes that let the evaluator narrow lookups by predicate
//! name and by argument position. `Bindings` is the per-statement
//! variable-binding context threaded through evaluation.

use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A runtime value flowing through evaluation. Distinct from the IR's
/// `Value` (which holds literals only).
///
/// JSON encoding uses an adjacently-tagged shape
/// (`{ "type": "...", "value": ... }`), suitable for the PG JSONB columns
/// defined in `crates/morpholog-core/sql/schema.sql`. Decimals serialise
/// as JSON **strings** to preserve exactness; never as JSON numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum EvalValue {
    Decimal(#[serde(with = "rust_decimal::serde::str")] Decimal),
    Subject(String),
    Bool(bool),
    Collection(Vec<EvalValue>),
    /// Civil date (ISO-8601 `YYYY-MM-DD`) with no time-of-day and no
    /// time zone. JSON shape: `{ "type": "date", "value": "YYYY-MM-DD" }`
    /// (jiff's default serde format for [`jiff::civil::Date`]).
    Date(Date),
}

/// A grounded claim: all args are values, no variables or wildcards.
///
/// JSON encoding shape: `{ "predicate": "...", "args": [ ... ] }`.
///
/// Used as-is for elements of `audit.asserted_claims` and
/// `audit.retracted_claims` (each column is a JSONB array of these objects).
///
/// For row writes to the `claims` table itself, the PG adapter **splits**
/// the claim across two columns: `predicate_name` (text, from `predicate`)
/// and `arguments` (JSONB array, from `args`). The `arguments` column has
/// a CHECK constraint that requires `jsonb_typeof(arguments) = 'array'`,
/// so writing the full object there would fail. The `claim_args_serialise_as_a_json_array`
/// test pins this contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClaimInstance {
    pub predicate: String,
    pub args: Vec<EvalValue>,
}

/// The admitted state of the runtime: a set of grounded [`ClaimInstance`]s
/// against which invariants are evaluated and transformations are
/// proposed. State is set-valued: identity is `(predicate, args)`. The
/// PG adapter persists this set as rows in `morpholog.claims`; this
/// in-memory representation is what the kernel evaluates against.
///
/// Internally indexed by predicate name AND by `(predicate, arg
/// position, arg value)` to support ground-argument lookup. Construct
/// via [`State::from_claims`] or [`State::default`]; mutation is not
/// part of the API (the indexes would otherwise go stale). The
/// public accessors are [`State::claims`] (all admitted claims, in
/// construction order) and [`State::claims_for`] (`O(1)` lookup of
/// all claims for a given predicate). Argument-position lookup is
/// internal to the kernel and used by `find_claim_matches` to narrow
/// the candidate set when any argument is already ground.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct State {
    claims: Vec<ClaimInstance>,
    by_predicate: HashMap<String, PredicateIndex>,
}

/// Per-predicate index entry stored on [`State`]. Holds the
/// construction-order positions of every claim with this predicate,
/// plus a secondary index keyed on `(arg position, arg value)` for
/// ground-argument lookup.
///
/// `by_arg` grows lazily as predicates of varying arity are observed:
/// position `p` gets a map only when some claim of this predicate has
/// at least `p + 1` args.
#[derive(Clone, Default, PartialEq, Eq)]
struct PredicateIndex {
    /// Indices into `State.claims` for every claim with this predicate.
    all: Vec<usize>,
    /// `by_arg[position][value]` -> indices into `State.claims` for
    /// claims with this predicate where `args[position] == value`.
    by_arg: Vec<HashMap<EvalValue, Vec<usize>>>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("claims", &self.claims)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Build a `State` from a vector of admitted claims. Builds two
    /// indexes during construction: a per-predicate bucket of claim
    /// positions, and a per-`(predicate, arg position, arg value)`
    /// bucket of claim positions for ground-argument lookup. Both
    /// indexes are immutable thereafter; the State itself is
    /// immutable.
    pub fn from_claims(claims: Vec<ClaimInstance>) -> Self {
        let mut by_predicate: HashMap<String, PredicateIndex> = HashMap::new();
        for (i, c) in claims.iter().enumerate() {
            let entry = by_predicate.entry(c.predicate.clone()).or_default();
            entry.all.push(i);
            if entry.by_arg.len() < c.args.len() {
                entry.by_arg.resize_with(c.args.len(), HashMap::new);
            }
            for (pos, value) in c.args.iter().enumerate() {
                entry.by_arg[pos].entry(value.clone()).or_default().push(i);
            }
        }
        Self {
            claims,
            by_predicate,
        }
    }

    /// All admitted claims, in the order supplied to
    /// [`State::from_claims`]. Read-only.
    pub fn claims(&self) -> &[ClaimInstance] {
        &self.claims
    }

    /// Iterator over every admitted claim whose predicate name matches
    /// `predicate`. `O(1)` to find the bucket; iteration is linear in
    /// the bucket's size. Returns an empty iterator when no claims of
    /// that predicate are admitted.
    pub fn claims_for<'a>(
        &'a self,
        predicate: &str,
    ) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        self.by_predicate
            .get(predicate)
            .map(|idx| idx.all.iter().map(|&i| &self.claims[i]))
            .into_iter()
            .flatten()
    }

    /// Indices into `claims()` for every claim where `predicate`
    /// matches AND `args[position] == value`. `O(1)` lookup. Returns
    /// `None` when no claim of this predicate has this value at this
    /// position, which the caller uses to short-circuit an empty
    /// intersection. Internal to the kernel; used by
    /// `find_claim_matches` to narrow the candidate set when at least
    /// one argument is already ground (a literal in the IR, or a
    /// variable already bound in the surrounding context).
    pub(crate) fn claim_indices_for_arg(
        &self,
        predicate: &str,
        position: usize,
        value: &EvalValue,
    ) -> Option<&[usize]> {
        self.by_predicate
            .get(predicate)
            .and_then(|idx| idx.by_arg.get(position))
            .and_then(|m| m.get(value))
            .map(|v| v.as_slice())
    }

    /// Look up a claim by its `claims()` index. Used internally
    /// alongside [`State::claim_indices_for_arg`] when iterating an
    /// argument-position bucket.
    pub(crate) fn claim_at(&self, index: usize) -> &ClaimInstance {
        &self.claims[index]
    }

    /// Total number of admitted claims across all predicates.
    pub fn len(&self) -> usize {
        self.claims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }
}

/// Variable bindings used during expression evaluation and
/// transformation execution. Maps variable name to resolved
/// [`EvalValue`].
pub type Bindings = HashMap<String, EvalValue>;
/// A resolved intent: all args are values, ready to be enqueued in an outbox.
///
/// JSON encoding shape: `{ "name": "...", "args": [ ... ] }`.
///
/// Used as-is for elements of `audit.emitted_intents` (a JSONB array of these
/// objects).
///
/// For row writes to the `outbox` table, the PG adapter **splits** the intent
/// across two columns: `intent_type` (text, from `name`) and `arguments`
/// (JSONB array, from `args`). The `arguments` column has a CHECK constraint
/// that requires `jsonb_typeof(arguments) = 'array'`, so writing the full
/// object there would fail. The `intent_args_serialise_as_a_json_array`
/// test pins this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentInstance {
    pub name: String,
    pub args: Vec<EvalValue>,
}
