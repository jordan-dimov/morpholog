//! `morpholog outbox` - claim, complete, and release outbox rows from
//! outside Rust, so a Python or shell deliverer can take part in the lease
//! protocol without a `Deliverer` impl.
//!
//! Each subcommand calls the same [`morpholog_postgres`] helper the
//! in-process worker uses:
//!
//! - `claim` -> [`morpholog_postgres::claim_pending_outbox_row`]
//! - `complete --outcome delivered|transient|failed` -> the
//!   matching `mark_outbox_*` helper
//! - `release` -> [`morpholog_postgres::release_outbox_claim`]
//!
//! Output is always JSON. `claim` prints `{"row": <OutboxRow>}`, or
//! `{"row": null}` when nothing is claimable. `complete` and `release`
//! print `{"status": "applied"}`, or `{"status": "lease_lost"}` when the
//! worker no longer holds the lease.
//!
//! No compensation here: a deployment that needs it uses the in-process
//! worker, which carries `CompensationSpec`. CLI users mark a row `failed`
//! and stop.

use anyhow::{Context, anyhow};
use jiff::Timestamp;
use morpholog_postgres::{
    OutboxUpdate, claim_pending_outbox_row, mark_outbox_delivered, mark_outbox_failed,
    mark_outbox_transient_attempt, release_outbox_claim,
};
use std::time::Duration;
use uuid::Uuid;

use crate::commands::{AlreadyReported, connect, print_json};
use crate::{OutboxClaimArgs, OutboxCompleteArgs, OutboxCompleteOutcome, OutboxReleaseArgs};

/// `morpholog outbox claim` - lease the next pending row of the given
/// intent type for `--lease-seconds`. Without a `worker_id`, one is
/// generated; it comes back in the row's `locked_by` field, for passing to
/// `complete` or `release`.
///
/// Exits 0 on success, including when no row is available. Non-zero only
/// on an operational error.
pub(crate) async fn claim(args: OutboxClaimArgs) -> anyhow::Result<()> {
    let worker_id = args
        .worker_id
        .clone()
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let lease = Duration::from_secs(args.lease_seconds);
    let pool = connect(&args.db.database_url).await?;
    let row = claim_pending_outbox_row(
        &pool,
        &worker_id,
        &args.intent_type,
        lease,
        Timestamp::now(),
    )
    .await
    .context("claim_pending_outbox_row failed")?;
    // The row already carries `locked_by` and `lock_expires_at`.
    print_json(&serde_json::json!({ "row": row }))?;
    Ok(())
}

/// `morpholog outbox complete` - resolve a leased row to a terminal
/// outcome. `delivered` marks the row done; `transient` schedules
/// another attempt after `--retry-after-seconds`; `failed` marks the
/// row failed with an optional `--reason`.
///
/// Every outcome needs the lease. If it expired and another worker took
/// the row, this prints `{"status": "lease_lost"}` and exits 1.
pub(crate) async fn complete(args: OutboxCompleteArgs) -> anyhow::Result<()> {
    // Reject contradictory flags before touching the database.
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
    // A transient attempt stores no reason, and silently dropping one the
    // caller gave would mislead, so `--reason` is for `failed` only.
    if !matches!(args.outcome, OutboxCompleteOutcome::Failed) && args.reason.is_some() {
        return Err(anyhow!("--reason is only meaningful with --outcome failed"));
    }

    let pool = connect(&args.db.database_url).await?;
    let update = match args.outcome {
        OutboxCompleteOutcome::Delivered => {
            mark_outbox_delivered(&pool, args.intent_id, &args.worker_id)
                .await
                .context("mark_outbox_delivered failed")?
        }
        OutboxCompleteOutcome::Transient => {
            // Checked above, so unreachable; an error rather than unwrap.
            let secs = args
                .retry_after_seconds
                .ok_or_else(|| anyhow!("--outcome transient requires --retry-after-seconds N"))?;
            let retry_after = Duration::from_secs(secs);
            let next_attempt_at = Timestamp::now()
                .checked_add(retry_after)
                .context("retry-after-seconds is beyond the representable calendar")?;
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
    emit_update_outcome(&update)
}

/// `morpholog outbox release` - give up the lease on a row, returning it
/// to `pending` for another worker. For a deliverer shutting down with
/// claims it can no longer serve.
pub(crate) async fn release(args: OutboxReleaseArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;
    let update = release_outbox_claim(&pool, args.intent_id, &args.worker_id)
        .await
        .context("release_outbox_claim failed")?;
    emit_update_outcome(&update)
}

/// Print the `OutboxUpdate` as `{"status": "applied"}` or
/// `{"status": "lease_lost"}`. `LeaseLost` exits 1: the caller's lease was
/// taken and the change did not apply.
fn emit_update_outcome(update: &OutboxUpdate) -> anyhow::Result<()> {
    let status = match update {
        OutboxUpdate::Applied => "applied",
        OutboxUpdate::LeaseLost => "lease_lost",
    };
    print_json(&serde_json::json!({ "status": status }))?;
    match update {
        OutboxUpdate::Applied => Ok(()),
        OutboxUpdate::LeaseLost => Err(AlreadyReported.into()),
    }
}
