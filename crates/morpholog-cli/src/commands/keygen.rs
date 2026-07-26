//! `morpholog audit keygen` - generate an Ed25519 audit-signing keypair.

use std::io::Write;

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

    write_private_key(&args.private_out, &pem)?;
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

/// Write a private key, refusing to overwrite an existing file and, on
/// Unix, creating it `0600` so it is never world-readable - a private key
/// is not a casual `fs::write`.
fn write_private_key(path: &std::path::Path, pem: &str) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).with_context(|| {
        format!(
            "creating the private key at {} (it must not already exist)",
            path.display()
        )
    })?;
    file.write_all(pem.as_bytes())
        .with_context(|| format!("writing the private key to {}", path.display()))?;
    Ok(())
}
