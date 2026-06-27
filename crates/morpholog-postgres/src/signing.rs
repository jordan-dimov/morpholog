//! Ed25519 signatures over audit tree heads.
//!
//! A checkpoint commits to a prefix of the audit log (see [`crate::merkle`],
//! [`crate::checkpoints`]). Signing the tree head makes an externally-held
//! anchor *attributable*: tampering then needs the private key, not just
//! write access, and a third party can verify the anchor against a known
//! public key.
//!
//! The signature is over a typed, length-delimited, versioned payload -
//! the DSSE pre-authentication-encoding idea: bind both the bytes *and* an
//! unambiguous payload type, never a bare concatenation - so a signature
//! can never be reinterpreted across formats or artefact kinds. A future
//! signed artefact (a schema manifest, an evidence pack) gets its own
//! payload type and cannot be confused with a tree-head signature.
//!
//! Pure and synchronous: no I/O, no database. Key file reading lives in
//! the CLI; this module turns bytes into keys and keys into signatures.

use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pkcs8::LineEnding;
use rand::RngCore;

/// The payload type bound into every audit-tree-head signature.
const TREE_HEAD_PAYLOAD_TYPE: &str = "application/vnd.morpholog.tree-head.v1";
/// Rendering prefixes, mirroring the `sha256:` convention on hashes.
const PUBLIC_KEY_PREFIX: &str = "ed25519-pub:";
const SIGNATURE_PREFIX: &str = "ed25519-sig:";

/// A malformed key or signature - a parse-time failure, distinct from a
/// signature that parses but does not verify (which is a verdict, not an
/// error).
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
}

/// The tree-head fields a signature commits to. Borrowed from a
/// [`crate::Checkpoint`] at the call site so this module stays decoupled
/// from the storage type.
pub struct TreeHead<'a> {
    pub tree_size: i64,
    pub root_hash: &'a str,
    pub prev_checkpoint_hash: Option<&'a str>,
    pub checkpoint_hash: &'a str,
}

fn push_field(buf: &mut Vec<u8>, field: &[u8]) {
    buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
    buf.extend_from_slice(field);
}

/// The exact bytes a tree-head signature commits to: a length-delimited,
/// typed, versioned encoding. Length prefixes make every field boundary
/// unambiguous; the leading payload type stops a signature for one
/// artefact kind being replayed as another.
pub fn tree_head_signing_bytes(purpose: &str, key_id: &str, head: &TreeHead<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    push_field(&mut b, TREE_HEAD_PAYLOAD_TYPE.as_bytes());
    push_field(&mut b, purpose.as_bytes());
    push_field(&mut b, key_id.as_bytes());
    push_field(&mut b, &head.tree_size.to_le_bytes());
    push_field(&mut b, head.root_hash.as_bytes());
    match head.prev_checkpoint_hash {
        Some(prev) => {
            b.push(1);
            push_field(&mut b, prev.as_bytes());
        }
        None => b.push(0),
    }
    push_field(&mut b, head.checkpoint_hash.as_bytes());
    b
}

/// Sign a tree head under `key_id` for `purpose`.
pub fn sign_tree_head(
    key: &SigningKey,
    purpose: &str,
    key_id: &str,
    head: &TreeHead<'_>,
) -> Signature {
    key.sign(&tree_head_signing_bytes(purpose, key_id, head))
}

/// Verify a tree-head signature against a public key. True only if the
/// signature is over exactly this `(purpose, key_id, head)` payload.
pub fn verify_tree_head(
    public_key: &VerifyingKey,
    signature: &Signature,
    purpose: &str,
    key_id: &str,
    head: &TreeHead<'_>,
) -> bool {
    public_key
        .verify(&tree_head_signing_bytes(purpose, key_id, head), signature)
        .is_ok()
}

/// Generate a fresh Ed25519 signing key from OS entropy.
pub fn generate_signing_key() -> SigningKey {
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

/// Render a private key as a PKCS#8 PEM document - the production key
/// file format (`openssl` reads it; less foot-shooting than raw hex).
pub fn signing_key_to_pem(key: &SigningKey) -> Result<String, SigningError> {
    key.to_pkcs8_pem(LineEnding::LF)
        .map(|pem| pem.to_string())
        .map_err(|e| SigningError::Malformed {
            what: "signing key",
            detail: e.to_string(),
        })
}

/// Parse a private key from a PKCS#8 PEM document.
pub fn signing_key_from_pem(pem: &str) -> Result<SigningKey, SigningError> {
    SigningKey::from_pkcs8_pem(pem).map_err(|e| SigningError::Malformed {
        what: "PKCS#8 PEM signing key",
        detail: e.to_string(),
    })
}

/// `ed25519-pub:<hex>`.
pub fn render_public_key(key: &VerifyingKey) -> String {
    format!("{PUBLIC_KEY_PREFIX}{}", to_hex(&key.to_bytes()))
}

/// Parse an `ed25519-pub:<hex>` public key.
pub fn parse_public_key(text: &str) -> Result<VerifyingKey, SigningError> {
    let hex = text
        .strip_prefix(PUBLIC_KEY_PREFIX)
        .ok_or_else(|| SigningError::Malformed {
            what: "public key",
            detail: format!("expected a `{PUBLIC_KEY_PREFIX}` prefix"),
        })?;
    let arr = from_hex::<32>(hex, "public key")?;
    VerifyingKey::from_bytes(&arr).map_err(|e| SigningError::Malformed {
        what: "public key",
        detail: e.to_string(),
    })
}

/// `ed25519-sig:<hex>`.
pub fn render_signature(sig: &Signature) -> String {
    format!("{SIGNATURE_PREFIX}{}", to_hex(&sig.to_bytes()))
}

/// Parse an `ed25519-sig:<hex>` signature.
pub fn parse_signature(text: &str) -> Result<Signature, SigningError> {
    let hex = text
        .strip_prefix(SIGNATURE_PREFIX)
        .ok_or_else(|| SigningError::Malformed {
            what: "signature",
            detail: format!("expected a `{SIGNATURE_PREFIX}` prefix"),
        })?;
    let arr = from_hex::<64>(hex, "signature")?;
    Ok(Signature::from_bytes(&arr))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn from_hex<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], SigningError> {
    if s.len() != N * 2 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(SigningError::Malformed {
            what,
            detail: format!("expected {N} hex-encoded bytes"),
        });
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| SigningError::Malformed {
            what,
            detail: e.to_string(),
        })?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_head() -> TreeHead<'static> {
        TreeHead {
            tree_size: 42,
            root_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            prev_checkpoint_hash: None,
            checkpoint_hash: "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        }
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let key = generate_signing_key();
        let sig = sign_tree_head(&key, "audit_checkpoint_v1", "k1", &sample_head());
        assert!(verify_tree_head(
            &key.verifying_key(),
            &sig,
            "audit_checkpoint_v1",
            "k1",
            &sample_head()
        ));
    }

    #[test]
    fn a_changed_tree_size_does_not_verify() {
        let key = generate_signing_key();
        let sig = sign_tree_head(&key, "audit_checkpoint_v1", "k1", &sample_head());
        let mut tampered = sample_head();
        tampered.tree_size = 43;
        assert!(!verify_tree_head(
            &key.verifying_key(),
            &sig,
            "audit_checkpoint_v1",
            "k1",
            &tampered
        ));
    }

    #[test]
    fn a_different_key_does_not_verify() {
        let signer = generate_signing_key();
        let other = generate_signing_key();
        let sig = sign_tree_head(&signer, "audit_checkpoint_v1", "k1", &sample_head());
        assert!(!verify_tree_head(
            &other.verifying_key(),
            &sig,
            "audit_checkpoint_v1",
            "k1",
            &sample_head()
        ));
    }

    #[test]
    fn purpose_and_key_id_are_bound_into_the_signature() {
        let key = generate_signing_key();
        let sig = sign_tree_head(&key, "audit_checkpoint_v1", "k1", &sample_head());
        let pk = key.verifying_key();
        assert!(!verify_tree_head(
            &pk,
            &sig,
            "evidence_pack_v1",
            "k1",
            &sample_head()
        ));
        assert!(!verify_tree_head(
            &pk,
            &sig,
            "audit_checkpoint_v1",
            "k2",
            &sample_head()
        ));
    }

    #[test]
    fn public_key_and_signature_text_round_trips() {
        let key = generate_signing_key();
        let sig = sign_tree_head(&key, "audit_checkpoint_v1", "k1", &sample_head());
        let pk_text = render_public_key(&key.verifying_key());
        let sig_text = render_signature(&sig);
        assert!(pk_text.starts_with(PUBLIC_KEY_PREFIX));
        assert!(sig_text.starts_with(SIGNATURE_PREFIX));
        let pk = parse_public_key(&pk_text).unwrap();
        let parsed_sig = parse_signature(&sig_text).unwrap();
        assert!(verify_tree_head(
            &pk,
            &parsed_sig,
            "audit_checkpoint_v1",
            "k1",
            &sample_head()
        ));
    }

    #[test]
    fn pem_round_trips_the_signing_key() {
        let key = generate_signing_key();
        let pem = signing_key_to_pem(&key).unwrap();
        let restored = signing_key_from_pem(&pem).unwrap();
        assert_eq!(key.to_bytes(), restored.to_bytes());
    }

    #[test]
    fn a_non_hex_public_key_is_a_malformed_error() {
        assert!(parse_public_key("ed25519-pub:not-hex").is_err());
        assert!(parse_public_key("missing-prefix").is_err());
    }
}
