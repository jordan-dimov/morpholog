//! Admission: the rule under which a staged transition is accepted.
//!
//! An invariant is still a whole-state predicate. Admission only
//! rechecks the cases a transition could affect: those must satisfy
//! every invariant afterwards, while cases it cannot reach stay as
//! history left them. If the impact cannot be bounded safely, the whole
//! invariant is checked. So old bad data blocks only the transitions
//! that touch it, and a transition that changes nothing is always
//! admitted.
//!
//! The effective delta is the change in the admitted set, not the
//! staged lists: a duplicate admit, a retract of something absent, and
//! a retract then re-admit change nothing.

use std::borrow::Cow;
use std::collections::HashSet;

use crate::impact::ImpactPlan;
use crate::ir::{Definition, Invariant};
use crate::state::{ClaimInstance, State};

/// The rules a transition is admitted under, with each invariant's
/// impact plan built once beside it. Only a [`crate::PreparedProgram`]
/// hands one out, so its rules are always a validated programme's: a
/// caller may read them, never replace them.
pub struct Admission<'a> {
    invariants: &'a [Invariant],
    definitions: &'a [Definition],
    plans: Cow<'a, [ImpactPlan]>,
}

impl<'a> Admission<'a> {
    /// Rules from bare slices, planning each invariant now.
    pub(crate) fn of(invariants: &'a [Invariant], definitions: &'a [Definition]) -> Self {
        Self {
            invariants,
            definitions,
            plans: Cow::Owned(
                invariants
                    .iter()
                    .map(|inv| ImpactPlan::with_definitions(inv, definitions))
                    .collect(),
            ),
        }
    }

    /// Rules with plans built earlier, one per invariant in order.
    pub(crate) fn with_plans(
        invariants: &'a [Invariant],
        definitions: &'a [Definition],
        plans: &'a [ImpactPlan],
    ) -> Self {
        assert_eq!(
            invariants.len(),
            plans.len(),
            "one impact plan per invariant, in order"
        );
        Self {
            invariants,
            definitions,
            plans: Cow::Borrowed(plans),
        }
    }

    /// The invariants this admission checks, in programme order.
    pub fn invariants(&self) -> &'a [Invariant] {
        self.invariants
    }

    /// The definitions those invariants call.
    pub fn definitions(&self) -> &'a [Definition] {
        self.definitions
    }

    pub(crate) fn plans(&self) -> &[ImpactPlan] {
        &self.plans
    }

    /// One contiguous run of these invariants with their plans. Public
    /// only through [`crate::execution::admission_range`], which states
    /// the caller's obligation.
    pub(crate) fn range(&self, range: std::ops::Range<usize>) -> Admission<'_> {
        Admission {
            invariants: &self.invariants[range.clone()],
            definitions: self.definitions,
            plans: Cow::Borrowed(&self.plans[range]),
        }
    }
}

/// The admitted-set change a transition makes: what the candidate
/// holds that the pre-state did not, and the reverse.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectiveDelta {
    pub asserted: Vec<ClaimInstance>,
    pub retracted: Vec<ClaimInstance>,
}

impl EffectiveDelta {
    /// Computes the delta, with `present` answering pre-state membership.
    /// An admission counts only if the claim was absent. A retraction
    /// counts only if the claim was present and is not re-admitted in
    /// the same delta, so a retract-then-re-admit counts on neither side.
    pub fn of(
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        present: impl Fn(&ClaimInstance) -> bool,
    ) -> Self {
        let mut out = EffectiveDelta::default();
        let mut admitted = Seen::new(asserted.len());
        for claim in asserted {
            if !present(claim) && admitted.first(claim, &out.asserted) {
                out.asserted.push(claim.clone());
            }
        }
        let staged = Seen::of(asserted);
        let mut gone = Seen::new(retracted.len());
        for claim in retracted {
            if present(claim) && !staged.holds(claim, asserted) && gone.first(claim, &out.retracted)
            {
                out.retracted.push(claim.clone());
            }
        }
        out
    }
}

/// Membership in a list being built or already built: a scan for the
/// few claims an ordinary act stages, a set for an act that stages many,
/// where a scan per claim would make the delta quadratic.
enum Seen<'c> {
    Scan,
    Set(HashSet<&'c ClaimInstance>),
}

impl<'c> Seen<'c> {
    /// Past this many claims a scan per claim costs more than a set.
    const SCAN_UP_TO: usize = 32;

    fn new(len: usize) -> Self {
        if len > Self::SCAN_UP_TO {
            Seen::Set(HashSet::with_capacity(len))
        } else {
            Seen::Scan
        }
    }

    fn of(claims: &'c [ClaimInstance]) -> Self {
        if claims.len() > Self::SCAN_UP_TO {
            Seen::Set(claims.iter().collect())
        } else {
            Seen::Scan
        }
    }

    /// Whether `claim` is not yet in `list`, recording it if so; `list`
    /// is the output so far, the one a scan reads.
    fn first(&mut self, claim: &'c ClaimInstance, list: &[ClaimInstance]) -> bool {
        match self {
            Seen::Scan => !list.contains(claim),
            Seen::Set(set) => set.insert(claim),
        }
    }

    /// Whether `claim` is in `list`, the list this was made of.
    fn holds(&self, claim: &ClaimInstance, list: &[ClaimInstance]) -> bool {
        match self {
            Seen::Scan => list.contains(claim),
            Seen::Set(set) => set.contains(claim),
        }
    }
}

/// The effective delta of staging `asserted` and `retracted` over
/// `pre`, the pre-state answering membership.
pub fn effective_delta(
    pre: &State,
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> EffectiveDelta {
    EffectiveDelta::of(asserted, retracted, |c| pre.contains(c))
}
