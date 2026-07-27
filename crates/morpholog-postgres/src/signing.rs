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
    // `fill_bytes` alone would accept any generator, so the bound
    // rejects one not designated cryptographically secure - a real
    // guard, since nothing downstream could detect a predictable
    // signing key. It is not a proof of unpredictable seeding: a
    // ChaCha built from a fixed seed satisfies `CryptoRng` too, so the
    // entropy guarantee stays with the call site below.
    fn fill_from_csprng(rng: &mut impl rand::CryptoRng, seed: &mut [u8; 32]) {
        rng.fill_bytes(seed);
    }
    let mut seed = [0u8; 32];
    fill_from_csprng(&mut rand::rng(), &mut seed);
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
    format!("{PUBLIC_KEY_PREFIX}{}", crate::hex::encode(&key.to_bytes()))
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
    format!("{SIGNATURE_PREFIX}{}", crate::hex::encode(&sig.to_bytes()))
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

fn from_hex<const N: usize>(s: &str, what: &'static str) -> Result<[u8; N], SigningError> {
    if s.len() != N * 2 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(SigningError::Malformed {
            what,
            detail: format!("expected {N} hex-encoded bytes"),
        });
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot =
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| SigningError::Malformed {
                what,
                detail: e.to_string(),
            })?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each malformation alone is refused: a wrong length with clean
    /// hex digits, and a clean length with a non-hex byte.
    #[test]
    fn hex_length_and_digit_checks_each_bite_alone() {
        assert!(parse_public_key(&format!("ed25519-pub:{}", "ab".repeat(31))).is_err());
        let non_hex = format!("ed25519-pub:{}zz", "ab".repeat(31));
        assert!(parse_public_key(&non_hex).is_err());
    }

    fn sample_head() -> TreeHead<'static> {
        TreeHead {
            tree_size: 42,
            root_hash: "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            prev_checkpoint_hash: None,
            checkpoint_hash: "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        }
    }

    /// A signing key from a fixed seed. Deterministic on purpose: the frozen
    /// tests below are meaningless with a random key.
    fn fixed_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// The exact bytes a tree-head signature commits to, frozen.
    ///
    /// Every other test here round-trips - sign then verify, with both sides
    /// moving together - so a change to this encoding would leave all of them
    /// green while every signature already stored stopped verifying. This is
    /// the test that would notice.
    #[test]
    fn frozen_signing_input_pins_the_payload_encoding() {
        let bytes = tree_head_signing_bytes("audit_checkpoint_v1", "k1", &sample_head());
        let rendered = crate::hex::encode(&bytes);
        assert_eq!(
            rendered,
            "26000000000000006170706c69636174696f6e2f766e642e6d6f7270686f6c6f672e747265652d686561642e7631130000000000000061756469745f636865636b706f696e745f763102000000000000006b3108000000000000002a0000000000000047000000000000007368613235363a313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131310047000000000000007368613235363a32323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232"
        );
    }

    /// The signature itself, frozen against a fixed key and head.
    ///
    /// Ed25519 is deterministic (RFC 8032), so this value is stable across
    /// any correct implementation - which is what makes it the right pin for
    /// a dependency bump. If it moves, signatures in existing databases have
    /// stopped verifying.
    #[test]
    fn frozen_signature_pins_ed25519_over_the_payload() {
        let sig = sign_tree_head(&fixed_key(), "audit_checkpoint_v1", "k1", &sample_head());
        assert_eq!(
            render_signature(&sig),
            "ed25519-sig:1322b926f5a5f75159599bf3060a52bca152123b80d4dbdfdcbc37eb2733912edc573934b96f34cd0ec26a820e6a1e8ec48300e760b6a5fcaad6437a89196f08"
        );
    }

    /// The public key that seed yields, so a change in key derivation is
    /// caught as well - the signature pin alone would move for either reason
    /// and could not tell them apart.
    #[test]
    fn frozen_public_key_pins_the_seed_derivation() {
        assert_eq!(
            render_public_key(&fixed_key().verifying_key()),
            "ed25519-pub:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
    }

    // Artefacts produced by the PREVIOUS release - ed25519-dalek 2, pkcs8
    // 0.10 - captured by running the emitter against that code, not by
    // rendering them here. A same-version round trip cannot tell you that a
    // parser still accepts what an older writer left on disk, which is the
    // only question an upgrade actually asks.
    const PRE_UPGRADE_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
        MFECAQEwBQYDK2VwBCIEIAcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcH\n\
        gSEA6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=\n\
        -----END PRIVATE KEY-----\n";
    const PRE_UPGRADE_PUBLIC_KEY: &str =
        "ed25519-pub:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const PRE_UPGRADE_SIGNATURE: &str = "ed25519-sig:1322b926f5a5f75159599bf3060a52bca152123b80d4dbdfdcbc37eb2733912edc573934b96f34cd0ec26a820e6a1e8ec48300e760b6a5fcaad6437a89196f08";
    /// The chained form: every checkpoint after the first signs over the
    /// previous checkpoint's hash, which is a different encoding branch.
    const PRE_UPGRADE_SIGNATURE_CHAINED: &str = "ed25519-sig:99f8cb383dc5ed60192b284f3df8d5eeca05edb0660cb636a107a60ad94f68a7e5054f0ea6106b47cd35fc58aa8cdd8abe90ff2d6740970f1595e8b11e0cea09";
    const PRE_UPGRADE_INPUT_CHAINED: &str = "26000000000000006170706c69636174696f6e2f766e642e6d6f7270686f6c6f672e747265652d686561642e7631130000000000000061756469745f636865636b706f696e745f763102000000000000006b3108000000000000002a0000000000000047000000000000007368613235363a313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131310147000000000000007368613235363a3333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333347000000000000007368613235363a32323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232";

    /// The head every checkpoint after the first signs: the previous
    /// checkpoint's hash is present, which is the other branch of the
    /// payload encoding and the one an enduring chain actually uses.
    fn chained_head() -> TreeHead<'static> {
        TreeHead {
            prev_checkpoint_hash: Some(
                "sha256:3333333333333333333333333333333333333333333333333333333333333333",
            ),
            ..sample_head()
        }
    }

    /// A signing key written by the previous release still loads, and is the
    /// same key.
    ///
    /// PKCS#8 is a third of this upgrade and had nothing pinned: every key
    /// test wrote and read with the same version, so a parser regression
    /// would have rejected an operator's existing key file while every test
    /// stayed green.
    #[test]
    fn a_pre_upgrade_pem_still_loads_as_the_same_key() {
        let key = signing_key_from_pem(PRE_UPGRADE_PRIVATE_KEY_PEM)
            .expect("a key file from the previous release must still load");
        assert_eq!(
            render_public_key(&key.verifying_key()),
            PRE_UPGRADE_PUBLIC_KEY
        );
        // And writing it back out yields what the old release wrote, so a
        // key round-tripped through this version stays readable by both.
        assert_eq!(
            signing_key_to_pem(&key).unwrap(),
            PRE_UPGRADE_PRIVATE_KEY_PEM
        );
    }

    /// Signatures already stored in checkpoints still verify - both the
    /// first-checkpoint form and the chained form.
    #[test]
    fn pre_upgrade_signatures_still_verify() {
        let public_key = parse_public_key(PRE_UPGRADE_PUBLIC_KEY).expect("stored key parses");
        for (label, sig_text, head) in [
            ("unchained", PRE_UPGRADE_SIGNATURE, sample_head()),
            ("chained", PRE_UPGRADE_SIGNATURE_CHAINED, chained_head()),
        ] {
            let signature = parse_signature(sig_text).expect("stored signature parses");
            assert!(
                verify_tree_head(&public_key, &signature, "audit_checkpoint_v1", "k1", &head),
                "a {label} signature from the previous release must still verify"
            );
        }
    }

    /// And this version reproduces them byte for byte, so a checkpoint
    /// re-signed after the upgrade is indistinguishable from one signed
    /// before it.
    #[test]
    fn this_version_reproduces_the_pre_upgrade_signatures() {
        let key = signing_key_from_pem(PRE_UPGRADE_PRIVATE_KEY_PEM).unwrap();
        assert_eq!(
            render_signature(&sign_tree_head(
                &key,
                "audit_checkpoint_v1",
                "k1",
                &sample_head()
            )),
            PRE_UPGRADE_SIGNATURE
        );
        assert_eq!(
            render_signature(&sign_tree_head(
                &key,
                "audit_checkpoint_v1",
                "k1",
                &chained_head()
            )),
            PRE_UPGRADE_SIGNATURE_CHAINED
        );
    }

    /// The chained payload encoding, frozen. `sample_head` leaves the
    /// previous hash absent, so the frozen vector above covers only the
    /// `None` marker - this one covers the `Some` branch: its presence byte,
    /// length prefix and position.
    #[test]
    fn frozen_chained_signing_input_pins_the_other_branch() {
        let bytes = tree_head_signing_bytes("audit_checkpoint_v1", "k1", &chained_head());
        assert_eq!(crate::hex::encode(&bytes), PRE_UPGRADE_INPUT_CHAINED);
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
