//! `morpholog audit checkpoint` - record a tamper-evident checkpoint over the
//! audit log and print it as an external anchor.

use anyhow::Context;
use morpholog_cli::envelopes;
use morpholog_postgres::{
    CheckpointOutcome, CheckpointSigner, PgError, create_checkpoint, signing_key_from_pem,
    transition_is_covered,
};

use crate::CheckpointArgs;
use crate::commands::witness::{report_failures, witness_all};
use crate::commands::{AlreadyReported, connect, print_json};

/// How often `--wait` asks again whether the prefix has reached the
/// transition: each ask reads the watermark, so a few a second is cheap.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// The longest `--wait`: the prefix reaches a committed transition once
/// the writers that started before it finish, usually within seconds.
const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// `--wait`'s value: a duration from nothing to a day, written `30s`, `2m`
/// or `PT30S`.
pub(crate) fn parse_wait(text: &str) -> Result<std::time::Duration, String> {
    let duration: jiff::SignedDuration = text
        .parse()
        .map_err(|e| format!("not a duration (30s, 2m or PT30S): {e}"))?;
    let wait = std::time::Duration::try_from(duration)
        .map_err(|_| "a wait cannot be negative".to_string())?;
    if wait > WAIT_LIMIT {
        return Err(
            "a wait is at most a day: the prefix reaches a transition within seconds once \
                    the writers before it finish"
                .to_string(),
        );
    }
    Ok(wait)
}

/// Run `audit checkpoint`: compute the audit Merkle root, chain it onto the
/// previous checkpoint, optionally sign it, and print it as JSON.
///
/// Keep the output outside the database. A later `verify --anchor-file`
/// against it catches a rewrite of both the audit log and the checkpoint
/// table. A signature says who made the anchor; a `--witness` dates it.
/// Witnessing runs after the commit, outside any transaction: every named
/// authority is tried, the checkpoint is kept and printed regardless, and
/// any failed submission exits 1 naming the retry. With `--covering`, a
/// transition the stable prefix has not reached is the one refusal a
/// deliverer retries, so it is a coded object on stdout; every other
/// failure stays an error.
pub(crate) async fn run(args: CheckpointArgs) -> anyhow::Result<()> {
    let signer = match (&args.signing_key, &args.key_id) {
        (Some(path), Some(key_id)) => {
            let pem = std::fs::read_to_string(path)
                .with_context(|| format!("reading signing key {}", path.display()))?;
            let key = signing_key_from_pem(&pem)
                .with_context(|| format!("parsing signing key {}", path.display()))?;
            Some(CheckpointSigner {
                key_id: key_id.clone(),
                key,
            })
        }
        // clap's `requires` keeps the two flags together, so the mixed
        // cases never reach here.
        _ => None,
    };

    let pool = connect(&args.db.database_url).await?;
    // Wait on the cheap question, the watermark and one lookup, then take
    // the checkpoint once: a checkpoint reads the whole prefix.
    if let (Some(id), Some(wait)) = (args.covering, args.wait) {
        let deadline = std::time::Instant::now() + wait;
        while !transition_is_covered(&pool, id)
            .await
            .context("create_checkpoint failed")?
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(WAIT_POLL).await;
        }
    }
    let mut outcome = match create_checkpoint(&pool, signer.as_ref(), args.covering).await {
        Ok(outcome) => outcome,
        Err(PgError::TransitionNotCovered { id, tree_size }) => {
            print_json(&envelopes::CheckpointRefusal::not_covered(id, tree_size))?;
            return Err(AlreadyReported.into());
        }
        Err(e) => return Err(anyhow::Error::new(e).context("create_checkpoint failed")),
    };

    let mut failed = Vec::new();
    match &mut outcome {
        CheckpointOutcome::Created(checkpoint) => {
            let (witnessed, failures) =
                witness_all(&pool, checkpoint.clone(), &args.witness).await?;
            *checkpoint = witnessed;
            failed = failures;
        }
        CheckpointOutcome::NoNewRows(checkpoint) if !args.witness.is_empty() => {
            eprintln!(
                "note: no new rows, so nothing was submitted; to witness the current head \
                 run `audit witness --tree-size {}`",
                checkpoint.tree_size
            );
        }
        CheckpointOutcome::NoNewRows(_) => {}
    }
    print_json(&outcome)?;
    let tree_size = match &outcome {
        CheckpointOutcome::Created(c) | CheckpointOutcome::NoNewRows(c) => c.tree_size,
    };
    if report_failures(tree_size, &failed) {
        return Err(AlreadyReported.into());
    }
    Ok(())
}
