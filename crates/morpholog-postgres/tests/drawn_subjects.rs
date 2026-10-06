//! No unrecorded input decides a commit. A history is committed through
//! every durable path, then each row is re-executed by the ordinary kernel
//! from the authenticated history and the row's recorded execution inputs,
//! under a programme matching the row's hash that implements its recorded
//! semantics: the state the earlier rows fold to, the row's arguments and
//! actor, and exactly the subjects it records drawing. No replay-specific
//! evaluator is involved.
//!
//! The attacker modelled here is no one: this is a sufficiency proof, not
//! a tamper test. It shows that no unrecorded input decides a commit made
//! under the current semantics.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::*;

use std::collections::HashMap;

use jiff::Timestamp;
use morpholog_core::{
    EvalError, EvalValue, Outcome, PreparedProgram, SEMANTICS_VERSION, State, Subject, Transition,
};
use morpholog_postgres::{
    AuditRow, CompensationSpec, Deliverer, DeliveryOutcome, InvariantPlan, OutboxRow,
    PgAtomicOutcome, PgProgram, PgTracedOutcome, ProcessOutcome, list_audit_rows,
    process_one_outbox_row, propose_against_pg, propose_against_pg_with_trace,
    propose_all_against_pg,
};

/// Refuses every delivery for good, so the processor compensates.
struct Refuses;

impl Deliverer for Refuses {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        DeliveryOutcome::NonRetryable {
            reason: "the herald is unreachable".to_string(),
        }
    }
}

const DRAWS: &str = "
program draws

predicate Note(id: Subject)
predicate Pair(note: Subject, first: Subject, second: Subject)

intent Announce(note: Subject, herald: Subject)

invariant pairs_belong_to_notes:
    Pair(n, _, _) implies Note(n)

transformation open(note):
    admit Note(note)

transformation pair(note):
    require Note(note)
    let first = new Subject()
    let second = new Subject()
    let unused = new Subject()
    let herald = new Subject()
    admit Pair(note, first, second)
    emit Announce(note, herald)
";

/// The same rules plus one more act, so rows name two programmes.
const DRAWS_REVISED: &str = "
transformation twin(note):
    require Note(note)
    let only = new Subject()
    admit Pair(note, only, only)
";

fn prepared(source: &str) -> PreparedProgram {
    PreparedProgram::new(morpholog_surface::parse_program(source).unwrap()).unwrap()
}

fn act(program: &PreparedProgram, name: &str, note: &str) -> Transition {
    test_transition(
        program.program().transformation(name).unwrap(),
        vec![subj(note)],
    )
}

/// One row's transition, rebuilt from what the row records.
fn recorded_transition(row: &AuditRow) -> Transition {
    Transition {
        transformation_name: row.transformation_name.clone(),
        args: row.arguments.clone(),
        actor: row.actor.clone(),
    }
}

/// Re-run `row` over `pre` with `drawn` as the subject source, returning
/// the outcome and the subjects left over.
fn rerun(
    programme: &PreparedProgram,
    row: &AuditRow,
    pre: &State,
    drawn: Vec<Subject>,
) -> (Result<Outcome, EvalError>, usize) {
    let mut source = drawn.into_iter();
    let outcome = programme
        .propose(&recorded_transition(row), pre, &mut source)
        .map(|o| o.expect("the recorded transformation is in the programme"));
    (outcome, source.count())
}

#[tokio::test]
async fn every_commit_reruns_from_its_history_and_recorded_inputs() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let first = prepared(DRAWS);
    let revised = prepared(&format!("{DRAWS}{DRAWS_REVISED}"));
    let compiled = PgProgram::new(first.clone());
    assert!(
        matches!(compiled.plan(), InvariantPlan::Compiled),
        "the default route must be the compiled one, so both routes are covered"
    );
    let interpreted = PgProgram::interpreted(first.clone());
    let later = PgProgram::new(revised.clone());

    // Every durable path: a single proposal on each route, a traced one,
    // an atomic batch on each route whose acts draw separately, and a
    // compensation.
    for (note, program) in [("n1", &compiled), ("n2", &interpreted)] {
        expect_committed(
            propose_against_pg(&pool, program, &attested(&act(&first, "open", note)))
                .await
                .unwrap(),
        );
        expect_committed(
            propose_against_pg(&pool, program, &attested(&act(&first, "pair", note)))
                .await
                .unwrap(),
        );
    }
    let PgTracedOutcome::Outcome { outcome, .. } =
        propose_against_pg_with_trace(&pool, &compiled, &attested(&act(&first, "pair", "n1")))
            .await
            .unwrap()
    else {
        panic!("the traced proposal must decide");
    };
    expect_committed(outcome);
    let PgAtomicOutcome::Committed { acts } = propose_all_against_pg(
        &pool,
        &compiled,
        &[
            attested(&act(&first, "open", "n3")),
            attested(&act(&first, "pair", "n3")),
            attested(&act(&first, "pair", "n3")),
        ],
    )
    .await
    .unwrap() else {
        panic!("the batch must commit");
    };
    assert_eq!(acts.len(), 3);
    expect_committed(
        propose_against_pg(&pool, &later, &attested(&act(&revised, "twin", "n3")))
            .await
            .unwrap(),
    );
    let PgAtomicOutcome::Committed { acts } = propose_all_against_pg(
        &pool,
        &interpreted,
        &[
            attested(&act(&first, "open", "n4")),
            attested(&act(&first, "pair", "n4")),
        ],
    )
    .await
    .unwrap() else {
        panic!("the interpreted batch must commit");
    };
    assert_eq!(acts.len(), 2);
    let compensation = CompensationSpec::new(
        revised.clone(),
        "twin".into(),
        Box::new(|_row: &OutboxRow| vec![subj("n4")]),
    )
    .unwrap();
    let processed = process_one_outbox_row(
        &pool,
        "worker",
        "Announce",
        std::time::Duration::from_secs(30),
        &Refuses,
        Some(&compensation),
        Timestamp::now(),
    )
    .await
    .unwrap();
    assert!(
        matches!(processed, ProcessOutcome::Compensated { .. }),
        "the refused delivery must be compensated: {processed:?}"
    );

    let rows = list_audit_rows(&pool).await.unwrap();
    assert_eq!(rows.len(), 12);
    let programmes: HashMap<&str, &PreparedProgram> = [&first, &revised]
        .into_iter()
        .map(|p| (p.model_hash(), p))
        .collect();
    let draws: Vec<usize> = rows
        .iter()
        .map(|r| r.drawn_subjects.as_ref().map_or(usize::MAX, Vec::len))
        .collect();
    assert_eq!(
        draws,
        vec![0, 4, 0, 4, 4, 0, 4, 4, 1, 0, 4, 1],
        "every row records its own act's draws, an empty list when it drew none"
    );

    let mut state = State::default();
    for row in &rows {
        let programme = programmes[row
            .model_hash
            .as_deref()
            .expect("a current row names its programme")];
        assert_eq!(
            row.semantics_version,
            Some(SEMANTICS_VERSION),
            "this runner implements the semantics the row names"
        );
        let drawn = row
            .drawn_subjects
            .clone()
            .expect("a current row records its draws");
        let (outcome, left) = rerun(programme, row, &state, drawn);
        let Ok(Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            candidate_state,
        }) = outcome
        else {
            panic!(
                "row {} must re-run to an acceptance, got {outcome:?}",
                row.transition_id
            );
        };
        assert_eq!(
            asserted_claims, row.asserted_claims,
            "{}",
            row.transition_id
        );
        assert_eq!(
            retracted_claims, row.retracted_claims,
            "{}",
            row.transition_id
        );
        assert_eq!(
            emitted_intents, row.emitted_intents,
            "{}",
            row.transition_id
        );
        assert_eq!(left, 0, "every recorded subject is consumed");
        state.apply(&row.asserted_claims, &row.retracted_claims);
        assert_eq!(candidate_state, state, "{}", row.transition_id);
    }
}

/// Without the exact record a commit does not re-run: two observable draws
/// swapped give another result, a missing one stops the run, and a surplus
/// one is left over.
#[tokio::test]
async fn the_record_is_needed_exactly() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let first = prepared(DRAWS);
    let program = PgProgram::new(first.clone());
    for name in ["open", "pair"] {
        expect_committed(
            propose_against_pg(&pool, &program, &attested(&act(&first, name, "n1")))
                .await
                .unwrap(),
        );
    }
    let rows = list_audit_rows(&pool).await.unwrap();
    let mut pre = State::default();
    pre.apply(&rows[0].asserted_claims, &rows[0].retracted_claims);
    let row = &rows[1];
    let drawn = row.drawn_subjects.clone().unwrap();
    assert_eq!(drawn.len(), 4);
    let recorded = |outcome: &Result<Outcome, EvalError>| {
        matches!(outcome, Ok(Outcome::Accepted { asserted_claims, emitted_intents, .. })
            if *asserted_claims == row.asserted_claims && *emitted_intents == row.emitted_intents)
    };

    let (outcome, _) = rerun(&first, row, &pre, drawn.clone());
    assert!(recorded(&outcome), "the record re-runs: {outcome:?}");

    // `first` and `second` are both admitted, so swapping them shows.
    let mut swapped = drawn.clone();
    swapped.swap(0, 1);
    let (outcome, _) = rerun(&first, row, &pre, swapped);
    assert!(
        !recorded(&outcome),
        "swapped draws must give another result"
    );

    let mut short = drawn.clone();
    short.pop();
    let (outcome, _) = rerun(&first, row, &pre, short);
    assert!(
        matches!(outcome, Err(EvalError::SubjectSourceExhausted)),
        "a missing draw stops the run: {outcome:?}"
    );

    let mut long = drawn.clone();
    long.push(Subject::from("01900000-0000-7000-8000-0000000000ff"));
    let (outcome, left) = rerun(&first, row, &pre, long);
    assert!(recorded(&outcome));
    assert_eq!(
        left, 1,
        "a surplus draw is left over, so a re-run notices it"
    );

    // The draw used only in an intent, and the one never used, are in the
    // record although no admitted claim holds them.
    let admitted: Vec<&EvalValue> = row.asserted_claims.iter().flat_map(|c| &c.args).collect();
    for unseen in &drawn[2..] {
        assert!(
            !admitted.contains(&&EvalValue::Subject(unseen.clone())),
            "{unseen:?} is in no admitted claim"
        );
    }
}
