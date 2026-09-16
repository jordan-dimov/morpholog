//! The verifier over real tokens, recorded from public authorities over the
//! frozen witness payloads (genesis and chained heads) with OpenSSL as the
//! ground truth at recording time. Attacker capability modelled: whoever
//! can write the checkpoints table can plant, edit, or re-home a proof;
//! whoever can propose can bring their own authority. The verifier's job is
//! to judge what it can and to say plainly what it cannot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{TimeZone, Utc};
use morpholog_witness::{
    Anchors, Refusal, WitnessStatus, build_request, check_response, verify_rfc3161,
};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rfc3161")
            .join(name),
    )
    .unwrap()
}

fn digicert_anchors() -> Anchors {
    Anchors::from_pem(&fixture("digicert_chain.pem")).unwrap()
}

#[test]
fn a_digicert_token_over_the_genesis_head_verifies_against_its_chain() {
    let status = verify_rfc3161(
        &fixture("genesis_digicert.tsr"),
        &fixture("genesis_payload.bin"),
        Some(&digicert_anchors()),
    );
    assert_eq!(
        status,
        WitnessStatus::Verified {
            attested_at: Utc.with_ymd_and_hms(2026, 9, 16, 10, 20, 5).unwrap(),
        }
    );
}

#[test]
fn the_chained_head_has_its_own_token_and_the_two_do_not_cross() {
    let anchors = digicert_anchors();
    assert!(matches!(
        verify_rfc3161(
            &fixture("chained_digicert.tsr"),
            &fixture("chained_payload.bin"),
            Some(&anchors)
        ),
        WitnessStatus::Verified { .. }
    ));
    // A proof re-homed onto another head is judged and refused.
    assert!(matches!(
        verify_rfc3161(
            &fixture("chained_digicert.tsr"),
            &fixture("genesis_payload.bin"),
            Some(&anchors)
        ),
        WitnessStatus::Invalid { detail } if detail.contains("not over this tree head")
    ));
}

#[test]
fn trust_is_the_verifiers_and_the_verdict_says_which_material_it_had() {
    let proof = fixture("genesis_digicert.tsr");
    let payload = fixture("genesis_payload.bin");
    assert!(matches!(
        verify_rfc3161(&proof, &payload, None),
        WitnessStatus::Unverified { .. }
    ));
    let unrelated = Anchors::from_pem(&fixture("unrelated.pem")).unwrap();
    assert!(matches!(
        verify_rfc3161(&proof, &payload, Some(&unrelated)),
        WitnessStatus::Untrusted { .. }
    ));
    // Pinning the exact leaf is an anchor too: the first certificate in
    // the carried chain is the signer.
    let chain = String::from_utf8(fixture("digicert_chain.pem")).unwrap();
    let leaf_end =
        chain.find("-----END CERTIFICATE-----").unwrap() + "-----END CERTIFICATE-----".len();
    let leaf = Anchors::from_pem(&chain.as_bytes()[..leaf_end]).unwrap();
    assert!(matches!(
        verify_rfc3161(&proof, &payload, Some(&leaf)),
        WitnessStatus::Verified { .. }
    ));
}

#[test]
fn a_tampered_token_is_invalid_not_unsupported() {
    let mut proof = fixture("genesis_digicert.tsr");
    let last = proof.len() - 1;
    proof[last] ^= 0x01; // inside the signature bytes at the tail
    assert!(matches!(
        verify_rfc3161(
            &proof,
            &fixture("genesis_payload.bin"),
            Some(&digicert_anchors())
        ),
        WitnessStatus::Invalid { .. }
    ));
    assert!(matches!(
        verify_rfc3161(b"not a token", &fixture("genesis_payload.bin"), None),
        WitnessStatus::Invalid { .. }
    ));
}

/// The negative control that guards the verifier's honesty: a genuine
/// FreeTSA token, granted, over this head, nonce echoed, signed with ECDSA
/// P-384 and SHA-512 - a primitive this implementation lacks. It is
/// `unsupported`, never `invalid`, with or without trust material.
#[test]
fn a_genuine_token_this_implementation_cannot_judge_is_unsupported_never_invalid() {
    let proof = fixture("genesis_freetsa.tsr");
    let payload = fixture("genesis_payload.bin");
    let freetsa = Anchors::from_pem(&fixture("freetsa_ca.pem")).unwrap();
    for anchors in [None, Some(&freetsa)] {
        assert!(matches!(
            verify_rfc3161(&proof, &payload, anchors),
            WitnessStatus::Unsupported { detail } if detail.contains("algorithm")
        ));
    }
}

#[test]
fn a_request_carries_a_sha256_imprint_and_a_fresh_nonce() {
    let a = build_request(b"head");
    let b = build_request(b"head");
    assert_ne!(a.der, b.der, "two requests over one head differ in nonce");
    assert!(a.der.len() > 40);
}

/// The self-check that decides storage runs every verifier check plus the
/// nonce; the recorded request and response prove the happy path, and a
/// response to a different request is refused on its nonce.
#[test]
fn check_response_accepts_the_recorded_pair_and_refuses_a_foreign_nonce() {
    // The recorded request's DER, wrapped as the Request the builder would
    // have produced: its nonce is the INTEGER the file carries.
    let recorded =
        morpholog_witness::testing::request_from_der(&fixture("genesis_digicert.tsq")).unwrap();
    let checked = check_response(
        &recorded,
        &fixture("genesis_digicert.tsr"),
        &fixture("genesis_payload.bin"),
    )
    .unwrap();
    assert_eq!(
        checked.attested_at,
        Utc.with_ymd_and_hms(2026, 9, 16, 10, 20, 5).unwrap()
    );

    let other =
        morpholog_witness::testing::request_from_der(&fixture("chained_digicert.tsq")).unwrap();
    assert!(matches!(
        check_response(&other, &fixture("genesis_digicert.tsr"), &fixture("genesis_payload.bin")),
        Err(Refusal::Invalid { detail }) if detail.contains("nonce")
    ));
    // An unsupported signer is a refusal the caller may still store.
    let free =
        morpholog_witness::testing::request_from_der(&fixture("genesis_freetsa.tsq")).unwrap();
    assert!(matches!(
        check_response(
            &free,
            &fixture("genesis_freetsa.tsr"),
            &fixture("genesis_payload.bin")
        ),
        Err(Refusal::Unsupported { .. })
    ));
}
