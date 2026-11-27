//! A deployment the v0.0.21 release provisioned, upgraded. That release
//! recorded one position per index and gave a `unique by` key of several
//! fields an index per field. Its own binary, downloaded and held to its
//! published digest, provisions two programmes; this binary migrates the
//! database and provisions them again. An index still needed is the one
//! the release built, by OID; the key index is built; only what no
//! programme needs goes stale, and a prune drops that and nothing else.
//!
//! Attacker capability: none. An operator with DDL on the claims table
//! builds an index of their own, and a concurrent build is interrupted.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use sqlx::PgPool;

const RELEASE: &str = "v0.0.21";

/// The release's assets, as published: architecture, OS, target triple
/// and the archive's sha256.
const ASSETS: &[(&str, &str, &str, &str)] = &[
    (
        "x86_64",
        "linux",
        "x86_64-unknown-linux-musl",
        "92fb1314fdbc406c09a7a8f0e94c7d443cf2a66014155ad715227bc571e307f8",
    ),
    (
        "aarch64",
        "linux",
        "aarch64-unknown-linux-musl",
        "4d59d9af9e1735a6223d8820136629509e835091d22f61b036c4153f89f7cb42",
    ),
    (
        "aarch64",
        "macos",
        "aarch64-apple-darwin",
        "b33d92d41e644008605b9ca44cc2cf8de5898db7292827174262ebb1f9c7e856",
    ),
];

const PRICE_CURVE: &str = "program price_curve
predicate PriceFile(file: Subject)
predicate Price(file: Subject, date: Date, slot: Decimal, minute: Decimal, price: Decimal)
    unique by (file, date, slot)
transformation open_file(file):
    require file_new: not PriceFile(file)
    admit PriceFile(file)
transformation set_price(file, date: Date, slot: Decimal, minute: Decimal, price: Decimal):
    require file_open: PriceFile(file)
    admit Price(file, date, slot, minute, price)
";

/// Another programme on the same database, which reads prices by slot.
const SLOT_REPORT: &str = "program slot_report
predicate Price(file: Subject, date: Date, slot: Decimal, minute: Decimal, price: Decimal)
predicate Flagged(slot: Decimal)
transformation flag_slot(slot: Decimal):
    require priced: Price(_, _, slot, _, _)
    admit Flagged(slot)
";

/// The names v0.0.21 gave the indexes it built for the two programmes.
const PRICE_FILE: &str = "morpholog_ci_price_0_vk1_1f586f9bba64";
const PRICE_DATE: &str = "morpholog_ci_price_1_vk1_79dffb343ef9";
const PRICE_SLOT: &str = "morpholog_ci_price_2_vk1_b61cbdbf45af";
const PRICE_MINUTE: &str = "morpholog_ci_price_3_vk1_88e8b21a18a0";
const PRICE_PRICE: &str = "morpholog_ci_price_4_vk1_da052733c5d1";
const PRICEFILE: &str = "morpholog_ci_pricefile_0_vk1_a1bab0265591";
const FLAGGED: &str = "morpholog_ci_flagged_0_vk1_490ba52bea33";

fn run(binary: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .args(args)
        .output()
        .expect("spawn morpholog")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// A database the release initialised and provisioned for both
/// programmes, then migrated by this binary: the state an upgrade starts
/// `provision indexes` from. With the release out of reach, none.
struct Released {
    admin: PgPool,
    name: String,
    url: String,
    price: common::Fixture,
    slot: common::Fixture,
    /// The release's indexes on the claims table, by name.
    built: BTreeMap<String, u32>,
}

impl Released {
    async fn new(label: &str) -> Option<Self> {
        let old = common::release_binary(RELEASE, ASSETS)?;
        let base = morpholog_postgres::with_connection_defaults(&common::database_url());
        let name = format!("morpholog_cli_v0021_{label}_{}", std::process::id());
        let admin = PgPool::connect(&common::with_database(&base, "postgres"))
            .await
            .unwrap();
        for sql in [
            format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
            format!("CREATE DATABASE {name}"),
        ] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&admin)
                .await
                .unwrap();
        }
        let url = common::with_database(&base, &name);
        let price = common::write_fixture("price_curve", PRICE_CURVE);
        let slot = common::write_fixture("slot_report", SLOT_REPORT);
        let init = run(&old, &["init", "--database-url", &url]);
        assert!(init.status.success(), "{init:?}");
        let provisioned = json(&run(
            &old,
            &[
                "provision",
                "indexes",
                "--json",
                "--database-url",
                &url,
                price.to_str().unwrap(),
                slot.to_str().unwrap(),
            ],
        ));
        assert_eq!(provisioned["applied"], true, "{provisioned}");
        let built = indexes(&url).await;
        assert_eq!(
            built.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                FLAGGED,
                PRICE_FILE,
                PRICE_DATE,
                PRICE_SLOT,
                PRICE_MINUTE,
                PRICE_PRICE,
                PRICEFILE
            ],
            "what the release built"
        );
        let migrated = run(
            Path::new(common::bin()),
            &["migrate", "--database-url", &url],
        );
        assert!(migrated.status.success(), "{migrated:?}");
        Some(Self {
            admin,
            name,
            url,
            price,
            slot,
            built,
        })
    }

    fn provision(&self, flags: &[&str], files: &[&Path]) -> Value {
        let mut args = vec![
            "provision",
            "indexes",
            "--json",
            "--database-url",
            &self.url,
        ];
        args.extend_from_slice(flags);
        args.extend(files.iter().map(|f| f.to_str().unwrap()));
        json(&run(Path::new(common::bin()), &args))
    }

    /// `provision indexes --check` for both programmes.
    fn check(&self) -> Output {
        run(
            Path::new(common::bin()),
            &[
                "provision",
                "indexes",
                "--check",
                "--database-url",
                &self.url,
                self.price.to_str().unwrap(),
                self.slot.to_str().unwrap(),
            ],
        )
    }

    fn both(&self) -> [&Path; 2] {
        [&self.price, &self.slot]
    }

    async fn drop(self) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await
        .unwrap();
    }
}

/// Morpholog's indexes on the claims table and the operator's (`ops_`),
/// by name, with OIDs: a dropped and rebuilt index keeps its name and not
/// its OID.
async fn indexes(url: &str) -> BTreeMap<String, u32> {
    let pool = PgPool::connect(url).await.unwrap();
    let rows: Vec<(String, sqlx::postgres::types::Oid)> = sqlx::query_as(
        "SELECT c.relname::text, c.oid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'morpholog.claims'::regclass
           AND (c.relname LIKE 'morpholog\\_ci\\_%' OR c.relname LIKE 'ops\\_%')",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    rows.into_iter().map(|(name, oid)| (name, oid.0)).collect()
}

/// Each index entry's action, name and positions, in the report's order.
fn entries(report: &Value) -> Vec<(String, String, Vec<u64>)> {
    report["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap().to_string(),
                e["name"].as_str().unwrap().to_string(),
                e["positions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p.as_u64().unwrap())
                    .collect(),
            )
        })
        .collect()
}

/// The names a report lists under `field`.
fn names(report: &Value, field: &str) -> Vec<String> {
    report[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect()
}

/// Every stale index and statistics object a report lists, sorted: what a
/// run that applied under `--prune` dropped.
fn stale(report: &Value) -> Vec<String> {
    let mut out: Vec<String> = ["indexes", "statistics"]
        .iter()
        .flat_map(|field| report[*field].as_array().unwrap())
        .filter(|e| e["action"] == "stale")
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

/// The upgrade itself, with the other programme left out of the call as
/// a deployment that provisions one programme at a time does. The plan
/// keeps what is still needed and builds the key index before anything
/// goes; the prune drops the three field indexes only the old check
/// needed, and the statistics of the two positions nothing seeks any
/// more. The index another programme recorded stays, required elsewhere.
#[tokio::test(flavor = "current_thread")]
async fn the_release_s_indexes_are_kept_where_needed_and_the_key_index_is_built() {
    let Some(db) = Released::new("upgrade").await else {
        return;
    };
    let price: &Path = &db.price;
    let plan = db.provision(&["--dry-run", "--prune"], &[price]);
    let key = entries(&plan)
        .into_iter()
        .find(|(_, _, positions)| positions == &[0, 1, 2])
        .map(|(_, name, _)| name)
        .expect("a key index");
    let stated = |report: &Value| -> Vec<(String, String)> {
        entries(report)
            .into_iter()
            .map(|(action, name, _)| (name, action))
            .collect()
    };
    let mut expected = vec![
        (PRICE_FILE.to_string(), "keep".to_string()),
        (PRICE_DATE.to_string(), "stale".to_string()),
        (PRICE_MINUTE.to_string(), "stale".to_string()),
        (PRICE_PRICE.to_string(), "stale".to_string()),
        (PRICEFILE.to_string(), "keep".to_string()),
        (key.clone(), "create".to_string()),
    ];
    expected.sort();
    let mut planned = stated(&plan);
    planned.sort();
    assert_eq!(planned, expected, "{plan}");
    assert_eq!(
        names(&plan, "required_elsewhere"),
        [FLAGGED, PRICE_SLOT],
        "slot_report's indexes, recorded by the release"
    );
    assert_eq!(
        indexes(&db.url).await,
        db.built,
        "a dry run changes nothing"
    );

    let applied = db.provision(&["--prune"], &[price]);
    assert_eq!(
        stated(&applied),
        stated(&plan),
        "the run does what the plan said"
    );
    assert_eq!(
        stale(&applied),
        [
            "morpholog_ci_price_1_vk1_79dffb343ef9",
            "morpholog_ci_price_3_vk1_88e8b21a18a0",
            "morpholog_ci_price_4_vk1_da052733c5d1",
            "morpholog_cs_vk1_p3",
            "morpholog_cs_vk1_p4",
        ]
    );
    let after = indexes(&db.url).await;
    for kept in [PRICE_FILE, PRICE_SLOT, PRICEFILE, FLAGGED] {
        assert_eq!(
            after.get(kept),
            db.built.get(kept),
            "{kept} is the index the release built, not a rebuild"
        );
    }
    assert!(after.contains_key(&key), "{after:?}");
    assert_eq!(after.len(), 5, "{after:?}");

    let check = db.check();
    assert!(check.status.success(), "current for both: {check:?}");
    assert_eq!(indexes(&db.url).await, after, "a check changes nothing");
    db.drop().await;
}

/// An operator's own index over the same key, the same expressions in the
/// same order under the same predicate, satisfies the requirement: it is
/// not duplicated, not taken over, and a prune never drops it.
#[tokio::test(flavor = "current_thread")]
async fn an_operator_s_own_key_index_satisfies_the_key_and_is_never_pruned() {
    let Some(db) = Released::new("external").await else {
        return;
    };
    let pool = PgPool::connect(&db.url).await.unwrap();
    sqlx::raw_sql(
        "CREATE INDEX ops_price_key ON morpholog.claims
            ((morpholog.claim_digest(morpholog.value_key_v1(arguments -> 0))),
             (morpholog.claim_digest(morpholog.value_key_v1(arguments -> 1))),
             (morpholog.claim_digest(morpholog.value_key_v1(arguments -> 2))))
         WHERE predicate_name = 'Price'",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let ops = indexes(&db.url).await["ops_price_key"];

    let report = db.provision(&["--prune"], &db.both());
    let key = report["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["positions"] == serde_json::json!([0, 1, 2]))
        .unwrap();
    assert_eq!(key["action"], "satisfied_externally", "{report}");
    assert!(
        key["detail"].as_str().unwrap().contains("ops_price_key"),
        "{key}"
    );
    let after = indexes(&db.url).await;
    assert_eq!(after.get("ops_price_key"), Some(&ops), "{after:?}");
    assert!(
        !after.contains_key(key["name"].as_str().unwrap()),
        "not duplicated: {after:?}"
    );
    let pool = PgPool::connect(&db.url).await.unwrap();
    let managed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM morpholog.managed_index WHERE index_name = $1")
            .bind(key["name"].as_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(managed, 0, "not taken over");
    let again = db.provision(&["--prune"], &db.both());
    assert!(stale(&again).is_empty(), "{again}");
    assert_eq!(indexes(&db.url).await, after);
    db.drop().await;
}

/// A build of the key index interrupted part way leaves it invalid. The
/// indexes the database already had are untouched, and the next run
/// repairs the key index, after which everything is current.
#[tokio::test(flavor = "current_thread")]
async fn an_interrupted_build_of_the_key_index_leaves_the_rest_and_is_repaired() {
    if !common_superuser().await {
        eprintln!("skipped: marking an index invalid needs a superuser session");
        return;
    }
    let Some(db) = Released::new("interrupted").await else {
        return;
    };
    let first = db.provision(&[], &db.both());
    let key = entries(&first)
        .into_iter()
        .find(|(_, _, positions)| positions == &[0, 1, 2])
        .map(|(_, name, _)| name)
        .unwrap();
    let pool = PgPool::connect(&db.url).await.unwrap();
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false
         WHERE indexrelid = (SELECT oid FROM pg_class WHERE relname = $1)",
    )
    .bind(&key)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let before = indexes(&db.url).await;

    let repaired = db.provision(&[], &db.both());
    let actions: BTreeMap<String, String> = entries(&repaired)
        .into_iter()
        .map(|(action, name, _)| (name, action))
        .collect();
    assert_eq!(actions[&key], "repair_invalid", "{repaired}");
    let after = indexes(&db.url).await;
    for (name, oid) in &before {
        if *name != key {
            assert_eq!(after.get(name), Some(oid), "{name} untouched");
        }
    }
    assert_ne!(after[&key], before[&key], "the key index was rebuilt");
    db.provision(&["--prune"], &db.both());
    assert!(db.check().status.success(), "current once pruned");
    db.drop().await;
}

async fn common_superuser() -> bool {
    let pool = PgPool::connect(&morpholog_postgres::with_connection_defaults(
        &common::database_url(),
    ))
    .await
    .unwrap();
    let is: bool = sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = session_user")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    is
}
