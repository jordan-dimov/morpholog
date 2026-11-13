//! The stock allocation example against PostgreSQL: an act that ranges
//! over the claims a pattern matches is decided by the adapter exactly as
//! the kernel decides it over the whole record.
//!
//! The adapter loads only what the read plan asks for, keyed by what the
//! act knows: the allocations on the shipment, and the stock items and
//! lines those allocations name. A plan that loaded too little would
//! visit fewer allocations, take the other branch for one, or miss a
//! rule's case. So the same shipping act, the same cancellation and the
//! same over-allocation are proposed through the database and through
//! the kernel over every claim, and they must agree on the decision, the
//! claims admitted and retracted with the subjects each pass drew, the
//! passes the trace records and the branch each took, and the rule and
//! witness of each refusal. The example's rules read every predicate its
//! loops read, so a second case ranges over a predicate nothing else
//! names: there the loop's own read is the only reason the adapter
//! loads it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::collections::BTreeMap;

use common::{dec, expect_committed, reset_db, seed_claims, subj, test_pool};
use morpholog_core::{
    ClaimInstance, EvalValue, Outcome, PreparedProgram, Program, State, Subject, TraceEntry,
    TracedProposal, Transformation,
};
use morpholog_examples::stock_allocation;
use morpholog_postgres::{PgPool, PgProgram, PgProposalOutcome, PgTracedOutcome, list_claims};
use morpholog_test_support::differential::same_subjects;
use morpholog_test_support::{propose_with_trace, test_transition};

fn routes() -> Vec<(&'static str, PgProgram)> {
    let prepared = || PreparedProgram::new(stock_allocation::program()).unwrap();
    vec![
        ("compiled", PgProgram::new(prepared())),
        ("interpreted", PgProgram::interpreted(prepared())),
    ]
}

/// Two items on the shelf, an order line for eight bolts, and a shipment
/// with an allocation on each item: one takes its item whole, the other
/// takes part of its item.
async fn stocked_and_allocated(pool: &PgPool, pg: &PgProgram) {
    reset_db(pool).await;
    for (t, args) in [
        (stock_allocation::add_part(), vec![subj("bolt")]),
        (
            stock_allocation::receive(),
            vec![subj("pallet"), subj("bolt"), dec(10)],
        ),
        (
            stock_allocation::receive(),
            vec![subj("crate"), subj("bolt"), dec(5)],
        ),
        (
            stock_allocation::place_order(),
            vec![subj("order"), subj("customer")],
        ),
        (
            stock_allocation::add_line(),
            vec![subj("line"), subj("order"), subj("bolt"), dec(8)],
        ),
        (
            stock_allocation::open_shipment(),
            vec![subj("shipment"), subj("order")],
        ),
        (
            stock_allocation::allocate(),
            vec![
                subj("alloc_b"),
                subj("line"),
                subj("shipment"),
                subj("pallet"),
                dec(3),
            ],
        ),
        (
            stock_allocation::allocate(),
            vec![
                subj("alloc_a"),
                subj("line"),
                subj("shipment"),
                subj("crate"),
                dec(5),
            ],
        ),
    ] {
        expect_committed(
            common::propose_pg_with_test_actor(pool, pg, &t, args)
                .await
                .unwrap(),
        );
    }
}

/// What a decision looks like with the subjects it drew named by the
/// order they were drawn in, so two runs that drew different fresh
/// subjects compare equal when they used them the same way.
#[derive(Debug, PartialEq, Eq)]
struct Decision {
    verdict: String,
    asserted: Vec<String>,
    retracted: Vec<String>,
    emitted: Vec<String>,
    /// Per `for` entry, the claim each pass ran for and the branch its
    /// `if` took, if any.
    passes: Vec<Vec<(String, Option<bool>)>>,
}

fn drawn(entries: &[TraceEntry], out: &mut Vec<Subject>) {
    for e in entries {
        match e {
            TraceEntry::LetNewSubject {
                subject: EvalValue::Subject(s),
                ..
            } => out.push(s.clone()),
            TraceEntry::For { iterations, .. } => {
                for it in iterations {
                    drawn(&it.trace, out);
                }
            }
            TraceEntry::ForEach { iterations, .. } => {
                for it in iterations {
                    drawn(&it.trace, out);
                }
            }
            TraceEntry::If { trace, .. } => drawn(trace, out),
            _ => {}
        }
    }
}

fn passes(entries: &[TraceEntry], out: &mut Vec<Vec<(String, Option<bool>)>>) {
    for e in entries {
        match e {
            TraceEntry::ForEach { iterations, .. } => {
                out.push(
                    iterations
                        .iter()
                        .map(|it| {
                            let branch = it.trace.iter().find_map(|e| match e {
                                TraceEntry::If { taken, .. } => Some(*taken),
                                _ => None,
                            });
                            (format!("{:?}", it.claim), branch)
                        })
                        .collect(),
                );
                for it in iterations {
                    passes(&it.trace, out);
                }
            }
            TraceEntry::For { iterations, .. } => {
                for it in iterations {
                    passes(&it.trace, out);
                }
            }
            TraceEntry::If { trace, .. } => passes(trace, out),
            _ => {}
        }
    }
}

fn decision(
    verdict: String,
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
    emitted: &[morpholog_core::IntentInstance],
    trace: &[TraceEntry],
) -> Decision {
    let mut fresh = vec![];
    drawn(trace, &mut fresh);
    let names: BTreeMap<String, String> = fresh
        .iter()
        .enumerate()
        .map(|(n, s)| (s.to_string(), format!("<fresh-{n}>")))
        .collect();
    let name = |v: &EvalValue| match v {
        EvalValue::Subject(s) => names
            .get(s.as_str())
            .cloned()
            .unwrap_or_else(|| s.to_string()),
        other => format!("{other:?}"),
    };
    let claims = |cs: &[ClaimInstance]| {
        let mut out: Vec<String> = cs
            .iter()
            .map(|c| {
                format!(
                    "{}({})",
                    c.predicate,
                    c.args.iter().map(name).collect::<Vec<_>>().join(", ")
                )
            })
            .collect();
        out.sort();
        out
    };
    let mut emitted: Vec<String> = emitted
        .iter()
        .map(|i| {
            format!(
                "{}({})",
                i.name,
                i.args.iter().map(name).collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    emitted.sort();
    let mut ran = vec![];
    passes(trace, &mut ran);
    Decision {
        verdict,
        asserted: claims(asserted),
        retracted: claims(retracted),
        emitted,
        passes: ran,
    }
}

/// The kernel's decision over every claim the database holds.
async fn by_the_kernel(
    pool: &PgPool,
    program: &Program,
    t: &Transformation,
    args: Vec<EvalValue>,
) -> Decision {
    let state = State::from_claims(list_claims(pool).await.unwrap());
    let transition = test_transition(t, args);
    match propose_with_trace(program, &transition, &state, &mut same_subjects()) {
        TracedProposal::Completed {
            outcome:
                Outcome::Accepted {
                    asserted_claims,
                    retracted_claims,
                    emitted_intents,
                    ..
                },
            trace,
        } => decision(
            "committed".into(),
            &asserted_claims,
            &retracted_claims,
            &emitted_intents,
            &trace,
        ),
        TracedProposal::Completed {
            outcome: Outcome::Rejected { reason },
            trace,
        } => {
            let at = |witness: &[morpholog_core::WitnessBinding]| {
                witness
                    .iter()
                    .map(|w| w.var.to_string())
                    .collect::<Vec<_>>()
            };
            let rule = match &reason {
                morpholog_core::RejectionReason::Invariant { name, witness, .. } => {
                    format!("{name} at {:?}", at(witness))
                }
                morpholog_core::RejectionReason::Require { name, witness, .. }
                | morpholog_core::RejectionReason::BindNone { name, witness, .. } => {
                    format!("{} at {:?}", name.as_ref().unwrap(), at(witness))
                }
            };
            decision(format!("rejected by {rule}"), &[], &[], &[], &trace)
        }
        TracedProposal::Errored { error, .. } => panic!("kernel error: {error}"),
    }
}

/// The database's decision on the same act, through `route`.
async fn by_the_database(
    pool: &PgPool,
    pg: &PgProgram,
    t: &Transformation,
    args: Vec<EvalValue>,
) -> Decision {
    match common::propose_pg_with_trace_using_test_actor(pool, pg, t, args)
        .await
        .unwrap()
    {
        PgTracedOutcome::Outcome {
            outcome:
                PgProposalOutcome::Committed {
                    asserted_claims,
                    retracted_claims,
                    emitted_intents,
                    ..
                },
            trace,
        } => decision(
            "committed".into(),
            &asserted_claims,
            &retracted_claims,
            &emitted_intents,
            &trace,
        ),
        PgTracedOutcome::Outcome {
            outcome: PgProposalOutcome::Rejected { rule, witness, .. },
            trace,
        } => decision(
            format!(
                "rejected by {} at {:?}",
                rule.unwrap(),
                witness
                    .iter()
                    .map(|w| w.var.to_string())
                    .collect::<Vec<_>>()
            ),
            &[],
            &[],
            &[],
            &trace,
        ),
        PgTracedOutcome::KernelErrored { error, .. } => panic!("kernel error: {error}"),
    }
}

#[tokio::test]
async fn shipping_cancelling_and_over_allocating_decide_as_the_kernel_does() {
    let pool = test_pool().await;
    for (route, pg) in routes() {
        stocked_and_allocated(&pool, &pg).await;

        // Over-allocating the line: a rule's refusal, with its witness.
        let overflow = vec![subj("alloc_b"), dec(9)];
        let expected = by_the_kernel(
            &pool,
            &stock_allocation::program(),
            &stock_allocation::change_allocation(),
            overflow.clone(),
        )
        .await;
        assert!(
            expected
                .verdict
                .starts_with("rejected by lines_are_not_overallocated"),
            "{route}: {expected:?}"
        );
        let got =
            by_the_database(&pool, &pg, &stock_allocation::change_allocation(), overflow).await;
        assert_eq!(got, expected, "{route}: change_allocation");

        // Shipping: two passes, the crate whole and the pallet split, with
        // the child drawn in the second pass.
        let expected = by_the_kernel(
            &pool,
            &stock_allocation::program(),
            &stock_allocation::ship(),
            vec![subj("shipment")],
        )
        .await;
        assert_eq!(expected.verdict, "committed", "{route}: {expected:?}");
        assert_eq!(expected.passes.len(), 1, "{route}: one loop");
        assert_eq!(
            expected.passes[0]
                .iter()
                .map(|(_, b)| *b)
                .collect::<Vec<_>>(),
            vec![Some(false), Some(true)],
            "{route}: alloc_a ships whole, alloc_b splits"
        );
        assert!(
            expected
                .asserted
                .iter()
                .any(|c| c.starts_with("SplitFrom(<fresh-0>, pallet")),
            "{route}: the child is the one subject drawn: {expected:?}"
        );
        let got = by_the_database(
            &pool,
            &pg,
            &stock_allocation::ship(),
            vec![subj("shipment")],
        )
        .await;
        assert_eq!(got, expected, "{route}: ship");

        // Cancelling after shipping: the gate inside the loop refuses at
        // the line, on both routes.
        let expected = by_the_kernel(
            &pool,
            &stock_allocation::program(),
            &stock_allocation::cancel_order(),
            vec![subj("order"), EvalValue::Bool(false)],
        )
        .await;
        assert!(
            expected
                .verdict
                .starts_with("rejected by nothing_shipped at"),
            "{route}: {expected:?}"
        );
        let got = by_the_database(
            &pool,
            &pg,
            &stock_allocation::cancel_order(),
            vec![subj("order"), EvalValue::Bool(false)],
        )
        .await;
        assert_eq!(got, expected, "{route}: cancel_order");
    }
}

/// A predicate read by the loop alone, with no rule and no gate naming
/// it: the loop's own read is the only reason the adapter loads it.
const RELABEL: &str = "
program relabel

predicate Tag(item: Subject, label: Subject)
predicate Seen(item: Subject, label: Subject, mark: Subject)

transformation relabel(item):
    for Tag(item, label):
        admit Seen(item, label, new Subject())
";

/// A line for a part takes stock of a variant three steps below it. The
/// gate follows the variant tree through a condition that calls itself,
/// so the adapter must load every step of the tree, not the one the
/// item's own part names.
#[tokio::test]
async fn a_gate_that_follows_the_variant_tree_decides_as_the_kernel_does() {
    let pool = test_pool().await;
    for (route, pg) in routes() {
        stocked_and_allocated(&pool, &pg).await;
        let mut setup = vec![(stock_allocation::add_part(), vec![subj("frame")])];
        for (part, of) in [
            ("red_frame", "frame"),
            ("large_red_frame", "red_frame"),
            ("signed_large_red_frame", "large_red_frame"),
            ("blue_frame", "frame"),
        ] {
            setup.push((stock_allocation::add_variant(), vec![subj(part), subj(of)]));
        }
        for part in ["signed_large_red_frame", "blue_frame"] {
            setup.push((
                stock_allocation::receive(),
                vec![subj(&format!("{part}_stock")), subj(part), dec(4)],
            ));
        }
        setup.push((
            stock_allocation::receive(),
            vec![subj("spare"), subj("signed_large_red_frame"), dec(4)],
        ));
        for (line, part) in [("frames", "frame"), ("red_frames", "red_frame")] {
            setup.push((
                stock_allocation::add_line(),
                vec![subj(line), subj("order"), subj(part), dec(4)],
            ));
        }
        for (t, args) in setup {
            expect_committed(
                common::propose_pg_with_test_actor(&pool, &pg, &t, args)
                    .await
                    .unwrap(),
            );
        }

        let allocation = |name: &str, line: &str, item: &str| {
            vec![subj(name), subj(line), subj("shipment"), subj(item), dec(1)]
        };
        for (name, line, item, verdict) in [
            // A sibling's stock is no variant of the red frame.
            (
                "a1",
                "red_frames",
                "blue_frame_stock",
                "rejected by same_part",
            ),
            // A bolt is outside the tree.
            ("a2", "frames", "pallet", "rejected by same_part"),
            // Three steps down from the red frame, four from the frame.
            (
                "a3",
                "red_frames",
                "signed_large_red_frame_stock",
                "committed",
            ),
        ] {
            let args = allocation(name, line, item);
            let expected = by_the_kernel(
                &pool,
                &stock_allocation::program(),
                &stock_allocation::allocate(),
                args.clone(),
            )
            .await;
            assert!(
                expected.verdict.starts_with(verdict),
                "{route}: {expected:?}"
            );
            let got = by_the_database(&pool, &pg, &stock_allocation::allocate(), args).await;
            assert_eq!(got, expected, "{route}: allocate {item} to {line}");
        }

        // The same gate without a trace. A traced proposal is decided
        // over a wider load, which would hide a tree loaded one step
        // deep; this one loads only what the act's own reads ask for.
        expect_committed(
            common::propose_pg_with_test_actor(
                &pool,
                &pg,
                &stock_allocation::allocate(),
                allocation("a4", "frames", "spare"),
            )
            .await
            .unwrap(),
        );
    }
}

#[tokio::test]
async fn a_predicate_only_the_loop_reads_is_loaded_for_it() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(RELABEL).unwrap();
    let relabel = program.transformation("relabel").unwrap().clone();
    let tag = |item: &str, label: &str| ClaimInstance {
        predicate: "Tag".into(),
        args: vec![subj(item), subj(label)],
    };
    for (route, pg) in [
        (
            "compiled",
            PgProgram::new(PreparedProgram::new(program.clone()).unwrap()),
        ),
        (
            "interpreted",
            PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap()),
        ),
    ] {
        reset_db(&pool).await;
        seed_claims(&pool, &[tag("x", "b"), tag("x", "a"), tag("y", "c")]).await;
        let expected = by_the_kernel(&pool, &program, &relabel, vec![subj("x")]).await;
        assert_eq!(expected.passes.len(), 1, "{route}: {expected:?}");
        assert_eq!(
            expected.passes[0].len(),
            2,
            "{route}: two tags on x: {expected:?}"
        );
        assert_eq!(
            expected.asserted,
            vec![
                "Seen(x, a, <fresh-0>)".to_string(),
                "Seen(x, b, <fresh-1>)".to_string()
            ],
            "{route}"
        );
        let got = by_the_database(&pool, &pg, &relabel, vec![subj("x")]).await;
        assert_eq!(got, expected, "{route}: relabel");
    }
}

/// One batch receives stock of a variant three steps below the line's
/// part, then allocates it. The second act's walk starts from a part it
/// reads off a stock item the first act admitted, which the snapshot
/// does not hold, so the walk must not be keyed from the snapshot.
#[tokio::test]
async fn an_allocation_of_stock_received_earlier_in_the_batch_follows_the_tree() {
    use morpholog_core::Transition;
    use morpholog_postgres::{PgAtomicOutcome, Proposal, propose_all_against_pg};
    let pool = test_pool().await;
    for (route, pg) in routes() {
        stocked_and_allocated(&pool, &pg).await;
        let mut setup = vec![(stock_allocation::add_part(), vec![subj("frame")])];
        for (part, of) in [
            ("red_frame", "frame"),
            ("large_red_frame", "red_frame"),
            ("signed_large_red_frame", "large_red_frame"),
        ] {
            setup.push((stock_allocation::add_variant(), vec![subj(part), subj(of)]));
        }
        setup.push((
            stock_allocation::add_line(),
            vec![subj("frames"), subj("order"), subj("frame"), dec(4)],
        ));
        for (t, args) in setup {
            expect_committed(
                common::propose_pg_with_test_actor(&pool, &pg, &t, args)
                    .await
                    .unwrap(),
            );
        }
        let act = |t: &morpholog_core::Transformation, args: Vec<EvalValue>| {
            Proposal::gateway(&Transition {
                transformation_name: t.name.clone(),
                args,
                actor: morpholog_test_support::test_actor(),
                decided_at: None,
            })
        };
        let outcome = propose_all_against_pg(
            &pool,
            &pg,
            &[
                act(
                    &stock_allocation::receive(),
                    vec![subj("fresh"), subj("signed_large_red_frame"), dec(4)],
                ),
                act(
                    &stock_allocation::allocate(),
                    vec![
                        subj("a_fresh"),
                        subj("frames"),
                        subj("shipment"),
                        subj("fresh"),
                        dec(1),
                    ],
                ),
            ],
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, PgAtomicOutcome::Committed { .. }),
            "{route}: {outcome:?}"
        );
    }
}
