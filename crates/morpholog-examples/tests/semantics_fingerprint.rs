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
    Term, Transformation, Transition, ValueExpr, fold,
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
        EvalError::UnboundDecisionTime => json!({"error": "unbound_decision_time"}),
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
        EvalError::ClaimKindMismatch {
            predicate,
            field,
            declared,
            found,
        } => json!({
            "error": "claim_kind_mismatch",
            "predicate": predicate,
            "field": field,
            "declared": declared.to_string(),
            "found": found.to_string(),
        }),
        EvalError::ArgumentKindMismatch {
            transformation,
            parameter,
            declared,
            found,
        } => json!({
            "error": "argument_kind_mismatch",
            "transformation": transformation,
            "parameter": parameter,
            "declared": declared.to_string(),
            "found": found.to_string(),
        }),
        EvalError::IntentKindMismatch {
            intent,
            field,
            declared,
            found,
        } => json!({
            "error": "intent_kind_mismatch",
            "intent": intent,
            "field": field,
            "declared": declared.to_string(),
            "found": found.to_string(),
        }),
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

/// The decision time every proposal in the corpus is given.
const DECIDED_AT: &str = "2026-11-06T12:00:00Z";

fn reads_decision_time(t: &Transformation) -> bool {
    let mut reads = false;
    for stmt in &t.body {
        fold::walk_stmt(stmt, &mut |n| {
            reads |= matches!(
                n,
                fold::Node::Slot(Term::DecidedAt)
                    | fold::Node::Value(ValueExpr::Term(Term::DecidedAt))
            );
        });
    }
    reads
}

/// A proposal's question: every input its decision reads.
fn proposal_case(
    program: &Program,
    t: &Transformation,
    args: Vec<EvalValue>,
    state: &State,
) -> (Value, Value) {
    let transition = Transition {
        decided_at: Some(DECIDED_AT.parse().unwrap()),
        ..test_transition(t, args)
    };
    let subjects = subject_input();
    let mut input = json!({
        "propose": morpholog_core::format::canonical_hash(program),
        "transformation": t.name,
        "args": values(&transition.args),
        "actor": transition.actor,
        "state": claims(state),
        "subjects": subjects,
    });
    // In the question only where a body reads it, so the cases that
    // cannot see it ask what they always asked.
    if reads_decision_time(t) {
        input["decided_at"] = json!(DECIDED_AT);
    }
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

    // A conditional statement: the branch taken draws a subject, the
    // other does not, so which branch ran shows in the subjects the
    // decision used.
    let program = morpholog_surface::parse_program(
        "
program branching

predicate Whole(x: Subject)
predicate Part(x: Subject, piece: Subject)
predicate Sent(x: Subject)

transformation send(x):
    if Whole(x):
        admit Sent(x)
    else:
        admit Part(x, new Subject())
        admit Sent(x)
",
    )
    .unwrap();
    let whole = State::from_claims(vec![morpholog_core::ClaimInstance {
        predicate: "Whole".into(),
        args: vec![subj("w")],
    }]);
    let t = program.transformation("send").unwrap();
    let (input, decided) = proposal_case(&program, t, vec![subj("w")], &whole);
    record(out, "fixture/if/then_branch".to_string(), &input, &decided);
    let (input, decided) = proposal_case(&program, t, vec![subj("w")], &State::default());
    record(out, "fixture/if/else_branch".to_string(), &input, &decided);

    // A for over a pattern: one iteration per matching claim in the
    // canonical order of the claims, a subject drawn in each, so the
    // order the claims are visited in shows in the subjects the decision
    // used; and zero matches is zero iterations.
    let program = morpholog_surface::parse_program(
        "
program ranging

predicate Line(line: Subject, order: Subject, quantity: Decimal)
predicate Ticket(ticket: Subject, line: Subject, quantity: Decimal)

transformation ticket(order):
    for Line(line, order, quantity):
        admit Ticket(new Subject(), line, quantity)
",
    )
    .unwrap();
    let line = |l: &str, q: &str| {
        use morpholog_test_support::{dec_str, subj};
        morpholog_core::ClaimInstance {
            predicate: "Line".into(),
            args: vec![subj(l), subj("o"), dec_str(q)],
        }
    };
    let lines = State::from_claims(vec![line("b", "2"), line("a", "1"), line("a", "10")]);
    let t = program.transformation("ticket").unwrap();
    let order = vec![EvalValue::Subject(Subject::from("o"))];
    let (input, decided) = proposal_case(&program, t, order.clone(), &lines);
    record(
        out,
        "fixture/for_each/one_draw_per_claim".to_string(),
        &input,
        &decided,
    );
    let (input, decided) = proposal_case(&program, t, order, &State::default());
    record(
        out,
        "fixture/for_each/no_match".to_string(),
        &input,
        &decided,
    );

    // A figure: one value definition read by a gate, a rule and a
    // derived read, with a nested call and an expression argument. The
    // gate admits within the figure and refuses beyond it; the rule
    // refuses an act that skips the gate; the empty sum takes its seed.
    let program = morpholog_surface::parse_program(
        "
program figures

predicate Stock(item: Subject, quantity: Decimal)
    unique by (item)
predicate Allocation(allocation: Subject, item: Subject, quantity: Decimal)
    unique by (allocation)
predicate Allocated(item: Subject, quantity: Decimal)

define allocated(item) = (sum(q | Allocation(item: item, quantity: q, ..)))

define headroom(item, on_hand) = (on_hand - allocated(item))

invariant fits:
    Stock(item, on_hand) implies allocated(item) <= on_hand

transformation allocate(allocation, item, quantity):
    bind Stock(item, on_hand)
    require fits: quantity <= headroom(item, on_hand * 1)
    admit Allocation(allocation, item, quantity)

transformation force(allocation, item, quantity):
    admit Allocation(allocation, item, quantity)

derived Allocated(item):
    over Stock(item: item, ..)
    value quantity = allocated(item)
",
    )
    .unwrap();
    let claim = |predicate: &str, args: Vec<EvalValue>| morpholog_core::ClaimInstance {
        predicate: predicate.into(),
        args,
    };
    let (stock, reserved) = {
        use morpholog_test_support::{dec, subj};
        (
            claim("Stock", vec![subj("pallet"), dec(10)]),
            claim("Allocation", vec![subj("a1"), subj("pallet"), dec(4)]),
        )
    };
    let empty = State::from_claims(vec![stock.clone()]);
    let partly = State::from_claims(vec![stock, reserved]);
    let allocate = program.transformation("allocate").unwrap();
    let force = program.transformation("force").unwrap();
    let args = |a: &str, q: i64| {
        use morpholog_test_support::{dec, subj};
        vec![subj(a), subj("pallet"), dec(q)]
    };
    for (case, t, args, state) in [
        (
            "fixture/figure/gate_within_empty_sum",
            allocate,
            args("a1", 10),
            &empty,
        ),
        (
            "fixture/figure/gate_within",
            allocate,
            args("a2", 6),
            &partly,
        ),
        (
            "fixture/figure/gate_beyond",
            allocate,
            args("a2", 7),
            &partly,
        ),
        ("fixture/figure/rule_refuses", force, args("a2", 7), &partly),
    ] {
        let (input, decided) = proposal_case(&program, t, args, state);
        record(out, case.to_string(), &input, &decided);
    }
    let input = json!({
        "derived": morpholog_core::format::canonical_hash(&program),
        "predicate": "Allocated",
        "state": claims(&partly),
    });
    let result = validated(&program)
        .enumerate_derived("Allocated", &partly)
        .map(|rows| json!(rows.expect("the programme derives it")));
    record(
        out,
        "fixture/figure/derived_read".to_string(),
        &input,
        &answer(&result),
    );

    // A declared parameter kind: the argument is held to it before the
    // body runs. The right kind reaches the gate and decides; the wrong
    // kind is an error that stages nothing.
    let program = morpholog_surface::parse_program(
        "
program closing

predicate Order(order: Subject)
predicate Closed(order: Subject)

transformation close(order, force: Bool):
    require known: Order(order)
    require force = true or Closed(order)
    admit Closed(order)
",
    )
    .unwrap();
    let open = State::from_claims(vec![morpholog_core::ClaimInstance {
        predicate: "Order".into(),
        args: vec![EvalValue::Subject(Subject::from("o"))],
    }]);
    let t = program.transformation("close").unwrap();
    for (case, flag) in [
        ("fixture/declared_kind/right_kind", EvalValue::Bool(true)),
        (
            "fixture/declared_kind/right_kind_refused",
            EvalValue::Bool(false),
        ),
        (
            "fixture/declared_kind/wrong_kind",
            morpholog_test_support::dec(1),
        ),
    ] {
        let (input, decided) = proposal_case(
            &program,
            t,
            vec![EvalValue::Subject(Subject::from("o")), flag],
            &open,
        );
        record(out, case.to_string(), &input, &decided);
    }

    // A rule over a grid built from a claim's bounds: a full day
    // publishes, a short one is refused at its missing hour, a window
    // beyond the bound is refused by the bound, and an empty window
    // passes the grid rule, since bounded is not non-empty.
    let program = morpholog_surface::parse_program(
        "
program priced

predicate Day(day: Date, starts_at: Timestamp, ends_at: Timestamp)
    unique by (day)
predicate Published(day: Date)
    append only
predicate Price(day: Date, hour: Timestamp, price: Decimal)
    unique by (day, hour)

invariant a_published_day_is_priced:
    Published(day)
    implies Day(day, starts_at, ends_at)
    and ends_at - starts_at no_longer_than duration(PT25H)
    and (forall h in periods(starts_at, ends_at, duration(PT1H)): exists p: Price(day, h, p))

transformation publish(day):
    admit Published(day)
",
    )
    .unwrap();
    let claim = |predicate: &str, args: Vec<EvalValue>| morpholog_core::ClaimInstance {
        predicate: predicate.into(),
        args,
    };
    let (day, price) = {
        use morpholog_test_support::{date, dec, ts};
        (
            |from: &str, until: &str| claim("Day", vec![date("2026-06-01"), ts(from), ts(until)]),
            |hour: &str| claim("Price", vec![date("2026-06-01"), ts(hour), dec(5)]),
        )
    };
    let publish = program.transformation("publish").unwrap();
    let day_arg = || vec![morpholog_test_support::date("2026-06-01")];
    for (case, claims) in [
        (
            "fixture/grid_rule/full_day",
            vec![
                day("2026-06-01T00:00:00Z", "2026-06-01T03:00:00Z"),
                price("2026-06-01T00:00:00Z"),
                price("2026-06-01T01:00:00Z"),
                price("2026-06-01T02:00:00Z"),
            ],
        ),
        (
            "fixture/grid_rule/missing_hour",
            vec![
                day("2026-06-01T00:00:00Z", "2026-06-01T03:00:00Z"),
                price("2026-06-01T00:00:00Z"),
                price("2026-06-01T01:00:00Z"),
            ],
        ),
        (
            "fixture/grid_rule/beyond_the_bound",
            vec![day("2026-06-01T00:00:00Z", "2026-06-03T00:00:00Z")],
        ),
        (
            "fixture/grid_rule/empty_window",
            vec![day("2026-06-01T00:00:00Z", "2026-06-01T00:00:00Z")],
        ),
    ] {
        let (input, decided) =
            proposal_case(&program, publish, day_arg(), &State::from_claims(claims));
        record(out, case.to_string(), &input, &decided);
    }
}

/// A condition that calls itself: a gate follows a tree any number of
/// steps, through a tree with a loop in it too, a rule reads it with one
/// variable for both arguments, and a derived read lists the whole
/// relation.
fn recursion_cases(out: &mut BTreeMap<String, Fingerprints>) {
    use morpholog_test_support::subj;
    let program = morpholog_surface::parse_program(
        "
program variant_tree

predicate Parent(part: Subject, parent: Subject)
predicate Filled(line_part: Subject, item_part: Subject)
predicate Lineage(part: Subject, ancestor: Subject, links: Decimal)

define descends_from(part, ancestor):
    Parent(part, ancestor)
    or (Parent(part, between) and descends_from(between, ancestor))

define fits(item_part, line_part):
    item_part = line_part or descends_from(item_part, line_part)

invariant no_part_descends_from_itself:
    Parent(p, _) implies not descends_from(p, p)

transformation link(part, parent):
    admit Parent(part, parent)

transformation fill(line_part, item_part):
    require same_part: fits(item_part, line_part)
    admit Filled(line_part, item_part)

derived Lineage(part, ancestor):
    over descends_from(part, ancestor)
    value links = 1
",
    )
    .unwrap();
    let parent = |part: &str, of: &str| morpholog_core::ClaimInstance {
        predicate: "Parent".into(),
        args: vec![subj(part), subj(of)],
    };
    let tree = State::from_claims(vec![
        parent("frame", "root"),
        parent("red_frame", "frame"),
        parent("large_red_frame", "red_frame"),
        parent("blue_frame", "frame"),
    ]);
    let fill = program.transformation("fill").unwrap();
    let link = program.transformation("link").unwrap();
    let pair = |a: &str, b: &str| vec![subj(a), subj(b)];
    for (case, t, args) in [
        (
            "fixture/recursion/gate_three_steps",
            fill,
            pair("root", "large_red_frame"),
        ),
        (
            "fixture/recursion/gate_same_part",
            fill,
            pair("frame", "frame"),
        ),
        (
            "fixture/recursion/gate_sibling",
            fill,
            pair("red_frame", "blue_frame"),
        ),
        (
            "fixture/recursion/gate_upwards",
            fill,
            pair("red_frame", "frame"),
        ),
        (
            "fixture/recursion/rule_admits_a_leaf",
            link,
            pair("wheel", "root"),
        ),
        (
            "fixture/recursion/rule_refuses_a_loop",
            link,
            pair("root", "large_red_frame"),
        ),
        (
            "fixture/recursion/rule_refuses_a_self_link",
            link,
            pair("wheel", "wheel"),
        ),
    ] {
        let (input, decided) = proposal_case(&program, t, args, &tree);
        record(out, case.to_string(), &input, &decided);
    }
    let input = json!({
        "derived": morpholog_core::format::canonical_hash(&program),
        "predicate": "Lineage",
        "state": claims(&tree),
    });
    let result = validated(&program)
        .enumerate_derived("Lineage", &tree)
        .map(|rows| json!(rows.expect("the programme derives it")));
    record(
        out,
        "fixture/recursion/derived_read".to_string(),
        &input,
        &answer(&result),
    );
}

/// A reveal held to a commitment: a matching reveal is admitted, so is
/// one at another scale, and a wrong amount or a wrong salt is refused.
fn digest_cases(out: &mut BTreeMap<String, Fingerprints>) {
    let program = morpholog_surface::parse_program(
        "
program sealed

predicate Sealed(lot: Subject, bidder: Subject, commitment: Subject)
    unique by (lot, bidder)
predicate Bid(lot: Subject, bidder: Subject, amount: Decimal, salt: Subject)
    unique by (lot, bidder)

invariant a_revealed_bid_hashes_to_its_commitment:
    Bid(lot, bidder, amount, salt) and Sealed(lot, bidder, commitment)
    implies digest(#sealed_bid_v1, lot, bidder, amount, salt) = commitment

transformation reveal(lot, bidder, amount, salt):
    admit Bid(lot, bidder, amount, salt)
",
    )
    .unwrap();
    use morpholog_test_support::{dec_str, subj};
    let commitment = morpholog_core::digest::digest(&[
        subj("sealed_bid_v1"),
        subj("lot"),
        subj("alice"),
        dec_str("100"),
        subj("s1"),
    ])
    .unwrap();
    let sealed = State::from_claims(vec![morpholog_core::ClaimInstance {
        predicate: "Sealed".into(),
        args: vec![subj("lot"), subj("alice"), subj(&commitment)],
    }]);
    let reveal = program.transformation("reveal").unwrap();
    for (case, amount, salt) in [
        ("fixture/digest/matching", "100", "s1"),
        ("fixture/digest/another_scale", "100.0", "s1"),
        ("fixture/digest/wrong_amount", "101", "s1"),
        ("fixture/digest/wrong_salt", "100", "s2"),
    ] {
        let (input, decided) = proposal_case(
            &program,
            reveal,
            vec![subj("lot"), subj("alice"), dec_str(amount), subj(salt)],
            &sealed,
        );
        record(out, case.to_string(), &input, &decided);
    }
}

/// Values admitted or emitted into fields of another declared kind,
/// beside values that fit. The arguments are tagged, as `propose --args`
/// sends them, so nothing checks them before the body runs.
fn kind_fixtures(out: &mut BTreeMap<String, Fingerprints>) {
    use morpholog_test_support::{claim_instance, coll, date, dec, qty, subj};
    let program = morpholog_surface::parse_program(
        "
program kinds

predicate Manifest(id: Subject, source_name: Date)
predicate Priced(id: Subject, amount: Decimal[USD])
predicate Noted(id: Subject, note: Any)
predicate Tagged(id: Subject, tags: Collection)
predicate Day(id: Subject, on: Date)

intent Pay(account: Subject, amount: Decimal[GBP])

transformation manifest(id, source_name):
    admit Manifest(id, source_name)

transformation price(id, amount):
    admit Priced(id, amount)

transformation note(id, note):
    admit Noted(id, note)

transformation tag(id, tags):
    admit Tagged(id, tags)

transformation pay(account, amount):
    emit Pay(account, amount)

transformation price_then_pay(id, amount, paid):
    admit Priced(id, amount)
    emit Pay(id, paid)

transformation pay_then_manifest(id, paid, source_name):
    emit Pay(id, paid)
    admit Manifest(id, source_name)

transformation days(id, dates):
    for d in dates:
        admit Day(id, d)

transformation drop_manifest(id):
    require Manifest(id, _)
    retract Manifest(id, _)
",
    )
    .unwrap();
    let empty = State::default();
    let misfit = State::from_claims(vec![claim_instance("Manifest", &[subj("m1"), subj("s1")])]);
    let cases: Vec<(&str, &str, Vec<EvalValue>, &State)> = vec![
        (
            "subject_into_date",
            "manifest",
            vec![subj("m1"), subj("s1")],
            &empty,
        ),
        (
            "date_into_date",
            "manifest",
            vec![subj("m1"), date("2026-11-07")],
            &empty,
        ),
        (
            "bare_decimal_into_quantity",
            "price",
            vec![subj("p1"), dec(5)],
            &empty,
        ),
        (
            "quantity_in_another_unit",
            "price",
            vec![subj("p1"), qty("5", "EUR")],
            &empty,
        ),
        (
            "quantity_in_the_unit",
            "price",
            vec![subj("p1"), qty("5", "USD")],
            &empty,
        ),
        (
            "any_takes_a_subject",
            "note",
            vec![subj("n1"), subj("x")],
            &empty,
        ),
        (
            "any_takes_a_decimal",
            "note",
            vec![subj("n1"), dec(7)],
            &empty,
        ),
        (
            "collection_takes_mixed_items",
            "tag",
            vec![subj("t1"), coll(vec![subj("a"), dec(1)])],
            &empty,
        ),
        (
            "intent_of_another_kind",
            "pay",
            vec![subj("a1"), dec(5)],
            &empty,
        ),
        (
            "intent_in_the_unit",
            "pay",
            vec![subj("a1"), qty("5", "GBP")],
            &empty,
        ),
        (
            "fitting_claim_then_misfitting_intent",
            "price_then_pay",
            vec![subj("p1"), qty("5", "USD"), dec(5)],
            &empty,
        ),
        (
            "fitting_intent_then_misfitting_claim",
            "pay_then_manifest",
            vec![subj("m1"), qty("5", "GBP"), subj("s1")],
            &empty,
        ),
        (
            "for_with_a_subject_third",
            "days",
            vec![
                subj("d1"),
                coll(vec![date("2026-11-07"), date("2026-11-08"), subj("x")]),
            ],
            &empty,
        ),
        (
            "retract_of_a_stored_misfit",
            "drop_manifest",
            vec![subj("m1")],
            &misfit,
        ),
    ];
    for (case, transformation, args, state) in cases {
        let t = program.transformation(transformation).unwrap();
        let (input, decided) = proposal_case(&program, t, args, state);
        record(out, format!("fixture/kinds/{case}"), &input, &decided);
    }
}

/// A bounded obligation evaluates nothing outside its cases. The
/// antecedent divides by a figure one row holds as zero; a change to
/// another case is admitted (version 4 scanned that row and errored),
/// a change to that case errors, and a change to a case that fails the
/// rule is refused rather than lost behind the untouched case's error.
fn bounded_fixtures(out: &mut BTreeMap<String, Fingerprints>) {
    use morpholog_test_support::{dec, subj};
    let program = morpholog_surface::parse_program(
        "
program bounded

predicate Row(k: Subject, v: Decimal)
predicate Ok(k: Subject)

invariant rows_are_ok:
    Row(k, v) and (1 / v) > 0 implies Ok(k)

transformation note(k, v):
    admit Row(k, v)

transformation approve(k):
    admit Ok(k)
",
    )
    .unwrap();
    let row = |k: &str, v: i64| morpholog_core::ClaimInstance {
        predicate: "Row".into(),
        args: vec![subj(k), dec(v)],
    };
    let ok = |k: &str| morpholog_core::ClaimInstance {
        predicate: "Ok".into(),
        args: vec![subj(k)],
    };
    let state = State::from_claims(vec![row("zero", 0), row("good", 1), ok("good")]);
    let approve = program.transformation("approve").unwrap();
    let note = program.transformation("note").unwrap();
    for (case, t, args) in [
        ("untouched_error_case_admits", approve, vec![subj("good")]),
        (
            "touched_error_case_errors",
            note,
            vec![subj("other"), dec(0)],
        ),
        (
            "touched_failing_case_is_refused",
            note,
            vec![subj("new"), dec(2)],
        ),
    ] {
        let (input, decided) = proposal_case(&program, t, args, &state);
        record(out, format!("fixture/bounded/{case}"), &input, &decided);
    }

    // A scope that reads the case name before the rule binds it is a
    // fresh binding, so the rule is checked whole: tagging one row makes
    // every untagged-ok row's obligation live, and the untagged row
    // without its Ok is refused (version 4 bounded the check to the
    // tagged row and admitted).
    let program = morpholog_surface::parse_program(
        "
program scoped

predicate Tag(x: Subject, t: Subject)
predicate Row(x: Subject)
predicate Ok(x: Subject)

invariant rows_ok_once_tagged:
    (exists y: Tag(x, y)) and Row(x) implies Ok(x)

transformation tag(x, t):
    admit Tag(x, t)
",
    )
    .unwrap();
    let state = State::from_claims(vec![morpholog_core::ClaimInstance {
        predicate: "Row".into(),
        args: vec![subj("new")],
    }]);
    let tag = program.transformation("tag").unwrap();
    let (input, decided) = proposal_case(&program, tag, vec![subj("old"), subj("t")], &state);
    record(
        out,
        "fixture/bounded/scope_before_binding_checks_whole".to_string(),
        &input,
        &decided,
    );
}

/// Decimal arithmetic at the edge of what a decimal can hold: a product
/// and a rounding whose exact result does not fit, beside one that does.
fn arithmetic_fixtures(out: &mut BTreeMap<String, Fingerprints>) {
    use morpholog_test_support::dec_str;
    let program = morpholog_surface::parse_program(
        "
program arithmetic

predicate Scaled(y: Decimal)
predicate Rounded(y: Decimal)

transformation scale(x, f):
    let y = x * f
    admit Scaled(y)

transformation to_multiple(x, q):
    let y = round(x, q)
    admit Rounded(y)
",
    )
    .unwrap();
    let scale = program.transformation("scale").unwrap();
    let cases = [
        (
            "fixture/arithmetic/exact_product",
            scale,
            vec![dec_str("1.5"), dec_str("2")],
        ),
        (
            "fixture/arithmetic/inexact_product",
            scale,
            vec![dec_str("0.0000000000000000000000000001"), dec_str("0.5")],
        ),
        (
            "fixture/arithmetic/round_to_an_unrepresentable_multiple",
            program.transformation("to_multiple").unwrap(),
            vec![dec_str("79228162514264337593543950334"), dec_str("0.3")],
        ),
    ];
    for (name, t, args) in cases {
        let (input, decided) = proposal_case(&program, t, args, &State::default());
        record(out, name.to_string(), &input, &decided);
    }
}

fn corpus() -> BTreeMap<String, Fingerprints> {
    let mut out = BTreeMap::new();
    for program in morpholog_examples::all_programs() {
        proposals(&program, &mut out);
        reads(&program, &mut out);
    }
    fixtures(&mut out);
    arithmetic_fixtures(&mut out);
    bounded_fixtures(&mut out);
    kind_fixtures(&mut out);
    digest_cases(&mut out);
    recursion_cases(&mut out);
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
