//! The evaluation memo changes no result. Every gallery programme, over
//! sampled states, gives the same explanation of every boundary argument
//! vector of every transformation, the same outcome and trace, and the
//! same derived rows, with the memo on and off. Explanations are compared as JSON, so
//! a decimal's scale counts. The switch is process-wide, so this binary
//! holds this one test.

#![expect(
    clippy::expect_used,
    reason = "a helper here fails fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;
use common::all_programs;

use morpholog_core::{State, Subject};
use morpholog_test_support::differential::{boundary_argument_cases, sample_state};

const DECIDED_AT: &str = "2026-01-01T00:00:00Z";

/// Everything the kernel answers about `state`, as text.
fn answers(program: &morpholog_core::Program, state: &State) -> Vec<String> {
    let validated = program.validated().expect("gallery programme validates");
    let mut out = Vec::new();
    for t in &program.transformations {
        for case in boundary_argument_cases(&validated, &t.name) {
            let transition = morpholog_test_support::transition_at(
                t,
                case.args.clone(),
                morpholog_test_support::test_actor(),
                DECIDED_AT,
            );
            let drawn = || (0..).map(|i| Subject::from(format!("drawn_{i}")));
            let explanation =
                morpholog_test_support::explain(program, &transition, state, &mut drawn());
            out.push(serde_json::to_string(&explanation).expect("an explanation serialises"));
            let traced = morpholog_test_support::propose_with_trace(
                program,
                &transition,
                state,
                &mut drawn(),
            );
            out.push(format!("{traced:?}"));
        }
    }
    for derived in &program.derived_claims {
        out.push(format!(
            "{:?}",
            validated.enumerate_derived(derived.predicate.as_str(), state)
        ));
    }
    out
}

#[test]
fn the_memo_changes_no_answer() {
    let mut compared = 0;
    for program in all_programs() {
        for salt in 0..3 {
            let state = sample_state(&program, 2, salt);
            morpholog_core::set_memo(false);
            let off = answers(&program, &state);
            morpholog_core::set_memo(true);
            let on = answers(&program, &state);
            assert_eq!(on.len(), off.len());
            for (on, off) in on.iter().zip(&off) {
                assert_eq!(on, off, "{} salt {salt}", program.name);
            }
            compared += on.len();
        }
    }
    assert!(compared > 1000, "answers compared: {compared}");
}
