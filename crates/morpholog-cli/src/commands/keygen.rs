//! `morpholog keygen` - generate an Ed25519 audit-signing keypair.

use anyhow::Context;
use morpholog_postgres::{generate_signing_key, render_public_key, signing_key_to_pem};

use crate::KeygenArgs;

/// Generate a keypair and write the PKCS#8 PEM private key and the
/// `ed25519-pub:<hex>` public key to the given files. The public key is
/// also printed to stdout - it is the value to admit as an
/// `AuditSigningKey` claim and hand to verifiers. No database, no network.
pub(crate) fn run(args: &KeygenArgs) -> anyhow::Result<()> {
    let key = generate_signing_key();
    let pem = signing_key_to_pem(&key).context("encoding the private key as PKCS#8 PEM")?;
    let public = render_public_key(&key.verifying_key());

    std::fs::write(&args.private_out, pem.as_bytes())
        .with_context(|| format!("writing the private key to {}", args.private_out.display()))?;
    std::fs::write(&args.public_out, format!("{public}\n"))
        .with_context(|| format!("writing the public key to {}", args.public_out.display()))?;

    eprintln!(
        "wrote the private key to {} (keep it secret) and the public key to {}",
        args.private_out.display(),
        args.public_out.display()
    );
    println!("{public}");
    Ok(())
}
