//! What each version of the kernel's semantics decides, pinned over a fixed
//! corpus, so a change to what Morpholog means cannot land without moving
//! [`SEMANTICS_VERSION`].
//!
//! Every case has a stable name, a fingerprint of its inputs (the question)
//! and a fingerprint of its result (the answer). Against the golden for the
//! current version:
//!
//! - the same question with a different answer is a change of semantics,
//!   and fails until the version moves;
//! - a case added, removed or asked differently is a change of corpus, and
//!   fails until the golden is regenerated (`UPDATE_GOLDENS=1`), so lost
//!   coverage shows in review;
//! - a new version starts its own golden, and keeps the old ones as the
//!   record of what each version decided.
//!
//! Results are encoded by meaning, not by `Debug` or `Display`. A refusal
//! counts by its kind and stable identity (an invariant's name, version and
//! witness; a gate's kind and rule name, when it has one), never by how the
//! refusing expression renders. An error counts by its typed kind and its
//! structured payload (a predicate, a value); free-form diagnostic strings
//! do not. Every input a decision reads is in its question, the subjects
//! `new Subject()` draws included. This catches a change the corpus can
//! see; it does not prove nothing else changed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use morpholog_core::{
    EvalError, EvalValue, Outcome, Program, RejectionReason, SEMANTICS_VERSION, State, Subject,
    Transformation,
};
use morpholog_test_support::differential::{sample_args, sample_state};
use morpholog_test_support::{propose, test_transition, validated};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn digest(v: &Value) -> String {
    let bytes = Sha256::digest(v.to_string().as_bytes());
    "sha256:".to_string() + &bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
}

fn values(vs: &[EvalValue]) -> Value {
    serde_json::to_value(vs).unwrap()
}

fn claims(state: &State) -> Value {
    serde_json::to_value(state.claims().iter().collect::<Vec<_>>()).unwrap()
}

/// The subjects every proposal in the corpus draws from, in order: a finite
/// transcript, as a replay will hand over, comfortably longer than any case
/// consumes.
fn subject_input() -> Vec<Subject> {
    (0..1024)
        .map(|n| Subject::from(format!("semantics-{n}")))
        .collect()
}

/// What an error means: its typed kind and its structured payload. Free-form
/// diagnostic strings are left out, so rewording one changes nothing.
fn error(e: &EvalError) -> Value {
    match e {
        // Not always a name: an unbound derived key carries a sentence here.
        EvalError::UnboundVariable(_) => json!({"error": "unbound_variable"}),
        EvalError::TypeMismatch(_) => json!({"error": "type_mismatch"}),
        EvalError::ValueOfZeroMatches(predicate) => {
            json!({"error": "value_of_zero_matches", "predicate": predicate})
        }
        EvalError::ValueOfMultipleMatches(predicate) => {
            json!({"error": "value_of_multiple_matches", "predicate": predicate})
        }
        EvalError::UnboundActor => json!({"error": "unbound_actor"}),
        EvalError::PreStateUnavailable => json!({"error": "pre_state_unavailable"}),
        EvalError::SubjectSourceExhausted => json!({"error": "subject_source_exhausted"}),
        EvalError::DivisionByZero => json!({"error": "division_by_zero"}),
        EvalError::RoundQuantumNotPositive(quantum) => {
            json!({"error": "round_quantum_not_positive", "quantum": quantum})
        }
        EvalError::PeriodSpanNotPositive { builtin, span } => {
            json!({"error": "period_span_not_positive", "builtin": builtin, "span": span})
        }
        EvalError::PeriodIndexNotWhole(index) => {
            json!({"error": "period_index_not_whole", "index": index})
        }
        EvalError::RoundOutOfRange { value, quantum } => {
            json!({"error": "round_out_of_range", "value": value, "quantum": quantum})
        }
        EvalError::ArithOutOfRange(_) => json!({"error": "arith_out_of_range"}),
        EvalError::EmptyExtremum { op, body: _ } => json!({"error": "empty_extremum", "op": op}),
        EvalError::UnknownDefinition(name) => json!({"error": "unknown_definition", "name": name}),
    }
}

/// What a proposal decided.
fn decision(result: &Result<Outcome, EvalError>) -> Value {
    match result {
        Ok(Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            candidate_state: _,
        }) => json!({
            "accepted": {
                "asserted": asserted_claims,
                "retracted": retracted_claims,
                "emitted": emitted_intents,
            }
        }),
        Ok(Outcome::Rejected { reason }) => match reason {
            // The comparison a refusal blames is diagnosis, as a gate's
            // witness is; neither is part of the decision's identity.
            RejectionReason::Invariant {
                name,
                version,
                witness,
                compared: _,
            } => json!({"invariant": {"name": name, "version": version, "witness": witness}}),
            // A gate's stable identity is its rule name; how the refusing
            // expression renders is diagnosis, not meaning.
            RejectionReason::Require { name, .. } => json!({"require": {"rule": name}}),
            RejectionReason::BindNone { name, .. } => json!({"bind": {"rule": name}}),
        },
        Err(e) => error(e),
    }
}

/// What a read answered: its result, or the error.
fn answer(result: &Result<Value, EvalError>) -> Value {
    match result {
        Ok(value) => json!({"result": value}),
        Err(e) => error(e),
    }
}

/// One case: its inputs and its result, each fingerprinted.
type Fingerprints = (String, String);

fn record(out: &mut BTreeMap<String, Fingerprints>, case: String, input: &Value, result: &Value) {
    let fingerprints = (digest(input), digest(result));
    assert!(
        out.insert(case.clone(), fingerprints).is_none(),
        "duplicate semantics case id: {case}"
    );
}

/// A proposal's question: every input its decision reads.
fn proposal_case(
    program: &Program,
    t: &Transformation,
    args: Vec<EvalValue>,
    state: &State,
) -> (Value, Value) {
    let transition = test_transition(t, args);
    let subjects = subject_input();
    let input = json!({
        "propose": morpholog_core::format::canonical_hash(program),
        "transformation": t.name,
        "args": values(&transition.args),
        "actor": transition.actor,
        "state": claims(state),
        "subjects": subjects,
    });
    let result = propose(program, &transition, state, &mut subjects.into_iter());
    (input, decision(&result))
}

fn proposals(program: &Program, out: &mut BTreeMap<String, Fingerprints>) {
    for t in &program.transformations {
        for salt in 0..3u64 {
            // Every transformation is asked: one the generator cannot
            // give arguments to would otherwise leave the corpus unseen.
            let args = sample_args(program, t, salt).unwrap_or_else(|| {
                panic!(
                    "no sampled arguments for `{}::{}`: extend the generator so the \
                     corpus covers it",
                    program.name, t.name
                )
            });
            let (input, decided) = proposal_case(program, t, args, &sample_state(program, 2, salt));
            record(
                out,
                format!("gallery/{}/propose/{}/salt={salt}", program.name, t.name),
                &input,
                &decided,
            );
        }
    }
}

fn reads(program: &Program, out: &mut BTreeMap<String, Fingerprints>) {
    let v = validated(program);
    for salt in 0..3u64 {
        let state = sample_state(program, 2, salt);
        let pre = sample_state(program, 2, salt + 3);
        for inv in &program.invariants {
            let input = json!({
                "invariant": morpholog_core::format::canonical_hash(program),
                "name": inv.name,
                "state": claims(&state),
                "pre": claims(&pre),
            });
            let result = v
                .eval_invariant(inv.name.as_str(), &state, Some(&pre))
                .map(|held| json!(held.expect("the programme declares it")));
            record(
                out,
                format!(
                    "gallery/{}/invariant/{}/salt={salt}",
                    program.name, inv.name
                ),
                &input,
                &answer(&result),
            );
        }
        for d in &program.derived_claims {
            let input = json!({
                "derived": morpholog_core::format::canonical_hash(program),
                "predicate": d.predicate,
                "state": claims(&state),
            });
            let result = v
                .enumerate_derived(d.predicate.as_str(), &state)
                .map(|rows| json!(rows.expect("the programme derives it")));
            record(
                out,
                format!(
                    "gallery/{}/derived/{}/salt={salt}",
                    program.name, d.predicate
                ),
                &input,
                &answer(&result),
            );
        }
    }
}

/// What the gallery's sampled inputs do not reach: a kernel error from a
/// programme that validates.
fn fixtures(out: &mut BTreeMap<String, Fingerprints>) {
    use morpholog_test_support::dec;
    let program = morpholog_surface::parse_program(
        "
program fixtures

transformation share(amount, parts):
    require amount / parts > 0
",
    )
    .unwrap();
    let t = program.transformation("share").unwrap();
    let (input, decided) = proposal_case(&program, t, vec![dec(10), dec(0)], &State::default());
    record(
        out,
        "fixture/division_by_zero".to_string(),
        &input,
        &decided,
    );

    // A consequent `or` over a history that already violates it: a
    // change to the valid case `a` is admitted, a change to the
    // violated case `b` is still refused. Which cases admission checks
    // is what this pair pins.
    use morpholog_test_support::subj;
    let program = morpholog_surface::parse_program(
        "
program either_way

predicate A(x: Subject, k: Subject)
predicate B(x: Subject)
predicate C(x: Subject)

invariant either_record:
    A(x, _) implies (B(x) or C(x))

transformation note(x, k):
    admit A(x, k)

transformation mark_b(x):
    admit B(x)
",
    )
    .unwrap();
    let dirty = State::from_claims(vec![
        morpholog_core::ClaimInstance {
            predicate: "A".into(),
            args: vec![subj("a"), subj("k1")],
        },
        morpholog_core::ClaimInstance {
            predicate: "A".into(),
            args: vec![subj("b"), subj("k1")],
        },
        morpholog_core::ClaimInstance {
            predicate: "C".into(),
            args: vec![subj("a")],
        },
    ]);
    let t = program.transformation("mark_b").unwrap();
    let (input, decided) = proposal_case(&program, t, vec![subj("a")], &dirty);
    record(
        out,
        "fixture/consequent_or/untouched_case_admits".to_string(),
        &input,
        &decided,
    );
    let t = program.transformation("note").unwrap();
    let (input, decided) = proposal_case(&program, t, vec![subj("b"), subj("k2")], &dirty);
    record(
        out,
        "fixture/consequent_or/touched_case_is_judged".to_string(),
        &input,
        &decided,
    );
}

fn corpus() -> BTreeMap<String, Fingerprints> {
    let mut out = BTreeMap::new();
    for program in morpholog_examples::all_programs() {
        proposals(&program, &mut out);
        reads(&program, &mut out);
    }
    fixtures(&mut out);
    out
}

fn golden_path(version: u32) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/semantics")
        .join(format!("v{version}.ndjson"))
}

fn read_golden(version: u32) -> Option<BTreeMap<String, Fingerprints>> {
    let text = std::fs::read_to_string(golden_path(version)).ok()?;
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next()?).unwrap();
    assert_eq!(
        header,
        json!({"semantics_version": version}),
        "golden header"
    );
    Some(
        lines
            .map(|line| {
                let row: Value = serde_json::from_str(line).unwrap();
                (
                    row["case"].as_str().unwrap().to_string(),
                    (
                        row["input"].as_str().unwrap().to_string(),
                        row["decision"].as_str().unwrap().to_string(),
                    ),
                )
            })
            .collect(),
    )
}

fn write_golden(version: u32, cases: &BTreeMap<String, Fingerprints>) {
    let mut text = json!({"semantics_version": version}).to_string() + "\n";
    for (case, (input, decision)) in cases {
        text += &(json!({"case": case, "input": input, "decision": decision}).to_string() + "\n");
    }
    let path = golden_path(version);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Cases whose question is unchanged but whose answer moved.
fn drifted(
    golden: &BTreeMap<String, Fingerprints>,
    now: &BTreeMap<String, Fingerprints>,
) -> Vec<String> {
    now.iter()
        .filter(|(case, (input, decision))| {
            golden
                .get(*case)
                .is_some_and(|(was_input, was)| was_input == input && was != decision)
        })
        .map(|(case, _)| case.clone())
        .collect()
}

#[test]
fn the_semantics_version_names_what_the_corpus_decides() {
    let now = corpus();
    let golden = read_golden(SEMANTICS_VERSION);
    if let Some(golden) = &golden {
        let drift = drifted(golden, &now);
        assert!(
            drift.is_empty(),
            "the same question now has a different answer under semantics version \
             {SEMANTICS_VERSION}, which changes what Morpholog means: move SEMANTICS_VERSION, \
             record the change in runtime-semantics.md, and generate the new version's golden \
             with UPDATE_GOLDENS=1. Changed: {drift:?}"
        );
    }
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        write_golden(SEMANTICS_VERSION, &now);
        return;
    }
    let golden = golden.unwrap_or_else(|| {
        panic!(
            "no golden for semantics version {SEMANTICS_VERSION}; generate it with \
             UPDATE_GOLDENS=1"
        )
    });
    let added: Vec<_> = now.keys().filter(|c| !golden.contains_key(*c)).collect();
    let removed: Vec<_> = golden.keys().filter(|c| !now.contains_key(*c)).collect();
    let reasked: Vec<_> = now
        .iter()
        .filter(|(c, (input, _))| golden.get(*c).is_some_and(|(was, _)| was != input))
        .map(|(c, _)| c)
        .collect();
    assert!(
        added.is_empty() && removed.is_empty() && reasked.is_empty(),
        "the corpus changed, not the semantics: regenerate with UPDATE_GOLDENS=1 and review \
         the diff. Added: {added:?}; removed: {removed:?}; asked differently: {reasked:?}"
    );
}
