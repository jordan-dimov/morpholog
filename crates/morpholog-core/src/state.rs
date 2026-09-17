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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ir::{IntentName, PredicateName, Subject, Unit, Var};

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
    Subject(Subject),
    Bool(bool),
    Collection(Vec<EvalValue>),
    /// Civil date (ISO-8601 `YYYY-MM-DD`) with no time-of-day and no
    /// time zone. JSON shape: `{ "type": "date", "value": "YYYY-MM-DD" }`
    /// (jiff's default serde format for [`jiff::civil::Date`]).
    Date(Date),
    /// An exact instant on the UTC timeline. JSON shape:
    /// `{ "type": "timestamp", "value": "2026-10-24T14:00:00Z" }`
    /// (jiff's default serde format for [`jiff::Timestamp`]).
    /// Zone-less by design: local-time interpretation is domain
    /// modelling, admitted as claims, never a runtime assumption.
    Timestamp(jiff::Timestamp),
    /// An exact span of time, in exact seconds (no calendar units).
    /// JSON shape: `{ "type": "duration", "value": "PT6H" }` (jiff's
    /// default serde format for [`jiff::SignedDuration`]).
    Duration(jiff::SignedDuration),
    /// A calendar span (whole months plus whole days) - an arithmetic
    /// operand only, never admitted state: the proposal path refuses a
    /// calendar span in any claim, intent, or transition argument, so
    /// this variant lawfully never reaches storage or the wire.
    CalendarSpan(crate::calendar::CalendarSpan),
    /// A unit-tagged exact decimal quantity. The amount serialises as a
    /// JSON **string** (exactness, like [`EvalValue::Decimal`]); the
    /// unit is an opaque case-sensitive symbol. JSON shape:
    /// `{ "type": "quantity", "value": { "amount": "25000", "unit": "USD" } }`.
    Quantity {
        #[serde(with = "rust_decimal::serde::str")]
        amount: Decimal,
        unit: Unit,
    },
}

impl EvalValue {
    /// Does this value carry a calendar span, directly or inside a
    /// collection? The storage and wire boundaries (claim and intent
    /// construction, transition arguments, derived rows) all refuse
    /// on this - a span shifts a date inside an expression and is
    /// never itself a governed value.
    pub(crate) fn contains_calendar_span(&self) -> bool {
        match self {
            EvalValue::CalendarSpan(_) => true,
            EvalValue::Collection(items) => items.iter().any(Self::contains_calendar_span),
            _ => false,
        }
    }
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
    pub predicate: PredicateName,
    pub args: Vec<EvalValue>,
}

/// The admitted state of the runtime: a set of grounded [`ClaimInstance`]s
/// against which invariants are evaluated and transformations are
/// proposed. State is set-valued: identity is `(predicate, args)`. The
/// PG adapter persists this set as rows in `morpholog.claims`; this
/// in-memory representation is what the kernel evaluates against.
///
/// Two layers: a base shared between a state and every state derived
/// from it, and an overlay of what changed since. A candidate built by
/// `with_delta` shares its pre-state's base and copies only the
/// overlay, so building it costs the uncompacted overlay plus the act's
/// own delta, never the base. When the overlay has grown past a
/// fraction of the base it is folded into a fresh base, once.
///
/// The logical claim sequence is the base in construction order minus
/// retractions, then the additions in order. Retracting and later
/// re-admitting a claim appends a new entry at the tail; it never
/// revives the old position. Two states are equal when their logical
/// sequences are equal, whatever their layering.
///
/// Indexed by predicate name and by `(predicate, arg position, arg
/// value)` so the evaluator can narrow a claim pattern to the smallest
/// bucket a ground argument names. Construct via [`State::from_claims`]
/// or [`State::default`]; derive a successor via `with_delta`.
#[derive(Clone, Default)]
pub struct State {
    base: Arc<Layer>,
    /// Claims admitted since the base, in admission order.
    overlay: Layer,
    /// Parallel to `overlay.claims`: an entry retracted after it was
    /// admitted stays in place, dead, until compaction.
    dead_overlay: Vec<bool>,
    /// Positions in the base retracted since it was built.
    dead_base: HashSet<usize>,
    /// The logical claim count.
    live: usize,
}

/// One indexed run of claims: the base, or the overlay.
#[derive(Clone, Default)]
struct Layer {
    claims: Vec<ClaimInstance>,
    by_predicate: HashMap<PredicateName, PredicateIndex>,
}

/// Per-predicate index entry stored on a [`Layer`]. Holds the
/// construction-order positions of every claim with this predicate,
/// plus a secondary index keyed on `(arg position, arg value)` for
/// ground-argument lookup.
///
/// `by_arg` grows lazily as predicates of varying arity are observed:
/// position `p` gets a map only when some claim of this predicate has
/// at least `p + 1` args.
#[derive(Clone, Default)]
struct PredicateIndex {
    /// Indices into `Layer.claims` for every claim with this predicate.
    all: Vec<usize>,
    /// `by_arg[position][value]` -> indices into `Layer.claims` for
    /// claims with this predicate where `args[position] == value`.
    by_arg: Vec<HashMap<EvalValue, Vec<usize>>>,
}

impl Layer {
    fn from_claims(claims: Vec<ClaimInstance>) -> Self {
        let mut layer = Layer::default();
        for claim in claims {
            layer.push(claim);
        }
        layer
    }

    fn push(&mut self, claim: ClaimInstance) {
        let i = self.claims.len();
        let entry = self
            .by_predicate
            .entry(claim.predicate.clone())
            .or_default();
        entry.all.push(i);
        if entry.by_arg.len() < claim.args.len() {
            entry.by_arg.resize_with(claim.args.len(), HashMap::new);
        }
        for (pos, value) in claim.args.iter().enumerate() {
            entry.by_arg[pos].entry(value.clone()).or_default().push(i);
        }
        self.claims.push(claim);
    }

    fn bucket(&self, predicate: &PredicateName) -> &[usize] {
        self.by_predicate
            .get(predicate)
            .map_or(&[], |idx| idx.all.as_slice())
    }

    fn arg_bucket(
        &self,
        predicate: &PredicateName,
        position: usize,
        value: &EvalValue,
    ) -> Option<&[usize]> {
        self.by_predicate
            .get(predicate)
            .and_then(|idx| idx.by_arg.get(position))
            .and_then(|m| m.get(value))
            .map(Vec::as_slice)
    }

    /// The positions to check for an exact copy of `claim`: the
    /// smallest bucket one of its arguments names, else every claim of
    /// its predicate.
    fn candidates_for(&self, claim: &ClaimInstance) -> &[usize] {
        let mut best = self.bucket(&claim.predicate);
        for (pos, value) in claim.args.iter().enumerate() {
            match self.arg_bucket(&claim.predicate, pos, value) {
                None => return &[],
                Some(bucket) if bucket.len() < best.len() => best = bucket,
                Some(_) => {}
            }
        }
        best
    }
}

/// The compaction floor: below this much churn the overlay is never
/// folded, whatever the base's size. A performance policy, not a
/// semantic constant; the logical state is the same either way.
const COMPACTION_FLOOR: usize = 512;

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("claims", &self.claims().to_vec())
            .finish_non_exhaustive()
    }
}

impl PartialEq for State {
    fn eq(&self, other: &Self) -> bool {
        self.live == other.live && self.claims().iter().eq(other.claims().iter())
    }
}

impl Eq for State {}

impl State {
    /// Build a `State` from a vector of admitted claims: one base, an
    /// empty overlay. The claims are kept in the order supplied.
    pub fn from_claims(claims: Vec<ClaimInstance>) -> Self {
        let live = claims.len();
        Self {
            base: Arc::new(Layer::from_claims(claims)),
            overlay: Layer::default(),
            dead_overlay: Vec::new(),
            dead_base: HashSet::new(),
            live,
        }
    }

    /// The state after retracting `retracted` and then admitting
    /// `asserted`: retracting what is absent and admitting what is
    /// present are no-ops, and a claim retracted and admitted in one
    /// delta ends up at the tail. Shares this state's base; copies the
    /// overlay.
    pub(crate) fn with_delta(
        &self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
    ) -> State {
        let threshold = (self.base.claims.len() / 8).max(COMPACTION_FLOOR);
        self.with_delta_under(asserted, retracted, threshold)
    }

    /// `with_delta` folding into a fresh base once the churn
    /// (retracted base positions plus every overlay slot, dead or
    /// alive) exceeds `threshold`, so a test can drive every layering
    /// of one logical history.
    pub(crate) fn with_delta_under(
        &self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        threshold: usize,
    ) -> State {
        let mut next = self.clone();
        for claim in retracted {
            next.retract(claim);
        }
        for claim in asserted {
            if !next.contains(claim) {
                next.overlay.push(claim.clone());
                next.dead_overlay.push(false);
                next.live += 1;
            }
        }
        let churn = next.dead_base.len() + next.overlay.claims.len();
        if churn > threshold {
            return State::from_claims(next.claims().to_vec());
        }
        next
    }

    fn retract(&mut self, claim: &ClaimInstance) {
        let base = Arc::clone(&self.base);
        for &i in base.candidates_for(claim) {
            if base.claims[i] == *claim && self.dead_base.insert(i) {
                self.live -= 1;
            }
        }
        let Self {
            overlay,
            dead_overlay,
            live,
            ..
        } = self;
        for &i in overlay.candidates_for(claim) {
            if !dead_overlay[i] && overlay.claims[i] == *claim {
                dead_overlay[i] = true;
                *live -= 1;
            }
        }
    }

    /// Whether an exact copy of `claim` is admitted.
    pub(crate) fn contains(&self, claim: &ClaimInstance) -> bool {
        self.base
            .candidates_for(claim)
            .iter()
            .any(|&i| !self.dead_base.contains(&i) && self.base.claims[i] == *claim)
            || self
                .overlay
                .candidates_for(claim)
                .iter()
                .any(|&i| !self.dead_overlay[i] && self.overlay.claims[i] == *claim)
    }

    /// All admitted claims in logical order. Read-only.
    pub fn claims(&self) -> Claims<'_> {
        Claims { state: self }
    }

    /// Every admitted claim whose predicate name matches `predicate`,
    /// in logical order. `O(1)` to find the buckets; iteration is
    /// linear in their size.
    pub fn claims_for<'a>(
        &'a self,
        predicate: &str,
    ) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        let name = PredicateName::from(predicate);
        let base = self.base.bucket(&name);
        let overlay = self.overlay.bucket(&name);
        base.iter()
            .filter(move |i| !self.dead_base.contains(i))
            .map(move |&i| &self.base.claims[i])
            .chain(
                overlay
                    .iter()
                    .filter(move |&&i| !self.dead_overlay[i])
                    .map(move |&i| &self.overlay.claims[i]),
            )
    }

    /// Like [`State::claims_for`] but takes the typed name the hot
    /// evaluator path already holds, so no name is allocated per call.
    pub(crate) fn claims_for_name<'a>(
        &'a self,
        predicate: &PredicateName,
    ) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        self.base
            .bucket(predicate)
            .iter()
            .filter(move |i| !self.dead_base.contains(i))
            .map(move |&i| &self.base.claims[i])
            .chain(
                self.overlay
                    .bucket(predicate)
                    .iter()
                    .filter(move |&&i| !self.dead_overlay[i])
                    .map(move |&i| &self.overlay.claims[i]),
            )
    }

    /// The claims a ground argument narrows a pattern to: every
    /// admitted claim of `predicate` with `value` at `position`.
    /// `None` when no claim of this predicate ever carried this value
    /// at this position, which the caller uses to short-circuit an
    /// empty intersection; a bucket whose every entry was retracted is
    /// `Some` and yields nothing, which reads the same.
    pub(crate) fn claim_candidates(
        &self,
        predicate: &PredicateName,
        position: usize,
        value: &EvalValue,
    ) -> Option<CandidateBucket<'_>> {
        let base = self.base.arg_bucket(predicate, position, value);
        let overlay = self.overlay.arg_bucket(predicate, position, value);
        if base.is_none() && overlay.is_none() {
            return None;
        }
        Some(CandidateBucket {
            state: self,
            base: base.unwrap_or(&[]),
            overlay: overlay.unwrap_or(&[]),
        })
    }

    /// Total number of admitted claims across all predicates.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
}

/// The admitted claims of a [`State`], in logical order.
#[derive(Clone, Copy)]
pub struct Claims<'a> {
    state: &'a State,
}

impl<'a> Claims<'a> {
    pub fn iter(&self) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        let state = self.state;
        state
            .base
            .claims
            .iter()
            .enumerate()
            .filter(move |(i, _)| !state.dead_base.contains(i))
            .map(|(_, c)| c)
            .chain(
                state
                    .overlay
                    .claims
                    .iter()
                    .zip(&state.dead_overlay)
                    .filter(|(_, dead)| !**dead)
                    .map(|(c, _)| c),
            )
    }

    pub fn len(&self) -> usize {
        self.state.live
    }

    pub fn is_empty(&self) -> bool {
        self.state.live == 0
    }

    pub fn to_vec(self) -> Vec<ClaimInstance> {
        self.iter().cloned().collect()
    }
}

impl std::fmt::Debug for Claims<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for Claims<'a> {
    type Item = &'a ClaimInstance;
    type IntoIter = Box<dyn Iterator<Item = &'a ClaimInstance> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

/// The admitted claims of one predicate sharing one ground argument
/// value: what the evaluator checks a pattern against once an argument
/// has narrowed it. Layering is the state's business; the evaluator
/// sees an estimated size and the claims.
pub(crate) struct CandidateBucket<'a> {
    state: &'a State,
    base: &'a [usize],
    overlay: &'a [usize],
}

impl<'a> CandidateBucket<'a> {
    /// An upper bound on the claims the bucket yields, for choosing the
    /// smallest bucket; retracted entries still count.
    pub(crate) fn estimate_len(&self) -> usize {
        self.base.len() + self.overlay.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        let state = self.state;
        self.base
            .iter()
            .filter(move |i| !state.dead_base.contains(i))
            .map(move |&i| &state.base.claims[i])
            .chain(
                self.overlay
                    .iter()
                    .filter(move |&&i| !state.dead_overlay[i])
                    .map(move |&i| &state.overlay.claims[i]),
            )
    }
}

/// Variable bindings used during expression evaluation and
/// transformation execution. Maps variable name to resolved
/// [`EvalValue`].
pub type Bindings = HashMap<Var, EvalValue>;
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
    pub name: IntentName,
    pub args: Vec<EvalValue>,
}
