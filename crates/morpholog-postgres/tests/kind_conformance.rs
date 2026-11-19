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

/// How a reader of a view gets a column of this kind back as the value it
/// holds: an SQL expression over the column, and what its text means.
/// Exhaustive, so a new declarable kind must say how it reads back.
fn read_back(kind: &PredicateArgKind) -> (&'static str, fn(&PredicateArgKind, &str) -> EvalValue) {
    match kind {
        PredicateArgKind::Subject => ("v", |_, t| EvalValue::Subject(t.into())),
        PredicateArgKind::Decimal => ("v::text", |_, t| EvalValue::Decimal(t.parse().unwrap())),
        PredicateArgKind::Quantity(_) => ("v::text", |k, t| {
            let PredicateArgKind::Quantity(unit) = k else {
                unreachable!()
            };
            EvalValue::Quantity {
                amount: t.parse().unwrap(),
                unit: unit.clone(),
            }
        }),
        PredicateArgKind::Date => ("v::text", |_, t| EvalValue::Date(t.parse().unwrap())),
        PredicateArgKind::Timestamp => ("to_json(v) #>> '{}'", |_, t| {
            EvalValue::Timestamp(t.parse().unwrap())
        }),
        // A view's interval is in microseconds, which every sample is.
        PredicateArgKind::Duration => ("extract(epoch from v)::text", |_, t| {
            let seconds: rust_decimal::Decimal = t.parse().unwrap();
            let nanos = (seconds * rust_decimal::Decimal::from(1_000_000_000_i64))
                .trunc()
                .to_string()
                .parse::<i64>()
                .unwrap();
            EvalValue::Duration(jiff::SignedDuration::from_nanos(nanos))
        }),
        PredicateArgKind::Bool => ("v::text", |_, t| EvalValue::Bool(t == "true")),
        // A collection's column holds its tagged elements; an `Any` column
        // holds the whole tagged value.
        PredicateArgKind::Collection => ("v::text", |_, t| {
            EvalValue::Collection(serde_json::from_str(t).unwrap())
        }),
        PredicateArgKind::Any => ("v::text", |_, t| serde_json::from_str(t).unwrap()),
        PredicateArgKind::CalendarSpan => ("", |k, _| panic!("{k} is never declared")),
    }
}

/// Each declarable kind renders a typed view column that reads back as
/// the value admitted, by the kernel's equality, for every sample and both
/// sides of each pair written differently; the description names the kind.
#[tokio::test]
async fn every_declarable_kind_renders_a_view_column_that_reads_back_its_values() {
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
    let mut admitted = std::collections::BTreeMap::<usize, Vec<(String, EvalValue)>>::new();
    for (i, kind) in every_kind().iter().enumerate() {
        let samples = kind_samples(kind);
        let values = samples
            .distinct
            .iter()
            .chain(samples.equal_pairs.iter().flat_map(|(a, b)| [a, b]))
            // A sample in another unit is for the key test; the
            // declaration refuses it, as it should.
            .filter(|v| kind.holds(&v.kind()));
        for (n, v) in values.enumerate() {
            let id = format!("h{n}");
            let act = p.transformation(&format!("hold{i}")).unwrap();
            let outcome = common::propose_pg_with_test_actor(
                &pool,
                &pg,
                act,
                vec![EvalValue::Subject(id.as_str().into()), v.clone()],
            )
            .await
            .unwrap();
            assert!(
                matches!(outcome, PgProposalOutcome::Committed { .. }),
                "{kind}: {v:?}"
            );
            admitted.entry(i).or_default().push((id, v.clone()));
        }
    }
    let rendered = render_views(
        &p.validated().unwrap(),
        "morpholog_views",
        SENTINEL_HASH,
        morpholog_postgres::DerivedSource::Cache,
        &Default::default(),
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
        let (expression, decode) = read_back(kind);
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, {expression} FROM morpholog_views.{}",
            view.view
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        let expected = &admitted[&i];
        assert_eq!(
            rows.len(),
            expected.len(),
            "{kind}: one row per admitted claim"
        );
        for (id, value) in expected {
            let text = rows
                .iter()
                .find(|(row, _)| row == id)
                .and_then(|(_, text)| text.clone())
                .unwrap_or_else(|| panic!("{kind}: {id} has a value in the view"));
            assert_eq!(
                &decode(kind, &text),
                value,
                "{kind}: {id} reads back from `{text}`"
            );
        }
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
