//! The value and propose helpers the kernel's own tests share. They mirror
//! `morpholog-test-support`, which these tests cannot use: its values
//! would come from a second copy of this crate. Proposals here evaluate IR
//! directly, below validation, which is what these tests are for.

use rust_decimal::Decimal;

use crate::{
    ClaimInstance, Definition, EvalError, EvalValue, Invariant, Outcome, State, Subject,
    Transformation, Transition,
};

pub(crate) fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(s))
}

pub(crate) fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

pub(crate) fn dec_str(s: &str) -> EvalValue {
    EvalValue::Decimal(s.parse::<Decimal>().expect("valid decimal string"))
}

pub(crate) fn ts(s: &str) -> EvalValue {
    EvalValue::Timestamp(s.parse().expect("test timestamp literal must parse"))
}

pub(crate) fn dur(s: &str) -> EvalValue {
    EvalValue::Duration(s.parse().expect("test duration literal must parse"))
}

pub(crate) fn cal_span(s: &str) -> EvalValue {
    EvalValue::CalendarSpan(
        crate::calendar::parse_calendar_span(s).expect("test calendar-span literal must parse"),
    )
}

pub(crate) fn qty(amount: &str, unit: &str) -> EvalValue {
    EvalValue::Quantity {
        amount: amount.parse().expect("test quantity amount must parse"),
        unit: crate::Unit::from(unit.to_string()),
    }
}

pub(crate) fn claim_instance(predicate: &str, args: &[EvalValue]) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args: args.to_vec(),
    }
}

pub(crate) fn test_actor() -> Subject {
    Subject::from("test_actor")
}

pub(crate) fn test_transition(t: &Transformation, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: t.name.clone(),
        args,
        actor: test_actor(),
    }
}

pub(crate) fn fresh() -> impl crate::SubjectSource {
    super::fresh()
}

pub(crate) fn propose_with_test_actor(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: &State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<Outcome, EvalError> {
    let transition = test_transition(t, args);
    crate::propose::propose(t, &transition, pre, invariants, definitions, &mut fresh())
}

pub(crate) fn must_accept(
    t: &Transformation,
    args: Vec<EvalValue>,
    pre: State,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> State {
    match propose_with_test_actor(t, args, &pre, invariants, definitions)
        .expect("propose should not error")
    {
        Outcome::Accepted {
            candidate_state, ..
        } => candidate_state,
        Outcome::Rejected { reason } => {
            panic!(
                "expected Accepted from `{}`, got Rejected: {reason}",
                t.name
            )
        }
    }
}
