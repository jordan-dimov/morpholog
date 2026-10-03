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
//! Results are encoded by meaning, not by `Debug` or `Display`: error prose
//! can change freely, an error's kind and the names and values it carries
//! cannot. This catches a change the corpus can see; it does not prove
//! nothing else changed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use morpholog_core::{
    EvalError, EvalValue, Outcome, Program, RejectionReason, SEMANTICS_VERSION, State,
};
use morpholog_test_support::differential::{same_subjects, sample_args, sample_state};
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

/// What an error means: its kind, and the names and values it carries.
/// Prose payloads are left out, so rewording a message changes nothing.
fn error(e: &EvalError) -> Value {
    match e {
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
            RejectionReason::Invariant {
                name,
                version,
                witness,
            } => json!({"invariant": {"name": name, "version": version, "witness": witness}}),
            // The rendered gate stands for the statement that refused.
            RejectionReason::Require { name, rendered } => {
                json!({"require": {"name": name, "gate": rendered}})
            }
            RejectionReason::BindNone { name, rendered } => {
                json!({"bind": {"name": name, "gate": rendered}})
            }
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

fn proposals(program: &Program, out: &mut BTreeMap<String, Fingerprints>) {
    for t in &program.transformations {
        for salt in 0..3u64 {
            let Some(args) = sample_args(program, t, salt) else {
                continue;
            };
            let state = sample_state(program, 2, salt);
            let transition = test_transition(t, args);
            let input = json!({
                "propose": morpholog_core::format::canonical_hash(program),
                "transformation": t.name,
                "args": values(&transition.args),
                "actor": transition.actor,
                "state": claims(&state),
            });
            let result = propose(program, &transition, &state, &mut same_subjects());
            out.insert(
                format!("gallery/{}/propose/{}/salt={salt}", program.name, t.name),
                (digest(&input), digest(&decision(&result))),
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
            out.insert(
                format!(
                    "gallery/{}/invariant/{}/salt={salt}",
                    program.name, inv.name
                ),
                (digest(&input), digest(&answer(&result))),
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
            out.insert(
                format!(
                    "gallery/{}/derived/{}/salt={salt}",
                    program.name, d.predicate
                ),
                (digest(&input), digest(&answer(&result))),
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
    let transition = test_transition(t, vec![dec(10), dec(0)]);
    let input = json!({
        "propose": morpholog_core::format::canonical_hash(&program),
        "transformation": t.name,
        "args": values(&transition.args),
    });
    let result = propose(
        &program,
        &transition,
        &State::default(),
        &mut same_subjects(),
    );
    out.insert(
        "fixture/division_by_zero".to_string(),
        (digest(&input), digest(&decision(&result))),
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
