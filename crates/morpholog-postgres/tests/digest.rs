//! The digest's bytes against PostgreSQL: the kernel's port of
//! `morpholog.value_key_v1` prints what the schema's function prints, over
//! the kernel's own golden, and the hash over those bytes is the one the
//! builtin returns. The second half is the route: a rule over a digest
//! runs interpreted by a typed reason, and both routes decide a matching
//! and a mismatched reveal as the kernel does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{reset_db, test_pool};
use morpholog_core::EvalValue;
use serde::Deserialize;

const GOLDEN: &str = include_str!("../../morpholog-core/tests/golden/digest_v1.json");

#[derive(Deserialize)]
struct Case {
    name: String,
    args: Vec<EvalValue>,
    preimage: String,
    digest: String,
}

/// The schema's own functions render the golden's preimage bytes, and
/// sha256 over them is the golden's digest.
#[tokio::test]
async fn the_kernels_value_key_prints_what_the_schemas_does() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let cases: Vec<Case> = serde_json::from_str(GOLDEN).unwrap();
    assert!(cases.len() > 15);
    for case in cases {
        let args = serde_json::to_value(&case.args).unwrap();
        let (preimage, digest): (String, String) = sqlx::query_as(
            "WITH keys AS (
                 SELECT jsonb_agg(morpholog.value_key_v1(e) ORDER BY o) AS k
                 FROM jsonb_array_elements($1::jsonb) WITH ORDINALITY AS t(e, o)
             )
             SELECT k::text, 'sha256:' || encode(sha256(convert_to(k::text, 'UTF8')), 'hex')
             FROM keys",
        )
        .bind(args)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(preimage, case.preimage, "{}", case.name);
        assert_eq!(digest, case.digest, "{}", case.name);
    }
}

const SEALED: &str = "
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
";

#[derive(Debug, PartialEq, Eq)]
struct Decision {
    verdict: String,
    asserted: Vec<String>,
}

fn claims(cs: &[morpholog_core::ClaimInstance]) -> Vec<String> {
    let mut out: Vec<String> = cs.iter().map(|c| format!("{c:?}")).collect();
    out.sort();
    out
}

async fn by_the_kernel(
    pool: &morpholog_postgres::PgPool,
    program: &morpholog_core::Program,
    t: &morpholog_core::Transformation,
    args: Vec<EvalValue>,
) -> Decision {
    use morpholog_core::{Outcome, State, TracedProposal};
    let state = State::from_claims(morpholog_postgres::list_claims(pool).await.unwrap());
    let transition = morpholog_test_support::test_transition(t, args);
    match morpholog_test_support::propose_with_trace(
        program,
        &transition,
        &state,
        &mut morpholog_test_support::differential::same_subjects(),
    ) {
        TracedProposal::Completed {
            outcome: Outcome::Accepted {
                asserted_claims, ..
            },
            ..
        } => Decision {
            verdict: "committed".into(),
            asserted: claims(&asserted_claims),
        },
        TracedProposal::Completed {
            outcome: Outcome::Rejected { reason },
            ..
        } => Decision {
            verdict: format!("rejected: {reason}"),
            asserted: vec![],
        },
        TracedProposal::Errored { error, .. } => panic!("kernel error: {error}"),
    }
}

async fn by_the_database(
    pool: &morpholog_postgres::PgPool,
    pg: &morpholog_postgres::PgProgram,
    t: &morpholog_core::Transformation,
    args: Vec<EvalValue>,
) -> Decision {
    use morpholog_postgres::{PgProposalOutcome, PgTracedOutcome};
    match common::propose_pg_with_trace_using_test_actor(pool, pg, t, args)
        .await
        .unwrap()
    {
        PgTracedOutcome::Outcome {
            outcome:
                PgProposalOutcome::Committed {
                    asserted_claims, ..
                },
            ..
        } => Decision {
            verdict: "committed".into(),
            asserted: claims(&asserted_claims),
        },
        PgTracedOutcome::Outcome {
            outcome: PgProposalOutcome::Rejected { reason, .. },
            ..
        } => Decision {
            verdict: format!("rejected: {reason}"),
            asserted: vec![],
        },
        PgTracedOutcome::KernelErrored { error, .. } => panic!("kernel error: {error}"),
    }
}

/// The rule is kept off the compiled route by the digest, and both
/// routes decide a matching and a mismatched reveal as the kernel does.
#[tokio::test]
async fn a_digest_rule_runs_interpreted_and_decides_as_the_kernel_does() {
    use common::seed_claims;
    use morpholog_core::{ClaimInstance, PreparedProgram};
    use morpholog_postgres::{InvariantPlan, PgProgram};
    use morpholog_test_support::{dec_str, subj};
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(SEALED).unwrap();
    let reveal = program.transformation("reveal").unwrap().clone();
    let compiled = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    match compiled.plan() {
        InvariantPlan::Mixed { refusals, .. } | InvariantPlan::Interpreted { refusals } => {
            let refusal = refusals
                .iter()
                .find(|r| r.invariant.as_str() == "a_revealed_bid_hashes_to_its_commitment")
                .unwrap_or_else(|| panic!("{refusals:?}"));
            assert!(
                refusal.reason.to_string().contains("digest"),
                "{:?}",
                refusal.reason
            );
        }
        InvariantPlan::Compiled => panic!("a digest rule does not compile"),
    }
    let commitment = morpholog_core::digest::digest(&[
        subj("sealed_bid_v1"),
        subj("lot"),
        subj("alice"),
        dec_str("100"),
        subj("s1"),
    ]);
    let sealed = ClaimInstance {
        predicate: "Sealed".into(),
        args: vec![subj("lot"), subj("alice"), subj(&commitment)],
    };
    for (route, pg) in [
        ("compiled", compiled),
        (
            "interpreted",
            PgProgram::interpreted(PreparedProgram::new(program.clone()).unwrap()),
        ),
    ] {
        for (case, amount, verdict) in [
            ("matching", "100.0", "committed"),
            ("mismatched", "101", "rejected"),
        ] {
            reset_db(&pool).await;
            seed_claims(&pool, std::slice::from_ref(&sealed)).await;
            let args = vec![subj("lot"), subj("alice"), dec_str(amount), subj("s1")];
            let expected = by_the_kernel(&pool, &program, &reveal, args.clone()).await;
            assert!(
                expected.verdict.starts_with(verdict),
                "{route}/{case}: the kernel decides {expected:?}"
            );
            let got = by_the_database(&pool, &pg, &reveal, args).await;
            assert_eq!(got, expected, "{route}/{case}");
        }
    }
}
