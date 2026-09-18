//! Admission: the rule under which a staged transition is accepted.
//!
//! An invariant describes what lawful state means and stays a
//! whole-state predicate everywhere else. Admission is case-local
//! revalidation: every case the transition's effective delta could
//! affect must satisfy each invariant afterwards; a case the delta
//! cannot reach may remain as history left it; and where the impact
//! cannot be bounded safely the whole invariant is checked. Inherited
//! dirt therefore blocks only the transitions that touch it, which is
//! what rule versioning and exception claims need, and a transition
//! that changes nothing is admitted whatever the history holds.
//!
//! The effective delta is the admitted-set change between the
//! pre-state and the candidate, never the staged vectors: a duplicate
//! admit, a retract of what is absent, and a retract followed by a
//! re-admit change nothing and affect nothing.

use std::borrow::Cow;

use crate::impact::ImpactPlan;
use crate::ir::{Definition, Invariant};
use crate::state::{ClaimInstance, State};

/// The rules a transition is admitted under, with each invariant's
/// impact plan built once beside it. A programme object holds its
/// plans and lends them; a caller with bare slices builds them.
pub struct Admission<'a> {
    pub invariants: &'a [Invariant],
    pub definitions: &'a [Definition],
    plans: Cow<'a, [ImpactPlan]>,
}

impl<'a> Admission<'a> {
    /// Rules from bare slices, planning each invariant now.
    pub fn of(invariants: &'a [Invariant], definitions: &'a [Definition]) -> Self {
        Self {
            invariants,
            definitions,
            plans: Cow::Owned(invariants.iter().map(ImpactPlan::new).collect()),
        }
    }

    /// Rules with plans built earlier, one per invariant in order.
    pub fn with_plans(
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

    pub fn plans(&self) -> &[ImpactPlan] {
        &self.plans
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
    pub fn is_empty(&self) -> bool {
        self.asserted.is_empty() && self.retracted.is_empty()
    }

    /// The one rule, over whatever knows the pre-state's membership:
    /// retractions apply first, then admissions, so an admission is
    /// effective only when the claim was absent, and a retraction only
    /// when the claim was present and is not re-admitted in the same
    /// delta. A claim retracted and re-admitted is present on both
    /// sides and counts on neither.
    pub fn of(
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
        present: impl Fn(&ClaimInstance) -> bool,
    ) -> Self {
        let mut out = EffectiveDelta::default();
        for claim in asserted {
            if !present(claim) && !out.asserted.contains(claim) {
                out.asserted.push(claim.clone());
            }
        }
        for claim in retracted {
            if present(claim) && !asserted.contains(claim) && !out.retracted.contains(claim) {
                out.retracted.push(claim.clone());
            }
        }
        out
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
