//! Privileged execution seams.
//!
//! Ordinary execution uses the normal kernel entry points. These
//! operations exist for an execution layer that has already established
//! something the kernel cannot verify from its own inputs: a partition of
//! the invariants it has taken on itself, or the change a staged delta
//! made to the admitted set. A false but well-formed value changes the
//! semantic outcome, so each operation states the obligation its caller
//! assumes. Keeping those obligations is how an execution layer keeps the
//! law that preparation and execution may change cost, never meaning.
//!
//! An operation belongs here only if a well-typed caller could lie to
//! the kernel through it in a way that changes a decision.

use crate::admission::{Admission, EffectiveDelta};
use crate::eval::EvalError;
use crate::propose::{Outcome, StagedDelta, TraceSink, finish_staged_inner};
use crate::state::State;

/// The admission for one contiguous run of `admission`'s invariants, with
/// their impact plans: the part an execution layer that checks the other
/// invariants itself hands back to the kernel.
///
/// The caller owes the exact run its execution plan assigned to the
/// kernel, in programme order. Any other run names the wrong rule on a
/// refusal or leaves a rule unchecked.
///
/// # Panics
///
/// If `run` reaches past the invariants.
pub fn admission_range<'a>(
    admission: &'a Admission<'_>,
    run: std::ops::Range<usize>,
) -> Admission<'a> {
    admission.range(run)
}

/// [`finish_staged_delta_with`](crate::finish_staged_delta_with) under an
/// effective delta the execution layer established itself, so every
/// evaluator in one transaction classifies impact from one delta. The
/// candidate state is still `pre_state` plus the staged lists.
///
/// The caller owes the true change in the admitted set: the staged admits
/// that were absent and the staged retracts that were present and not
/// re-admitted, exactly as [`effective_delta`](crate::effective_delta)
/// computes it from a complete pre-state. A delta that understates the
/// change lets an invariant the change touches go unchecked, since impact
/// is classified from it and nothing else.
pub fn finish_staged_delta_with_effective(
    staged: StagedDelta,
    pre_state: &State,
    admission: &Admission<'_>,
    effective: &EffectiveDelta,
) -> Result<Outcome, EvalError> {
    finish_staged_inner(
        staged,
        pre_state,
        admission,
        Some(effective),
        &mut TraceSink::Off,
    )
}
