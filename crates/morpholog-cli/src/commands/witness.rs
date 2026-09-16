//! `morpholog audit witness` - have an outside authority witness a recorded
//! checkpoint, and the submission `audit checkpoint --witness` shares.

use std::str::FromStr;

use anyhow::Context;
use base64::Engine as _;
use morpholog_postgres::{
    Checkpoint, TreeHead, Witness, WitnessScheme, attach_witness, load_checkpoint,
    tree_head_witness_bytes,
};
use morpholog_witness::{Refusal, build_request, check_response};

use crate::WitnessArgs;
use crate::commands::{connect, print_json};

/// An authority to submit to: `rfc3161:<url>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WitnessTarget {
    pub(crate) scheme: WitnessScheme,
    pub(crate) url: String,
}

impl FromStr for WitnessTarget {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            Some(("rfc3161", url)) if url.starts_with("http://") || url.starts_with("https://") => {
                Ok(WitnessTarget {
                    scheme: WitnessScheme::Rfc3161,
                    url: url.to_string(),
                })
            }
            Some(("rfc3161", _)) => Err("rfc3161 needs an http(s) URL: rfc3161:https://...".into()),
            _ => Err("expected `rfc3161:<url>`".into()),
        }
    }
}

/// Run `audit witness`: submit the recorded head at `--tree-size` to each
/// authority and store what comes back. Prints the checkpoint as now
/// stored; a submission that fails stores nothing and exits one.
pub(crate) async fn run(args: WitnessArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;
    let mut checkpoint = load_checkpoint(&pool, args.tree_size)
        .await
        .context("load_checkpoint failed")?
        .with_context(|| format!("no checkpoint is recorded at tree size {}", args.tree_size))?;
    for target in &args.witness {
        let witness = obtain(target, &checkpoint).await?;
        checkpoint = attach_witness(
            &pool,
            checkpoint.tree_size,
            &checkpoint.checkpoint_hash,
            witness,
        )
        .await
        .context("attach_witness failed")?;
    }
    print_json(&checkpoint)?;
    Ok(())
}

/// Ask the authority to witness this head. The response is self-checked
/// before it is returned for storage: over this head's witness payload,
/// echoing the request's nonce, granted. Trust is not judged here - that
/// is the verifier's, with its own anchors. A response whose signature
/// this build cannot check is still returned: it is over this head, a
/// later verifier may check it, and it is never reported verified until
/// one does.
pub(crate) async fn obtain(target: &WitnessTarget, head: &Checkpoint) -> anyhow::Result<Witness> {
    let payload = tree_head_witness_bytes(&TreeHead {
        tree_size: head.tree_size,
        root_hash: &head.root_hash,
        prev_checkpoint_hash: head.prev_checkpoint_hash.as_deref(),
        checkpoint_hash: &head.checkpoint_hash,
    });
    let request = build_request(&payload);
    let url = target.url.clone();
    let der = request.der.clone();
    let response = tokio::task::spawn_blocking(move || post_timestamp_query(&url, &der))
        .await
        .context("the submission task panicked")?
        .with_context(|| format!("submitting to the timestamp authority at {}", target.url))?;
    match check_response(&request, &response, &payload) {
        Ok(_) => {}
        Err(Refusal::Unsupported { detail }) => {
            eprintln!(
                "note: {} answered with a token this build cannot check ({detail}); \
                 stored, and reported `unsupported` until a verifier can",
                target.url
            );
        }
        Err(Refusal::Invalid { detail }) => anyhow::bail!(
            "{} answered with a response that does not vouch for this head: {detail}; \
             nothing stored",
            target.url
        ),
    }
    Ok(Witness {
        scheme: target.scheme,
        proof: base64::engine::general_purpose::STANDARD.encode(&response),
        submitted_to: target.url.clone(),
    })
}

/// One RFC 3161 exchange over HTTP: post the DER request, take the DER
/// response, exactly as the authority sent it.
fn post_timestamp_query(url: &str, der: &[u8]) -> anyhow::Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let response = agent
        .post(url)
        .header("Content-Type", "application/timestamp-query")
        .header("Accept", "application/timestamp-reply")
        .send(der)?;
    let body = response
        .into_body()
        .with_config()
        .limit(1 << 20)
        .read_to_vec()
        .context("reading the authority's response")?;
    if body.is_empty() {
        anyhow::bail!("the authority returned an empty body");
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_a_scheme_and_an_http_url() {
        let t: WitnessTarget = "rfc3161:https://timestamp.example/tsr".parse().unwrap();
        assert_eq!(t.scheme, WitnessScheme::Rfc3161);
        assert_eq!(t.url, "https://timestamp.example/tsr");
        assert!(
            "rfc3161:timestamp.example"
                .parse::<WitnessTarget>()
                .is_err()
        );
        assert!("ots:https://a.example".parse::<WitnessTarget>().is_err());
        assert!("https://a.example".parse::<WitnessTarget>().is_err());
    }
}
