//! `morpholog audit verify` - replay the audit log against the claims table,
//! and verify the audit Merkle tree against its checkpoints.

use anyhow::Context;
use morpholog_postgres::{
    SignaturePolicy, VerifyPass, VerifyProgress, VerifyReport, WitnessAnchors, parse_public_key,
    render_public_key, verify_audit_tree_with_chain, verify_replay, verify_views, witnesses_report,
};

use crate::VerifyArgs;
use crate::commands::{AlreadyReported, connect, print_json, read_anchor};

/// Run `audit verify`: replay the audit log against the claims table, then
/// recompute the audit Merkle root against each checkpoint and any given
/// anchor. Prints both verdicts as one JSON object; exits 1 if either fails.
pub(crate) async fn run(args: VerifyArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    let mut progress = ProgressLine::default();
    let mut on_progress = |p| {
        if let Some(line) = progress.line(p) {
            eprintln!("{line}");
        }
    };
    let replay = verify_replay(&pool, &mut on_progress)
        .await
        .context("verify_replay failed")?;

    let anchor = read_anchor(args.anchor_file.as_deref())?;
    // The signature policy is checked in the same snapshot, over the chain
    // just proved.
    let policy = signature_policy(
        args.require_signatures,
        args.require_signatures_from,
        args.require_signing_key.as_deref(),
    )?;
    let (tree, chain, role_rebindings) =
        verify_audit_tree_with_chain(&pool, anchor, policy.as_ref(), &mut on_progress)
            .await
            .context("verify_audit_tree failed")?;
    // Witnesses are judged on the same chain but apart from the tree
    // verdict: a witness dates a head whether or not the log still matches
    // it.
    let anchors = witness_anchors(args.trusted_tsa_file.as_deref())?;
    let witnesses = witnesses_report(&chain, anchors.as_ref());

    // Opt-in: only a deployment that generated views has any to verify.
    let views = match &args.views_schema {
        Some(schema) => Some(
            verify_views(&pool, schema)
                .await
                .context("verify_views failed")?,
        ),
        None => None,
    };

    // A rebinding is a finding, never a failure: it does not affect the
    // exit code.
    let report = VerifyReport {
        replay,
        tree,
        views,
        witnesses,
        role_rebindings,
    };
    print_json(&report)?;

    if !report.passes() {
        return Err(AlreadyReported.into());
    }
    Ok(())
}

/// Rows between progress lines.
const PROGRESS_EVERY: u64 = 10_000;

/// A line for stderr each time a pass crosses another `PROGRESS_EVERY`
/// rows, so a long verification says it is working. A short one says
/// nothing; stdout is only ever the report.
#[derive(Default)]
struct ProgressLine {
    pass: Option<VerifyPass>,
    shown: u64,
}

impl ProgressLine {
    fn line(&mut self, progress: VerifyProgress) -> Option<String> {
        if self.pass != Some(progress.pass) {
            self.pass = Some(progress.pass);
            self.shown = 0;
        }
        let reached = progress.done / PROGRESS_EVERY;
        if reached <= self.shown {
            return None;
        }
        self.shown = reached;
        let verb = match progress.pass {
            VerifyPass::Replay => "replayed",
            VerifyPass::Tree => "hashed",
        };
        Some(format!(
            "audit verify: {verb} {} of {} transitions",
            progress.done, progress.total
        ))
    }
}

/// The timestamp-authority trust anchors the verifier named, or none.
pub(crate) fn witness_anchors(
    pem_file: Option<&std::path::Path>,
) -> anyhow::Result<Option<WitnessAnchors>> {
    match pem_file {
        Some(path) => {
            let pem = std::fs::read(path)
                .with_context(|| format!("reading the trusted TSA file {}", path.display()))?;
            let anchors = WitnessAnchors::from_pem(&pem)
                .with_context(|| format!("{} is not a PEM file of certificates", path.display()))?;
            Ok(Some(anchors))
        }
        None => Ok(None),
    }
}

/// The signature policy the flags spell, or none. Each flag alone requires
/// signatures: `--require-signatures` from zero, `--require-signatures-from
/// N` from N, and a pinned key from zero unless a threshold is also given.
/// The key file is re-rendered to canonical `ed25519-pub:<hex>`, so stray
/// whitespace does not matter.
pub(crate) fn signature_policy(
    require: bool,
    from: Option<i64>,
    key_file: Option<&std::path::Path>,
) -> anyhow::Result<Option<SignaturePolicy>> {
    let required_public_key = match key_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading the pinned signing key {}", path.display()))?;
            let key = parse_public_key(text.trim())
                .with_context(|| format!("{} is not an ed25519-pub:<hex> key", path.display()))?;
            Some(render_public_key(&key))
        }
        None => None,
    };
    if !require && from.is_none() && required_public_key.is_none() {
        return Ok(None);
    }
    Ok(Some(SignaturePolicy {
        from_tree_size: from.unwrap_or(0),
        required_public_key,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(pass: VerifyPass, total: u64) -> Vec<String> {
        let mut progress = ProgressLine::default();
        (1..=total.div_ceil(1024))
            .filter_map(|page| {
                progress.line(VerifyProgress {
                    pass,
                    done: (page * 1024).min(total),
                    total,
                })
            })
            .collect()
    }

    #[test]
    fn a_line_each_ten_thousand_rows_and_none_for_a_short_record() {
        assert!(pages(VerifyPass::Replay, 9_000).is_empty());
        assert_eq!(
            pages(VerifyPass::Replay, 25_000),
            [
                "audit verify: replayed 10240 of 25000 transitions",
                "audit verify: replayed 20480 of 25000 transitions",
            ]
        );
    }

    #[test]
    fn each_pass_counts_from_its_own_start() {
        let mut progress = ProgressLine::default();
        let at = |pass, done| VerifyProgress {
            pass,
            done,
            total: 30_000,
        };
        assert!(progress.line(at(VerifyPass::Replay, 20_480)).is_some());
        assert_eq!(
            progress.line(at(VerifyPass::Tree, 10_240)).as_deref(),
            Some("audit verify: hashed 10240 of 30000 transitions")
        );
    }
}
