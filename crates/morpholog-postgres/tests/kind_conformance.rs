//! Every value kind, held to every surface a stored value crosses on
//! PostgreSQL: the tagged wire, the key the database compares by, and the
//! generated view column. The kinds and their samples come from
//! `morpholog_test_support::kinds`, whose exhaustive match a new kind must
//! pass first; a kind no declaration may carry is checked for its refusal
//! instead, never skipped.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::test_pool;
use morpholog_core::{EvalValue, PredicateArgKind};
use morpholog_postgres::{PgPool, PgProposalOutcome, render_views};
use morpholog_test_support::kinds::{every_kind, kind_samples};

const SENTINEL_HASH: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

async fn sql_key(pool: &PgPool, value: &EvalValue) -> serde_json::Value {
    sqlx::query_scalar::<_, serde_json::Value>("SELECT morpholog.value_key_v1($1)")
        .bind(serde_json::to_value(value).unwrap())
        .fetch_one(pool)
        .await
        .expect("the key function is total")
}

/// Every sample of every declarable kind, distinct values and both sides
/// of each equal pair.
fn every_sample() -> Vec<(PredicateArgKind, EvalValue)> {
    let mut out = Vec::new();
    for kind in every_kind() {
        let s = kind_samples(&kind);
        for v in s
            .distinct
            .iter()
            .chain(s.equal_pairs.iter().flat_map(|(a, b)| [a, b]))
        {
            out.push((kind.clone(), v.clone()));
        }
    }
    out
}

/// One predicate per declarable kind, and an act that admits a claim of it.
fn programme() -> morpholog_core::Program {
    let mut source = String::from("program kinds\n");
    for (i, kind) in every_kind().iter().enumerate() {
        if kind.is_declarable() {
            source.push_str(&format!(
                "predicate Held{i}(id: Subject, v: {kind})\n\
                 transformation hold{i}(id, v):\n    admit Held{i}(id, v)\n"
            ));
        }
    }
    let p = morpholog_surface::parse_program(&source).expect("the kinds programme parses");
    p.validate().expect("the kinds programme validates");
    p
}

#[test]
fn every_sample_crosses_the_tagged_wire_unchanged() {
    for (kind, v) in every_sample() {
        let tagged = serde_json::to_value(&v).unwrap();
        let back: EvalValue = serde_json::from_value(tagged.clone())
            .unwrap_or_else(|e| panic!("{kind}: {tagged} reads back: {e}"));
        assert_eq!(back, v, "{kind}: {tagged}");
    }
}

/// The database compares values by `value_key_v1`, so the SQL function
/// must equal the kernel's port of it, and two values must have one key
/// exactly when the kernel holds them equal, across kinds too.
#[tokio::test]
async fn the_database_key_is_equal_exactly_when_the_kernel_says_so() {
    let pool = test_pool().await;
    let samples = every_sample();
    let mut keys = Vec::new();
    for (kind, v) in &samples {
        let sql = sql_key(&pool, v).await;
        let rust = morpholog_core::digest::value_key_v1(v).unwrap();
        assert_eq!(
            rust, sql,
            "{kind}: the kernel's key is the database's for {v:?}"
        );
        keys.push(sql);
    }
    for (i, (ki, a)) in samples.iter().enumerate() {
        for (j, (kj, b)) in samples.iter().enumerate() {
            assert_eq!(
                a == b,
                keys[i] == keys[j],
                "{ki} {a:?} and {kj} {b:?}: equal in the kernel exactly when their keys are"
            );
        }
    }
}

/// Each declarable kind renders a typed view column that holds every
/// sample admitted, and the description names the column's kind.
#[tokio::test]
async fn every_declarable_kind_renders_a_view_column_that_holds_its_values() {
    let pool = test_pool().await;
    morpholog_postgres::testing::reset_database(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("DROP SCHEMA IF EXISTS morpholog_views CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    let p = programme();
    let pg = common::pg_program(p.clone());
    let mut admitted = std::collections::BTreeMap::<usize, i64>::new();
    for (i, kind) in every_kind().iter().enumerate() {
        let samples = kind_samples(kind);
        // A sample of the kind in another unit is for the key test; the
        // declaration refuses it, as it should.
        for (n, v) in samples
            .distinct
            .iter()
            .enumerate()
            .filter(|(_, v)| kind.holds(&v.kind()))
        {
            let act = p.transformation(&format!("hold{i}")).unwrap();
            let outcome = common::propose_pg_with_test_actor(
                &pool,
                &pg,
                act,
                vec![
                    EvalValue::Subject(format!("h{n}").as_str().into()),
                    v.clone(),
                ],
            )
            .await
            .unwrap();
            assert!(
                matches!(outcome, PgProposalOutcome::Committed { .. }),
                "{kind}: {v:?}"
            );
            *admitted.entry(i).or_default() += 1;
        }
    }
    let rendered = render_views(
        &p.validated().unwrap(),
        "morpholog_views",
        SENTINEL_HASH,
        morpholog_postgres::DerivedSource::Cache,
        false,
    )
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(rendered.sql))
        .execute(&pool)
        .await
        .expect("the views apply");
    for (i, kind) in every_kind().iter().enumerate() {
        if !kind.is_declarable() {
            continue;
        }
        let view = rendered
            .surface
            .views
            .iter()
            .find(|v| v.predicate == format!("Held{i}"))
            .unwrap_or_else(|| panic!("{kind}: a view is described"));
        let column = view.columns.iter().find(|c| c.name == "v").unwrap();
        assert_eq!(
            column.kind.as_deref(),
            Some(kind.to_string().as_str()),
            "{kind}"
        );
        let held: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(v) FROM morpholog_views.{}",
            view.view
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            held, admitted[&i],
            "{kind}: every admitted value reads back non-null"
        );
    }
}

/// A kind no declaration may carry is refused on its way into storage,
/// even through a field that takes any kind.
#[tokio::test]
async fn a_value_no_declaration_carries_is_refused_on_the_way_to_storage() {
    let pool = test_pool().await;
    morpholog_postgres::testing::reset_database(&pool)
        .await
        .unwrap();
    let p = programme();
    let pg = common::pg_program(p.clone());
    let any = every_kind()
        .iter()
        .position(|k| *k == PredicateArgKind::Any)
        .unwrap();
    let act = p.transformation(&format!("hold{any}")).unwrap();
    for kind in every_kind() {
        let Some(refused) = kind_samples(&kind).refused else {
            continue;
        };
        let outcome = common::propose_pg_with_test_actor(
            &pool,
            &pg,
            act,
            vec![EvalValue::Subject("r".into()), refused.clone()],
        )
        .await;
        assert!(
            outcome.is_err(),
            "{kind}: {refused:?} is refused, got {outcome:?}"
        );
    }
}
