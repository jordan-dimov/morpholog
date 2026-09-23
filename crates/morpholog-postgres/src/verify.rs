use crate::as_of::reconstruct_inner;
use crate::audit::REPLAY_CHUNK;
use crate::audit_pages::{ReplayPages, ReplayRow};
use crate::checkpoints::TreeVerification;
use crate::claims::decode_claim_rows;
use crate::error::{PgError, classify, classify_checked_query};
use crate::propose::REJECTION_KIND_INVARIANT;
use crate::txn::{TxIsolation, begin_isolated_tx};
use crate::witnesses::WitnessesReport;
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use morpholog_core::{
    ClaimInstance, CoverageReport, CoverageTracker, PredicateName, Program, State,
};
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;
/// The outcome of replaying the audit log against the claims table.
///
/// The claims table and the audit log are independent records of one
/// history. Replaying the log must land on the same claim set; a
/// difference means one was modified outside the runtime.
///
/// Serialises with a `status` tag.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum VerifyOutcome {
    /// Replay reproduces the claims table exactly.
    Consistent {
        /// Committed transitions replayed.
        transitions: i64,
        /// Currently-admitted claims confirmed.
        claims: usize,
    },
    /// The two records disagree.
    Divergent {
        /// Claims present in the claims table that replaying the audit
        /// log does not produce - out-of-band inserts or edits.
        only_in_claims_table: Vec<ClaimInstance>,
        /// Claims the audit log says should be current but the claims
        /// table lacks - out-of-band deletes or edits.
        only_in_replay: Vec<ClaimInstance>,
    },
}
/// The `morpholog audit verify` envelope: the replay verdict (claims vs
/// audit log) and the tree verdict (Merkle tree vs checkpoints), plus the
/// view-surface verdict when requested and the witness report when any
/// checkpoint has witnesses. Field order is the wire contract.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub replay: VerifyOutcome,
    pub tree: TreeVerification,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<ViewsVerification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witnesses: Option<WitnessesReport>,
}

/// The verdict over a generated SQL view surface: the seal recorded at
/// apply time (each view's `pg_get_viewdef` hashed as it was created)
/// against a live re-read. A view redefined in place under the same name
/// passes the inventory but not this.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ViewsVerification {
    /// Every catalogued view (and the catalogue itself) has a seal, a
    /// live definition, and the two hashes agree.
    Intact { views_checked: u64 },
    /// The surface disagrees with its seal: `mismatched` names views
    /// whose live definition no longer hashes to the sealed value;
    /// `missing` names views the surface expects but that lack a seal
    /// row or a live definition (dropped, replaced by a table, or
    /// unsealed out of band).
    Tampered {
        mismatched: Vec<String>,
        missing: Vec<String>,
    },
    /// No seal table in the schema: the views predate sealing or were
    /// never applied. Nothing to compare - visible, not a failure.
    NotSealed,
}

/// Verify the generated view surface in `schema`. Cross-checks the
/// intended inventory (`_morpholog_catalog`), the seal
/// (`_morpholog_view_defs`), and the live views: a view missing from
/// any leg is named, so deleting a seal row hides nothing.
pub async fn verify_views(pool: &PgPool, schema: &str) -> Result<ViewsVerification, PgError> {
    let sealed_exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'r'
           ) AS "sealed!""#,
        schema,
        crate::sql_views::VIEW_DEFS_TABLE,
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    if !sealed_exists {
        return Ok(ViewsVerification::NotSealed);
    }

    // The schema is a runtime input, so these reads cannot be
    // compile-checked; it is quoted by the generator's own rule.
    let sealed_sql = format!(
        "SELECT view_name, definition_sha256 FROM {}.{}",
        crate::sql_quote::quote_ident(schema),
        crate::sql_quote::quote_ident(crate::sql_views::VIEW_DEFS_TABLE),
    );
    // Audited for AssertSqlSafe: only a caller's schema name and a crate
    // constant are interpolated, both through `quote_ident`.
    let sealed: HashMap<String, String> =
        sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sealed_sql))
            .fetch_all(pool)
            .await
            .map_err(classify)?
            .into_iter()
            .collect();

    let mut missing: Vec<String> = Vec::new();
    let mut intended: Vec<String>;
    let catalog_live = live_view_hash(pool, schema, crate::sql_views::CATALOG_VIEW).await?;
    if catalog_live.is_some() {
        let catalog_sql = format!(
            "SELECT DISTINCT view_name FROM {}.{}",
            crate::sql_quote::quote_ident(schema),
            crate::sql_quote::quote_ident(crate::sql_views::CATALOG_VIEW),
        );
        // Audited: same shape as the sealed-table read above.
        intended = match sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(catalog_sql))
            .fetch_all(pool)
            .await
        {
            Ok(names) => names,
            // A catalogue without a readable `view_name` column is
            // tampering, not an operational failure: fall back to the
            // seal's inventory and let the hash check name it. Anything
            // else stays an error.
            Err(sqlx::Error::Database(db)) if db.code().as_deref() == Some("42703") => {
                sealed.keys().cloned().collect()
            }
            Err(sqlx::Error::ColumnDecode { .. } | sqlx::Error::ColumnNotFound(_)) => {
                sealed.keys().cloned().collect()
            }
            Err(e) => return Err(classify(e)),
        };
    } else {
        // The catalogue itself is gone: name it, and fall back to the
        // seal's own inventory so its views are still checked.
        missing.push(crate::sql_views::CATALOG_VIEW.to_string());
        intended = sealed.keys().cloned().collect();
    }
    intended.push(crate::sql_views::CATALOG_VIEW.to_string());
    intended.sort();
    intended.dedup();

    let mut mismatched: Vec<String> = Vec::new();
    let mut views_checked: u64 = 0;
    for name in &intended {
        match (sealed.get(name), live_view_hash(pool, schema, name).await?) {
            (Some(sealed_hash), Some(live_hash)) if *sealed_hash == live_hash => {
                views_checked += 1;
            }
            (Some(_), Some(_)) => mismatched.push(name.clone()),
            _ if missing.contains(name) => {}
            _ => missing.push(name.clone()),
        }
    }
    if mismatched.is_empty() && missing.is_empty() {
        Ok(ViewsVerification::Intact { views_checked })
    } else {
        mismatched.sort();
        missing.sort();
        Ok(ViewsVerification::Tampered {
            mismatched,
            missing,
        })
    }
}

/// The live definition hash of one view, as the seal records it:
/// `sha256(pg_get_viewdef(oid, true))`. `None` when no view of that name
/// exists in the schema.
async fn live_view_hash(
    pool: &PgPool,
    schema: &str,
    view: &str,
) -> Result<Option<String>, PgError> {
    sqlx::query_scalar!(
        r#"SELECT encode(sha256(convert_to(pg_get_viewdef(c.oid, true), 'UTF8')), 'hex') AS "hash!"
           FROM pg_catalog.pg_class c
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'"#,
        schema,
        view,
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)
}
/// Report, per invariant of `program`, whether its antecedent ever bound
/// and whether it ever refused a real proposal: "which rules have done
/// work?" and "which have said no?". Replays the audit log through a
/// [`CoverageTracker`], then counts the rejection log into it. See
/// `morpholog_core::coverage` for the verdicts.
///
/// One `SERIALIZABLE READ ONLY DEFERRABLE` transaction reads everything:
/// it waits for a safe snapshot, then reads with no SSI footprint.
///
/// Cost: one pass over the log, plus a state snapshot and antecedent
/// evaluation for each transition whose delta touches a tracked
/// antecedent. An offline auditor command, not a hot path.
pub async fn coverage_replay(pool: &PgPool, program: &Program) -> Result<CoverageReport, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let mut tracker = CoverageTracker::new(program);
    let needs_pre = tracker.needs_pre_state();
    let mut replay = State::default();
    // The previous transition's state, kept only when some antecedent uses
    // pre(...). Otherwise, and before the first transition, it is empty,
    // never absent, so pre(...) evaluates rather than errors.
    let mut pre_state = State::from_claims(Vec::new());
    // Paged inside the snapshot: one chunk in memory at a time.
    let mut pages = ReplayPages::new(None);
    loop {
        let rows = pages.next(&mut tx).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let ReplayRow {
                transition_id,
                transformation_name,
                asserted_claims: asserted_json,
                retracted_claims: retracted_json,
                ..
            } = row;
            let asserted: Vec<ClaimInstance> = serde_json::from_value(asserted_json)?;
            let retracted: Vec<ClaimInstance> = serde_json::from_value(retracted_json)?;
            let delta: std::collections::BTreeSet<PredicateName> = retracted
                .iter()
                .chain(asserted.iter())
                .map(|c| c.predicate.clone())
                .collect();
            replay.apply(&asserted, &retracted);
            // Rules the change cannot fire are skipped inside observe;
            // the transition still counts.
            tracker
                .observe(
                    &replay,
                    &pre_state,
                    &delta,
                    &transition_id.to_string(),
                    &transformation_name,
                )
                .map_err(PgError::Kernel)?;
            if needs_pre {
                pre_state = replay.clone();
            }
        }
    }
    // Second pass: the rejection log, in the same snapshot so refusals
    // and committed history describe one moment. Counting only.
    struct RejRow {
        rejection_id: Uuid,
        transformation_name: String,
        kind: String,
        rule: String,
        rejected_at: Timestamp,
    }
    let mut rej_cursor: Option<(Timestamp, Uuid)> = None;
    loop {
        let rows: Vec<RejRow> = match &rej_cursor {
            None => {
                sqlx::query_as!(
                    RejRow,
                    "SELECT rejection_id, transformation_name, kind, rule, rejected_at
                     FROM morpholog.rejections
                     ORDER BY rejected_at, rejection_id
                     LIMIT $1",
                    REPLAY_CHUNK,
                )
                .fetch_all(&mut *tx)
                .await
            }
            Some((after_at, after_id)) => {
                sqlx::query_as!(
                    RejRow,
                    "SELECT rejection_id, transformation_name, kind, rule, rejected_at
                     FROM morpholog.rejections
                     WHERE (rejected_at, rejection_id) > ($2, $3)
                     ORDER BY rejected_at, rejection_id
                     LIMIT $1",
                    REPLAY_CHUNK,
                    after_at.to_sqlx(),
                    *after_id,
                )
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(classify)?;
        let Some(last) = rows.last() else {
            break;
        };
        rej_cursor = Some((last.rejected_at, last.rejection_id));
        let exhausted = (rows.len() as i64) < REPLAY_CHUNK;
        for row in rows {
            let invariant = (row.kind == REJECTION_KIND_INVARIANT).then_some(row.rule.as_str());
            tracker.observe_rejection(
                invariant,
                &row.transformation_name,
                &row.rejection_id.to_string(),
            );
        }
        if exhausted {
            break;
        }
    }
    tx.commit().await.map_err(classify)?;
    Ok(tracker.into_report())
}
/// Replay the audit log to its latest transition and compare the
/// reconstructed state against the claims table.
///
/// All reads share one `REPEATABLE READ READ ONLY` snapshot, so a
/// concurrent commit cannot fake a divergence by appearing in one record
/// but not the other. Safe against a live system.
///
/// An empty database is consistent. The comparison is an
/// order-insensitive multiset diff; divergences are sorted by
/// `(predicate, args)` so the report is deterministic.
pub async fn verify_replay(pool: &PgPool) -> Result<VerifyOutcome, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::RepeatableReadReadOnly).await?;
    let latest = sqlx::query!(
        "SELECT transition_id FROM morpholog.audit
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    // count(*) is never null, but Postgres cannot prove it.
    let transitions = sqlx::query!(r#"SELECT count(*) AS "count!" FROM morpholog.audit"#)
        .fetch_one(&mut *tx)
        .await
        .map_err(classify_checked_query)?
        .count;
    let replayed = match latest {
        Some(row) => reconstruct_inner(&mut tx, row.transition_id, None)
            .await?
            .claims()
            .to_vec(),
        None => Vec::new(),
    };
    // Keyset over the primary key: the diff ignores order, and the claims
    // table is paged rather than held whole.
    struct ClaimRow {
        predicate_name: String,
        arguments: serde_json::Value,
        arguments_hash: Vec<u8>,
    }
    let mut rows: Vec<(String, serde_json::Value)> = Vec::new();
    let mut claims_cursor: Option<(String, Vec<u8>)> = None;
    loop {
        let page: Vec<ClaimRow> = match &claims_cursor {
            None => {
                sqlx::query_as!(
                    ClaimRow,
                    "SELECT predicate_name, arguments, arguments_hash
                     FROM morpholog.claims
                     ORDER BY predicate_name, arguments_hash
                     LIMIT $1",
                    REPLAY_CHUNK,
                )
                .fetch_all(&mut *tx)
                .await
            }
            Some((pred, hash)) => {
                sqlx::query_as!(
                    ClaimRow,
                    "SELECT predicate_name, arguments, arguments_hash
                     FROM morpholog.claims
                     WHERE (predicate_name, arguments_hash) > ($2, $3)
                     ORDER BY predicate_name, arguments_hash
                     LIMIT $1",
                    REPLAY_CHUNK,
                    pred,
                    hash,
                )
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(classify)?;
        let Some(last) = page.last() else {
            break;
        };
        claims_cursor = Some((last.predicate_name.clone(), last.arguments_hash.clone()));
        let exhausted = (page.len() as i64) < REPLAY_CHUNK;
        rows.extend(page.into_iter().map(|r| (r.predicate_name, r.arguments)));
        if exhausted {
            break;
        }
    }
    tx.commit().await.map_err(classify)?;
    let current = decode_claim_rows(rows)?;
    // Multiset diff: +1 per current claim, -1 per replayed claim.
    // Positive residue exists only in the claims table, negative only
    // in the replay.
    let mut counts: HashMap<&ClaimInstance, i64> = HashMap::new();
    for c in &current {
        *counts.entry(c).or_default() += 1;
    }
    for c in &replayed {
        *counts.entry(c).or_default() -= 1;
    }
    let mut only_in_claims_table = Vec::new();
    let mut only_in_replay = Vec::new();
    for (claim, n) in counts {
        for _ in 0..n.abs() {
            if n > 0 {
                only_in_claims_table.push(claim.clone());
            } else if n < 0 {
                only_in_replay.push(claim.clone());
            }
        }
    }
    let sort_key = |c: &ClaimInstance| {
        (
            c.predicate.to_string(),
            serde_json::to_string(&c.args).unwrap_or_default(),
        )
    };
    only_in_claims_table.sort_by_key(sort_key);
    only_in_replay.sort_by_key(sort_key);
    if only_in_claims_table.is_empty() && only_in_replay.is_empty() {
        Ok(VerifyOutcome::Consistent {
            transitions,
            claims: current.len(),
        })
    } else {
        Ok(VerifyOutcome::Divergent {
            only_in_claims_table,
            only_in_replay,
        })
    }
}
