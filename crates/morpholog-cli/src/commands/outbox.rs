//! `morpholog outbox` - claim, complete, and release outbox rows
//! from outside Rust. The bridge that lets a Python or shell
//! deliverer participate in the lease protocol without writing a
//! `Deliverer` trait impl.
//!
//! Three subcommands, each calling directly into the existing
//! [`morpholog_postgres`] helpers that the in-process worker already
//! uses internally:
//!
//! - `claim` -> [`morpholog_postgres::claim_pending_outbox_row`]
//! - `complete --outcome delivered|transient|failed` -> the
//!   matching `mark_outbox_*` helper
//! - `release` -> [`morpholog_postgres::release_outbox_claim`]
//!
//! Output is always JSON. `claim` wraps the row as
//! `{"row": <OutboxRow>}` (or `{"row": null}` when nothing is
//! claimable); `complete` and `release` emit the result of the
//! lease-gated helper as JSON so a script can distinguish
//! `LeaseLost` from `Updated`.
//!
//! Compensation is deliberately out of scope at the CLI: if a
//! deployment needs the failed-then-compensate flow, it uses the
//! in-process worker which carries `CompensationSpec`. CLI consumers
//! mark a row `failed` and stop there.

use anyhow::{Context, anyhow};
use chrono::{DateTime, Utc};
use morpholog_postgres::{
    OutboxUpdate, claim_pending_outbox_row, mark_outbox_delivered, mark_outbox_failed,
    mark_outbox_transient_attempt, release_outbox_claim,
};
use std::time::Duration;
use uuid::Uuid;

use crate::commands::{connect, print_json};
use crate::{OutboxClaimArgs, OutboxCompleteArgs, OutboxCompleteOutcome, OutboxReleaseArgs};

/// `morpholog outbox claim` - acquire the next pending outbox row of
/// the given intent type, leasing it for `--lease-seconds` (default 30).
/// Generates a UUIDv7 `worker_id` if the caller did not supply one;
/// returns it inside the row's `locked_by` field so the caller can
/// pass it back to `complete` or `release`.
///
/// Exit codes: 0 always. Empty output (`{"row": null}`) means no
/// claimable row right now, which is normal and not an error.
pub(crate) async fn claim(args: OutboxClaimArgs) -> anyhow::Result<()> {
    let worker_id = args
        .worker_id
        .clone()
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let lease = Duration::from_secs(args.lease_seconds);
    let pool = connect(&args.database_url).await?;
    let row = claim_pending_outbox_row(&pool, &worker_id, &args.intent_type, lease, Utc::now())
        .await
        .context("claim_pending_outbox_row failed")?;
    // Wrap consistently in `{"row": ...}`. `locked_by` and
    // `lock_expires_at` already live inside the row, so we do not
    // surface the worker_id or lease end-time separately.
    print_json(&serde_json::json!({ "row": row }))?;
    Ok(())
}

/// `morpholog outbox complete` - resolve a leased row to a terminal
/// outcome. `delivered` marks the row done; `transient` schedules
/// another attempt after `--retry-after-seconds`; `failed` marks the
/// row failed with an optional `--reason`.
///
/// All three paths are lease-gated: if the worker no longer holds
/// the lease (because it expired and another worker reclaimed the
/// row), the helper returns `LeaseLost` and the CLI exits 1.
pub(crate) async fn complete(args: OutboxCompleteArgs) -> anyhow::Result<()> {
    // Reject contradictory flag combinations up front so the
    // caller's mistake is visible without a database round trip.
    if matches!(args.outcome, OutboxCompleteOutcome::Transient)
        && args.retry_after_seconds.is_none()
    {
        return Err(anyhow!(
            "--outcome transient requires --retry-after-seconds N"
        ));
    }
    if !matches!(args.outcome, OutboxCompleteOutcome::Transient)
        && args.retry_after_seconds.is_some()
    {
        return Err(anyhow!(
            "--retry-after-seconds is only meaningful with --outcome transient"
        ));
    }
    if matches!(args.outcome, OutboxCompleteOutcome::Delivered) && args.reason.is_some() {
        return Err(anyhow!(
            "--reason is only meaningful with --outcome transient or failed"
        ));
    }

    let pool = connect(&args.database_url).await?;
    let update = match args.outcome {
        OutboxCompleteOutcome::Delivered => {
            mark_outbox_delivered(&pool, args.intent_id, &args.worker_id)
                .await
                .context("mark_outbox_delivered failed")?
        }
        OutboxCompleteOutcome::Transient => {
            // The validation block above proved this option is Some
            // for the transient branch; the error case is unreachable
            // here, but we surface it as anyhow rather than
            // unwrap/expect (clippy::unwrap_used / expect_used).
            let secs = args
                .retry_after_seconds
                .ok_or_else(|| anyhow!("--outcome transient requires --retry-after-seconds N"))?;
            let retry_after = Duration::from_secs(secs);
            let next_attempt_at: DateTime<Utc> = Utc::now()
                + chrono::Duration::from_std(retry_after)
                    .context("retry-after-seconds overflowed chrono::Duration")?;
            // `mark_outbox_transient_attempt` does not persist a
            // reason today (the helper records the next attempt
            // schedule, not a per-attempt narrative). If the caller
            // supplied --reason we accept it and silently drop it;
            // a future enhancement to the helper would carry it
            // through. Tracked in the CLI docstring.
            mark_outbox_transient_attempt(&pool, args.intent_id, &args.worker_id, next_attempt_at)
                .await
                .context("mark_outbox_transient_attempt failed")?
        }
        OutboxCompleteOutcome::Failed => mark_outbox_failed(
            &pool,
            args.intent_id,
            &args.worker_id,
            args.reason.as_deref().unwrap_or(""),
        )
        .await
        .context("mark_outbox_failed failed")?,
    };
    emit_update_and_exit(&update)
}

/// `morpholog outbox release` - abandon the lease on a row,
/// returning it to `pending` so another worker can claim it. For
/// graceful shutdown of an external deliverer that has in-flight
/// claims it can no longer service.
pub(crate) async fn release(args: OutboxReleaseArgs) -> anyhow::Result<()> {
    let pool = connect(&args.database_url).await?;
    let update = release_outbox_claim(&pool, args.intent_id, &args.worker_id)
        .await
        .context("release_outbox_claim failed")?;
    emit_update_and_exit(&update)
}

/// Emit the `OutboxUpdate` as JSON and translate the variant into
/// an exit code. `Updated` is the happy path (exit 0); `LeaseLost`
/// is exit 1 - not a bug in the CLI, but the caller's lease was
/// stolen and the state change did not apply.
fn emit_update_and_exit(update: &OutboxUpdate) -> anyhow::Result<()> {
    print_json(update)?;
    match update {
        OutboxUpdate::Applied => Ok(()),
        OutboxUpdate::LeaseLost => std::process::exit(1),
    }
}
