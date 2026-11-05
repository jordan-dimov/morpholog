//! Runtime state types and the in-memory state store.
//!
//! `EvalValue` is the runtime form of the IR's `Value`.
//! `ClaimInstance` and `IntentInstance` are grounded claims and intents.
//! `State` holds the admitted claims, indexed by predicate and by
//! argument position. `Bindings` maps variables to values during
//! evaluation.

use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
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
    /// A calendar span (whole months plus whole days). Only an
    /// arithmetic operand: claims, intents, and transition arguments
    /// refuse it, so it never reaches storage or the wire.
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

/// An IR literal is a runtime value already; nothing can fail here.
impl From<&crate::ir::Value> for EvalValue {
    fn from(value: &crate::ir::Value) -> Self {
        use crate::ir::Value;
        match value {
            Value::Decimal(d) => EvalValue::Decimal(d.value()),
            Value::Subject(s) => EvalValue::Subject(s.clone()),
            Value::Date(d) => EvalValue::Date(*d),
            Value::Timestamp(t) => EvalValue::Timestamp(*t),
            Value::Duration(d) => EvalValue::Duration(*d),
            Value::CalendarSpan(s) => EvalValue::CalendarSpan(*s),
            Value::Quantity { amount, unit } => EvalValue::Quantity {
                amount: amount.value(),
                unit: unit.clone(),
            },
        }
    }
}

impl EvalValue {
    /// Does this value carry a calendar span, directly or inside a
    /// collection? Every storage and wire boundary refuses such a value.
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
/// Stored as-is in the JSONB arrays `audit.asserted_claims` and
/// `audit.retracted_claims`.
///
/// The `claims` table instead splits it into `predicate_name` (text) and
/// `arguments` (a JSONB array, enforced by a CHECK constraint). The
/// `claim_args_serialise_as_a_json_array` test pins this.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClaimInstance {
    pub predicate: PredicateName,
    pub args: Vec<EvalValue>,
}

/// What the state does that costs in proportion to its size, counted so
/// a measurement can attribute a run to copies and rebuilds apart, and a
/// test can hold a fold to doing neither. Per thread, so tests running
/// beside each other read their own. Only in test builds and under the
/// `measure` feature the bench enables; an ordinary build compiles the
/// increments to nothing.
#[cfg(any(test, feature = "measure"))]
#[doc(hidden)]
pub mod tally {
    use std::cell::Cell;

    thread_local! {
        /// Clones of a `State` (each copies the overlay and the retracted
        /// base positions), the entries they copied, compactions that
        /// rebuilt the whole state, and the live claims they re-indexed.
        static COUNTS: Cell<[u64; 4]> = const { Cell::new([0; 4]) };
    }

    fn add(index: usize, n: usize) {
        COUNTS.with(|c| {
            let mut counts = c.get();
            counts[index] += n as u64;
            c.set(counts);
        });
    }

    pub(super) fn clone_of(overlay_entries: usize) {
        add(0, 1);
        add(1, overlay_entries);
    }

    pub(super) fn rebuild_of(live: usize) {
        add(2, 1);
        add(3, live);
    }

    /// This thread's counts: clones, overlay entries copied, rebuilds,
    /// claims rebuilt.
    pub fn snapshot() -> [u64; 4] {
        COUNTS.with(Cell::get)
    }

    thread_local! {
        /// Binding maps the evaluator copied to extend or keep a match,
        /// and the entries those copies held.
        static BINDINGS: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
    }

    pub(crate) fn binding_copy(entries: usize) {
        BINDINGS.with(|c| {
            let [copies, copied] = c.get();
            c.set([copies + 1, copied + entries as u64]);
        });
    }

    /// This thread's binding counts: maps copied, entries copied.
    pub fn bindings() -> [u64; 2] {
        BINDINGS.with(Cell::get)
    }
}

/// The admitted state: a set of grounded [`ClaimInstance`]s that
/// invariants are evaluated against. Identity is `(predicate, args)`.
/// The PG adapter persists it as rows in `morpholog.claims`.
///
/// Two layers: a base shared with every state derived from it, and an
/// overlay of what changed since. A candidate built by `with_delta`
/// copies only the overlay, never the base. The overlay is folded into a
/// fresh base by one of two upkeeps: [`State::apply`] keeps it small, so
/// a copy stays cheap; [`State::apply_replayed`] lets it grow, for a fold
/// that never copies.
///
/// Logical order is the base in construction order minus retractions,
/// then additions in order. A claim retracted and re-admitted moves to
/// the tail. Two states are equal when their logical sequences are
/// equal, whatever their layering.
///
/// Indexed by predicate and by `(predicate, arg position, arg value)`,
/// so a ground argument narrows a lookup to the smallest bucket.
/// Construct via [`State::from_claims`] or [`State::default`].
#[derive(Default)]
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
    /// Claims the readers below examined, so a test can hold the work an
    /// evaluation does to the cases it was given.
    #[cfg(test)]
    visited: std::cell::Cell<u64>,
}

/// How a state's claims are laid out: the base it was last rebuilt to,
/// the base positions retracted since, and the overlay's live and dead
/// entries. What a compaction policy decides on.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layering {
    pub base: usize,
    pub base_dead: usize,
    pub overlay_live: usize,
    pub overlay_dead: usize,
}

impl Layering {
    /// The ordinary upkeep: fold once the overlay's entries, live or
    /// dead, and the base's retracted positions pass an eighth of the
    /// base. It keeps the overlay small, so a copy of the state, which an
    /// owned candidate is, stays cheap.
    #[doc(hidden)]
    pub fn compacts_ordinarily(self) -> bool {
        self.base_dead + self.overlay_live + self.overlay_dead
            > (self.base / 8).max(COMPACTION_FLOOR)
    }

    /// A replay's upkeep, for a fold that never copies the state it
    /// advances. Dead entries cost memory and the slots a lookup skips,
    /// so they are held to the live count; live overlay entries cost only
    /// a copy, so the overlay may grow to the base's size before it is
    /// folded, and a growing history is re-indexed about twice over in
    /// all rather than eight times.
    #[cfg(any(test, feature = "measure"))]
    #[doc(hidden)]
    pub fn compacts_in_replay(self) -> bool {
        self.compacts_in_replay_under(REPLAY_DEAD_FLOOR, REPLAY_OVERLAY_FLOOR)
    }

    fn compacts_in_replay_under(self, dead_floor: usize, overlay_floor: usize) -> bool {
        let live = self.base - self.base_dead + self.overlay_live;
        self.base_dead + self.overlay_dead > live.max(dead_floor)
            || self.overlay_live > self.base.max(overlay_floor)
    }
}

/// The counters' absence: the same calls, compiled to nothing.
#[cfg(not(any(test, feature = "measure")))]
pub(crate) mod tally {
    #[inline(always)]
    pub(super) fn clone_of(_overlay_entries: usize) {}
    #[inline(always)]
    pub(super) fn rebuild_of(_live: usize) {}
    #[inline(always)]
    pub(crate) fn binding_copy(_entries: usize) {}
}

impl Clone for State {
    fn clone(&self) -> Self {
        tally::clone_of(self.overlay.claims.len() + self.dead_base.len());
        Self {
            base: Arc::clone(&self.base),
            overlay: self.overlay.clone(),
            dead_overlay: self.dead_overlay.clone(),
            dead_base: self.dead_base.clone(),
            live: self.live,
            #[cfg(test)]
            visited: self.visited.clone(),
        }
    }
}

/// One indexed run of claims: the base, or the overlay.
#[derive(Clone, Default)]
struct Layer {
    claims: Vec<ClaimInstance>,
    by_predicate: HashMap<PredicateName, PredicateIndex>,
}

/// One predicate's index on a [`Layer`]: the positions of its claims,
/// plus a lookup by `(arg position, arg value)`.
///
/// `by_arg` grows lazily: position `p` gets a map only once some claim
/// has at least `p + 1` args.
#[derive(Clone, Default)]
struct PredicateIndex {
    /// Indices into `Layer.claims` for every claim with this predicate.
    all: Vec<usize>,
    /// `by_arg[position][value]` -> indices into `Layer.claims` for
    /// claims with this predicate where `args[position] == value`.
    by_arg: Vec<ArgIndex>,
}

/// A rule's every lookup by argument value hashes the value, so this
/// index uses a faster keyed hash than the rest of the kernel.
type ArgIndex = HashMap<EvalValue, Vec<usize>, ArgHasher>;

/// aHash keyed per map from the standard library's random keys, the ones
/// its own maps use, so no two maps share keys and no randomness crate
/// is needed.
#[derive(Clone)]
struct ArgHasher(ahash::RandomState);

impl Default for ArgHasher {
    fn default() -> Self {
        let keys = std::collections::hash_map::RandomState::new();
        let key = |i: u64| keys.hash_one(i);
        Self(ahash::RandomState::with_seeds(
            key(0),
            key(1),
            key(2),
            key(3),
        ))
    }
}

impl BuildHasher for ArgHasher {
    type Hasher = ahash::AHasher;

    fn build_hasher(&self) -> Self::Hasher {
        self.0.build_hasher()
    }
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
            entry
                .by_arg
                .resize_with(claim.args.len(), ArgIndex::default);
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

    fn contains(&self, claim: &ClaimInstance) -> bool {
        self.candidates_for(claim)
            .iter()
            .any(|&i| self.claims[i] == *claim)
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

/// Below this much churn the overlay is never folded, whatever the
/// base's size. A performance setting; the logical state is unaffected.
const COMPACTION_FLOOR: usize = 512;

/// A replay's floors: below these many dead entries, or live overlay
/// entries, its state is not folded for that cause. Performance
/// settings, like the floor above.
const REPLAY_DEAD_FLOOR: usize = 1024;
const REPLAY_OVERLAY_FLOOR: usize = 4096;

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
            #[cfg(test)]
            visited: Default::default(),
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
        let mut next = self.clone();
        next.apply(asserted, retracted);
        next
    }

    /// Advance this state in place by one transition's delta:
    /// retractions first, then admissions not already present.
    /// `clone()` is a cheap snapshot: it copies only what changed since
    /// the base, which this keeps small.
    pub fn apply(&mut self, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) {
        self.apply_under(asserted, retracted, usize::MAX);
        if self.layering_now().compacts_ordinarily() {
            self.rebuild();
        }
    }

    /// [`State::apply`] for a fold over recorded history that never
    /// copies the state it advances: the same claims in the same order,
    /// kept up for the fold rather than for copies. The overlay is let
    /// grow, so a copy taken from a state advanced this way costs more.
    pub fn apply_replayed(&mut self, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) {
        self.apply_replayed_under(asserted, retracted, REPLAY_DEAD_FLOOR, REPLAY_OVERLAY_FLOOR);
    }

    /// [`State::apply_replayed`] with explicit floors, so a test can
    /// drive its compactions at any size.
    pub(crate) fn apply_replayed_under(
        &mut self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        dead_floor: usize,
        overlay_floor: usize,
    ) {
        self.apply_under(asserted, retracted, usize::MAX);
        if self
            .layering_now()
            .compacts_in_replay_under(dead_floor, overlay_floor)
        {
            self.rebuild();
        }
    }

    /// [`State::apply`] with an explicit compaction `threshold` (on
    /// retracted base positions plus overlay slots), so a test can drive
    /// every layering of one logical history.
    pub(crate) fn apply_under(
        &mut self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        threshold: usize,
    ) {
        for claim in retracted {
            self.retract(claim);
        }
        for claim in asserted {
            if !self.contains(claim) {
                self.overlay.push(claim.clone());
                self.dead_overlay.push(false);
                self.live += 1;
            }
        }
        let churn = self.dead_base.len() + self.overlay.claims.len();
        if churn > threshold {
            self.rebuild();
        }
    }

    /// [`State::apply`] with the compaction decided by the caller from
    /// the layering after the change, so a measurement can hold it off
    /// or compare policies over one history.
    #[cfg(any(test, feature = "measure"))]
    #[doc(hidden)]
    pub fn apply_compacting_when(
        &mut self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        compact: impl FnOnce(Layering) -> bool,
    ) {
        self.apply_under(asserted, retracted, usize::MAX);
        if compact(self.layering()) {
            self.rebuild();
        }
    }

    #[cfg(any(test, feature = "measure"))]
    #[doc(hidden)]
    pub fn layering(&self) -> Layering {
        self.layering_now()
    }

    fn layering_now(&self) -> Layering {
        let base_live = self.base.claims.len() - self.dead_base.len();
        let overlay_live = self.live - base_live;
        Layering {
            base: self.base.claims.len(),
            base_dead: self.dead_base.len(),
            overlay_live,
            overlay_dead: self.overlay.claims.len() - overlay_live,
        }
    }

    #[cfg(any(test, feature = "measure"))]
    #[doc(hidden)]
    pub fn admits(&self, claim: &ClaimInstance) -> bool {
        self.contains(claim)
    }

    /// The whole state from its live claims, in logical order, indexed
    /// afresh: what a compaction does.
    fn rebuild(&mut self) {
        tally::rebuild_of(self.live);
        *self = State::from_claims(self.claims().to_vec());
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
        self.visit(base.len() + overlay.len());
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
    pub(crate) fn claims_for_name<'a>(&'a self, predicate: &PredicateName) -> NamedClaims<'a> {
        self.visit(self.base.bucket(predicate).len() + self.overlay.bucket(predicate).len());
        NamedClaims {
            state: self,
            base: self.base.bucket(predicate).iter(),
            overlay: self.overlay.bucket(predicate).iter(),
            retracted: None,
            extra: None,
        }
    }

    /// Every admitted claim of `predicate` with `value` at `position`.
    /// `None` when no claim ever carried that value there. A bucket whose
    /// entries were all retracted is `Some` and yields nothing, which
    /// means the same.
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
            retracted: None,
            extra: None,
        })
    }

    #[cfg(test)]
    fn visit(&self, claims: usize) {
        self.visited.set(self.visited.get() + claims as u64);
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn visit(&self, _claims: usize) {}

    /// Claims the readers examined since this state was built or cloned.
    #[cfg(test)]
    pub(crate) fn claims_visited(&self) -> u64 {
        self.visited.get()
    }

    /// Total number of admitted claims across all predicates.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }
}

/// The admitted claims of one predicate, in logical order: the state's,
/// and over a candidate view the staged admissions after them, with
/// the staged retractions left out.
pub(crate) struct NamedClaims<'a> {
    state: &'a State,
    base: std::slice::Iter<'a, usize>,
    overlay: std::slice::Iter<'a, usize>,
    retracted: Option<&'a HashSet<ClaimInstance>>,
    extra: Option<(&'a Layer, std::slice::Iter<'a, usize>)>,
}

impl<'a> Iterator for NamedClaims<'a> {
    type Item = &'a ClaimInstance;

    fn next(&mut self) -> Option<&'a ClaimInstance> {
        let kept = |retracted: Option<&HashSet<ClaimInstance>>, c: &ClaimInstance| {
            retracted.is_none_or(|r| !r.contains(c))
        };
        for &i in self.base.by_ref() {
            if !self.state.dead_base.contains(&i) {
                let c = &self.state.base.claims[i];
                if kept(self.retracted, c) {
                    return Some(c);
                }
            }
        }
        for &i in self.overlay.by_ref() {
            if !self.state.dead_overlay[i] {
                let c = &self.state.overlay.claims[i];
                if kept(self.retracted, c) {
                    return Some(c);
                }
            }
        }
        let (layer, positions) = self.extra.as_mut()?;
        positions.next().map(|&i| &layer.claims[i])
    }
}

/// A proposal's candidate state read without being built: the pre-state
/// with the staged delta over it. It answers as the owned state built
/// from the same delta would: retractions first, admissions not already
/// present after everything else, a claim retracted and admitted in one
/// delta at the tail. Nothing of the pre-state is copied; the delta is
/// indexed on its own.
pub struct Candidate<'a> {
    pre: &'a State,
    /// The retractions that remove something: those present before.
    retracted: HashSet<ClaimInstance>,
    /// The admissions that add something, in order, each once.
    asserted: Layer,
}

impl<'a> Candidate<'a> {
    pub fn new(pre: &'a State, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) -> Self {
        let retracted: HashSet<ClaimInstance> = retracted
            .iter()
            .filter(|c| pre.contains(c))
            .cloned()
            .collect();
        let mut layer = Layer::default();
        for claim in asserted {
            let present = pre.contains(claim) && !retracted.contains(claim);
            if !present && !layer.contains(claim) {
                layer.push(claim.clone());
            }
        }
        Self {
            pre,
            retracted,
            asserted: layer,
        }
    }
}

/// The state an evaluation reads: one it owns, or a candidate view.
#[derive(Clone, Copy)]
pub(crate) enum StateRef<'a> {
    Owned(&'a State),
    Candidate(&'a Candidate<'a>),
}

impl<'a> From<&'a State> for StateRef<'a> {
    fn from(state: &'a State) -> Self {
        StateRef::Owned(state)
    }
}

impl<'a> From<&'a Candidate<'a>> for StateRef<'a> {
    fn from(candidate: &'a Candidate<'a>) -> Self {
        StateRef::Candidate(candidate)
    }
}

impl<'a> StateRef<'a> {
    pub(crate) fn claims_for_name(self, predicate: &PredicateName) -> NamedClaims<'a> {
        match self {
            StateRef::Owned(state) => state.claims_for_name(predicate),
            StateRef::Candidate(c) => {
                let mut named = c.pre.claims_for_name(predicate);
                named.retracted = Some(&c.retracted);
                named.extra = Some((&c.asserted, c.asserted.bucket(predicate).iter()));
                named
            }
        }
    }

    pub(crate) fn claim_candidates(
        self,
        predicate: &PredicateName,
        position: usize,
        value: &EvalValue,
    ) -> Option<CandidateBucket<'a>> {
        match self {
            StateRef::Owned(state) => state.claim_candidates(predicate, position, value),
            StateRef::Candidate(c) => {
                let extra = c.asserted.arg_bucket(predicate, position, value);
                let Some(mut bucket) = c.pre.claim_candidates(predicate, position, value) else {
                    let extra = extra?;
                    return Some(CandidateBucket {
                        state: c.pre,
                        base: &[],
                        overlay: &[],
                        retracted: Some(&c.retracted),
                        extra: Some((&c.asserted, extra)),
                    });
                };
                bucket.retracted = Some(&c.retracted);
                bucket.extra = extra.map(|e| (&c.asserted, e));
                Some(bucket)
            }
        }
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

/// The admitted claims of one predicate sharing one argument value,
/// hiding the state's layers from the evaluator.
pub(crate) struct CandidateBucket<'a> {
    state: &'a State,
    base: &'a [usize],
    overlay: &'a [usize],
    /// Over a candidate view: the staged retractions, left out.
    retracted: Option<&'a HashSet<ClaimInstance>>,
    /// Over a candidate view: the staged admissions in this bucket.
    extra: Option<(&'a Layer, &'a [usize])>,
}

impl<'a> CandidateBucket<'a> {
    /// An upper bound on the claims the bucket yields, for choosing the
    /// smallest bucket; retracted entries still count.
    pub(crate) fn estimate_len(&self) -> usize {
        self.base.len() + self.overlay.len() + self.extra.map_or(0, |(_, e)| e.len())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        self.state.visit(self.estimate_len());
        let state = self.state;
        let retracted = self.retracted;
        let kept = move |c: &&'a ClaimInstance| retracted.is_none_or(|r| !r.contains(*c));
        self.base
            .iter()
            .filter(move |i| !state.dead_base.contains(i))
            .map(move |&i| &state.base.claims[i])
            .filter(kept)
            .chain(
                self.overlay
                    .iter()
                    .filter(move |&&i| !state.dead_overlay[i])
                    .map(move |&i| &state.overlay.claims[i])
                    .filter(kept),
            )
            .chain(
                self.extra.into_iter().flat_map(|(layer, positions)| {
                    positions.iter().map(move |&i| &layer.claims[i])
                }),
            )
    }
}

/// Variable bindings used during expression evaluation and
/// transformation execution. Maps variable name to resolved
/// [`EvalValue`]. The evaluator copies these maps to extend a match or
/// restore a loop's frame, so each value is a [`Bound`]: a copy costs
/// the map's entries, not the size of what they hold (an act's
/// collection argument, say).
pub type Bindings = HashMap<Var, Bound>;

/// A bound value: shared when it owns heap memory, so copying a binding
/// map never copies it; inline otherwise, where a copy costs nothing and
/// a shared box would cost an allocation.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub enum Bound {
    Inline(EvalValue),
    Shared(Arc<EvalValue>),
}

impl From<EvalValue> for Bound {
    fn from(value: EvalValue) -> Self {
        match value {
            EvalValue::Subject(_) | EvalValue::Collection(_) | EvalValue::Quantity { .. } => {
                Bound::Shared(Arc::new(value))
            }
            inline => Bound::Inline(inline),
        }
    }
}

impl std::ops::Deref for Bound {
    type Target = EvalValue;
    fn deref(&self) -> &EvalValue {
        match self {
            Bound::Inline(value) => value,
            Bound::Shared(value) => value,
        }
    }
}

impl PartialEq for Bound {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for Bound {}

impl std::hash::Hash for Bound {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl Bound {
    /// The value, copied only if something else still shares it.
    pub(crate) fn into_value(self) -> EvalValue {
        match self {
            Bound::Inline(value) => value,
            Bound::Shared(value) => Arc::unwrap_or_clone(value),
        }
    }
}
/// A resolved intent: all args are values, ready to be enqueued in an outbox.
///
/// JSON encoding shape: `{ "name": "...", "args": [ ... ] }`.
///
/// Stored as-is in the JSONB array `audit.emitted_intents`.
///
/// The `outbox` table instead splits it into `intent_type` (text) and
/// `arguments` (a JSONB array, enforced by a CHECK constraint). The
/// `intent_args_serialise_as_a_json_array` test pins this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentInstance {
    pub name: IntentName,
    pub args: Vec<EvalValue>,
}

#[cfg(test)]
mod arg_hasher_tests {
    use super::*;

    /// Each argument index is keyed afresh, never from fixed seeds. This
    /// catches a fixed seed; it says nothing of how good the keys are.
    #[test]
    fn each_argument_index_is_keyed_afresh() {
        let value = EvalValue::Subject("row_1".into());
        let hashes: HashSet<u64> = (0..16)
            .map(|_| ArgHasher::default().hash_one(&value))
            .collect();
        assert_eq!(hashes.len(), 16, "two indexes hashed a value alike");
    }
}
