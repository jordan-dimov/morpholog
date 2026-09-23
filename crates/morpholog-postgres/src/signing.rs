//! Ed25519 signatures over audit tree heads.
//!
//! A checkpoint commits to a prefix of the audit log (see [`crate::merkle`],
//! [`crate::checkpoints`]). Signing its tree head makes an anchor held
//! outside *attributable*: tampering then needs the private key, not just
//! write access, and anyone can verify it against a known public key.
//!
//! The signature covers a typed, length-delimited, versioned payload (the
//! DSSE pre-authentication encoding idea), so it cannot be reinterpreted as
//! a signature over another format or kind of artefact.
//!
//! Pure: no I/O. The CLI reads key files.

use crate::merkle::Digest;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pkcs8::LineEnding;

/// The payload type bound into every audit-tree-head signature.
const TREE_HEAD_PAYLOAD_TYPE: &str = "application/vnd.morpholog.tree-head.v1";
/// The payload type an external witness (a timestamp authority) binds:
/// the head alone. No purpose or key id, since a witness speaks for no key,
/// and no signatures, which may lawfully be added later. Its own type
/// string keeps a witness digest apart from a signing payload.
const TREE_HEAD_WITNESS_PAYLOAD_TYPE: &str = "application/vnd.morpholog.tree-head-witness.v1";
/// Rendering prefixes, mirroring the `sha256:` convention on hashes.
const PUBLIC_KEY_PREFIX: &str = "ed25519-pub:";
const SIGNATURE_PREFIX: &str = "ed25519-sig:";

/// A malformed key or signature. A signature that parses but does not
/// verify is a verdict, not this error.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
}

/// The tree-head fields a signature commits to, borrowed from a
/// [`crate::Checkpoint`].
pub struct TreeHead<'a> {
    pub tree_size: i64,
    pub root_hash: &'a Digest,
    pub prev_checkpoint_hash: Option<&'a Digest>,
    pub checkpoint_hash: &'a Digest,
}

fn push_field(buf: &mut Vec<u8>, field: &[u8]) {
    buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
    buf.extend_from_slice(field);
}

/// The exact bytes a tree-head signature commits to. Length prefixes make
/// every field boundary unambiguous; the leading payload type stops a
/// signature for one kind of artefact being replayed as another.
pub fn tree_head_signing_bytes(purpose: &str, key_id: &str, head: &TreeHead<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    push_field(&mut b, TREE_HEAD_PAYLOAD_TYPE.as_bytes());
    push_field(&mut b, purpose.as_bytes());
    push_field(&mut b, key_id.as_bytes());
    push_field(&mut b, &head.tree_size.to_le_bytes());
    push_field(&mut b, head.root_hash.to_string().as_bytes());
    match head.prev_checkpoint_hash {
        Some(prev) => {
            b.push(1);
            push_field(&mut b, prev.to_string().as_bytes());
        }
        None => b.push(0),
    }
    push_field(&mut b, head.checkpoint_hash.to_string().as_bytes());
    b
}

/// The bytes an external witness commits to: the head encoded as
/// [`tree_head_signing_bytes`] does, minus the signing-only fields. A
/// timestamp authority receives their SHA-256 as its message imprint.
/// Frozen by test: if this encoding changes, stored proofs stop verifying.
pub fn tree_head_witness_bytes(head: &TreeHead<'_>) -> Vec<u8> {
    let mut b = Vec::new();
    push_field(&mut b, TREE_HEAD_WITNESS_PAYLOAD_TYPE.as_bytes());
    push_field(&mut b, &head.tree_size.to_le_bytes());
    push_field(&mut b, head.root_hash.to_string().as_bytes());
    match head.prev_checkpoint_hash {
        Some(prev) => {
            b.push(1);
            push_field(&mut b, prev.to_string().as_bytes());
        }
        None => b.push(0),
    }
    push_field(&mut b, head.checkpoint_hash.to_string().as_bytes());
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
    // The `CryptoRng` bound refuses a generator not marked as secure,
    // since nothing downstream could detect a predictable key. It does not
    // prove good seeding (a fixed-seed ChaCha is `CryptoRng` too); the call
    // site below supplies the entropy.
    fn fill_from_csprng(rng: &mut impl rand::CryptoRng, seed: &mut [u8; 32]) {
        rng.fill_bytes(seed);
    }
    let mut seed = [0u8; 32];
    fill_from_csprng(&mut rand::rng(), &mut seed);
    SigningKey::from_bytes(&seed)
}

/// Render a private key as a PKCS#8 PEM document, the key file format
/// (`openssl` reads it).
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
    format!("{PUBLIC_KEY_PREFIX}{}", hex::encode(key.to_bytes()))
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
    format!("{SIGNATURE_PREFIX}{}", hex::encode(sig.to_bytes()))
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
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).map_err(|_| SigningError::Malformed {
        what,
        detail: format!("expected {N} hex-encoded bytes"),
    })?;
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

    static ONES: std::sync::LazyLock<Digest> =
        std::sync::LazyLock::new(|| format!("sha256:{}", "1".repeat(64)).parse().unwrap());
    static TWOS: std::sync::LazyLock<Digest> =
        std::sync::LazyLock::new(|| format!("sha256:{}", "2".repeat(64)).parse().unwrap());
    static THREES: std::sync::LazyLock<Digest> =
        std::sync::LazyLock::new(|| format!("sha256:{}", "3".repeat(64)).parse().unwrap());

    fn sample_head() -> TreeHead<'static> {
        TreeHead {
            tree_size: 42,
            root_hash: &ONES,
            prev_checkpoint_hash: None,
            checkpoint_hash: &TWOS,
        }
    }

    /// A signing key from a fixed seed. Deterministic on purpose: the frozen
    /// tests below are meaningless with a random key.
    fn fixed_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// The exact bytes a tree-head signature commits to, frozen.
    ///
    /// The round-trip tests move both sides together, so they would stay
    /// green if this encoding changed and every stored signature broke.
    #[test]
    fn frozen_signing_input_pins_the_payload_encoding() {
        let bytes = tree_head_signing_bytes("audit_checkpoint_v1", "k1", &sample_head());
        let rendered = hex::encode(bytes);
        assert_eq!(
            rendered,
            "26000000000000006170706c69636174696f6e2f766e642e6d6f7270686f6c6f672e747265652d686561642e7631130000000000000061756469745f636865636b706f696e745f763102000000000000006b3108000000000000002a0000000000000047000000000000007368613235363a313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131310047000000000000007368613235363a32323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232"
        );
    }

    /// The signature itself, frozen against a fixed key and head.
    ///
    /// Ed25519 is deterministic (RFC 8032), so any correct implementation
    /// gives this value. If it moves, stored signatures have stopped
    /// verifying.
    #[test]
    fn frozen_signature_pins_ed25519_over_the_payload() {
        let sig = sign_tree_head(&fixed_key(), "audit_checkpoint_v1", "k1", &sample_head());
        assert_eq!(
            render_signature(&sig),
            "ed25519-sig:1322b926f5a5f75159599bf3060a52bca152123b80d4dbdfdcbc37eb2733912edc573934b96f34cd0ec26a820e6a1e8ec48300e760b6a5fcaad6437a89196f08"
        );
    }

    /// The public key that seed yields, so a change in key derivation is
    /// told apart from a change in signing.
    #[test]
    fn frozen_public_key_pins_the_seed_derivation() {
        assert_eq!(
            render_public_key(&fixed_key().verifying_key()),
            "ed25519-pub:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"
        );
    }

    // Artefacts written by ed25519-dalek 2 and pkcs8 0.10, captured from
    // that code rather than rendered here. A same-version round trip cannot
    // show that files an older writer left on disk still load.
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
    /// checkpoint's hash is present, the other branch of the encoding.
    fn chained_head() -> TreeHead<'static> {
        TreeHead {
            prev_checkpoint_hash: Some(&THREES),
            ..sample_head()
        }
    }

    /// A key file written by the older libraries still loads, as the same
    /// key. Same-version tests would miss a parser that rejects an
    /// operator's existing key file.
    #[test]
    fn a_pre_upgrade_pem_still_loads_as_the_same_key() {
        let key = signing_key_from_pem(PRE_UPGRADE_PRIVATE_KEY_PEM)
            .expect("a key file from the previous release must still load");
        assert_eq!(
            render_public_key(&key.verifying_key()),
            PRE_UPGRADE_PUBLIC_KEY
        );
        // Writing it back yields the same file, readable by both versions.
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

    /// This version reproduces them byte for byte, so a re-signed checkpoint
    /// matches one signed with the older libraries.
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

    /// The chained payload encoding, frozen: the `Some` branch's presence
    /// byte, length prefix and position, which `sample_head` does not reach.
    #[test]
    fn frozen_chained_signing_input_pins_the_other_branch() {
        let bytes = tree_head_signing_bytes("audit_checkpoint_v1", "k1", &chained_head());
        assert_eq!(hex::encode(bytes), PRE_UPGRADE_INPUT_CHAINED);
    }

    /// The witness payload, frozen in both branches. The expected bytes were
    /// built by hand from the encoding's definition, not by this code.
    #[test]
    fn frozen_witness_payload_pins_both_branches() {
        assert_eq!(
            hex::encode(tree_head_witness_bytes(&sample_head())),
            "2e000000000000006170706c69636174696f6e2f766e642e6d6f7270686f6c6f672e747265652d686561642d7769746e6573732e763108000000000000002a0000000000000047000000000000007368613235363a313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131310047000000000000007368613235363a32323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232"
        );
        let chained = TreeHead {
            tree_size: 43,
            root_hash: &ONES,
            prev_checkpoint_hash: Some(&TWOS),
            checkpoint_hash: &THREES,
        };
        assert_eq!(
            hex::encode(tree_head_witness_bytes(&chained)),
            "2e000000000000006170706c69636174696f6e2f766e642e6d6f7270686f6c6f672e747265652d686561642d7769746e6573732e763108000000000000002b0000000000000047000000000000007368613235363a313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131313131310147000000000000007368613235363a3232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323232323247000000000000007368613235363a33333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333333"
        );
        // Domain separation: the witness bytes over a head never equal the
        // signing bytes over the same head under any purpose or key id.
        assert_ne!(
            tree_head_witness_bytes(&sample_head()),
            tree_head_signing_bytes("audit_checkpoint_v1", "k1", &sample_head())
        );
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
