//! External witnesses to an audit tree head, as RFC 3161 timestamp tokens.
//!
//! A witness proves a commitment existed by some time, vouched for by an authority the log
//! operator does not control. This crate builds the request, self-checks the response before
//! it is stored, and verifies a stored proof offline against anchors the verifier chose.
//!
//! A proof signed with an algorithm this crate lacks is reported as `unsupported`, never as
//! `invalid`. Nothing here touches the network: the caller posts the request bytes and hands
//! back the response bytes.

use std::ops::Deref as _;

use bcder::decode::Constructed;
use bcder::encode::Values;
use bcder::{BitString, Integer, Mode, OctetString, Oid};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use cryptographic_message_syntax::asn1::rfc3161::{
    MessageImprint, OID_CONTENT_TYPE_TST_INFO, PkiStatus, TimeStampReq, TimeStampResp, TstInfo,
};
use cryptographic_message_syntax::asn1::rfc5652::{
    OID_ID_SIGNED_DATA, SignedData as Asn1SignedData, SignerInfo as Asn1SignerInfo,
};
use cryptographic_message_syntax::{CmsError, SignedData, SignerInfo};
use jiff::Timestamp;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use x509_certificate::rfc5280::{AlgorithmIdentifier, Extension};
use x509_certificate::{
    CapturedX509Certificate, KeyAlgorithm, SignatureAlgorithm, X509CertificateError,
};

/// The media type of a request body, RFC 3161 section 3.4.
pub const REQUEST_CONTENT_TYPE: &str = "application/timestamp-query";
/// The media type of a response body.
pub const REPLY_CONTENT_TYPE: &str = "application/timestamp-reply";

const OID_SHA1: &[u8] = &[0x2b, 0x0e, 0x03, 0x02, 0x1a];
const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_SHA384: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02];
const OID_SHA512: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03];
const OID_KEY_USAGE: &str = "2.5.29.15";
const OID_BASIC_CONSTRAINTS: &str = "2.5.29.19";
const OID_EXTENDED_KEY_USAGE: &str = "2.5.29.37";
const OID_KP_TIMESTAMPING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x08];
/// id-aa-signingCertificate (RFC 2634) and -V2 (RFC 5035): the signed
/// attribute naming the certificate that signed, by hash.
const OID_AA_SIGNING_CERTIFICATE: &[u8] = &[
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x10, 0x02, 0x0c,
];
const OID_AA_SIGNING_CERTIFICATE_V2: &[u8] = &[
    0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x09, 0x10, 0x02, 0x2f,
];
/// ecdsa-with-SHA512 (RFC 5758 section 3.2), which the CMS crate cannot parse, and its
/// SHA-384 sibling, used only as a stand-in so it can parse the rest of such a token.
const OID_ECDSA_WITH_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
const OID_ECDSA_WITH_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
/// id-ecPublicKey and the named curves the SHA-512 fallback verifies on (RFC 5480).
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
/// keyUsage bits, RFC 5280 section 4.2.1.3.
const KU_DIGITAL_SIGNATURE: usize = 0;
const KU_NON_REPUDIATION: usize = 1;
const KU_KEY_CERT_SIGN: usize = 5;
/// Certificates a path may pass through between the signer and an anchor.
const MAX_PATH_LENGTH: usize = 6;

#[derive(Debug, thiserror::Error)]
pub enum WitnessError {
    #[error("{0}")]
    Anchors(String),
}

/// The certificates a verifier deliberately trusts.
///
/// A signer is accepted if it is an anchor, or if a certification path from it to an anchor
/// validates at the attested time (RFC 5280 section 6.1). Revocation and certificate policies
/// are not checked; a verifier that needs them picks its anchors accordingly. A pinned leaf is
/// the narrowest trust and breaks on key rotation; an intermediate or root allows rotation.
pub struct Anchors(Vec<CapturedX509Certificate>);

impl Anchors {
    /// Every certificate in a PEM bundle.
    pub fn from_pem(bundle: &[u8]) -> Result<Self, WitnessError> {
        let certs = CapturedX509Certificate::from_pem_multiple(bundle)
            .map_err(|e| WitnessError::Anchors(e.to_string()))?;
        if certs.is_empty() {
            return Err(WitnessError::Anchors(
                "the bundle holds no certificate".to_string(),
            ));
        }
        Ok(Self(certs))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What a verifier can say about one stored proof. Only `Invalid` is a
/// judgement against the proof; the rest describe what this verifier, with
/// the trust material it was given, could establish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitnessStatus {
    /// Genuine, over this head, and signed by an anchor or a chain to one.
    Verified { attested_at: Timestamp },
    /// Genuine and over this head, but no certification path from the
    /// signer to a supplied anchor validates; `reason` says where it
    /// failed.
    Untrusted {
        attested_at: Timestamp,
        signer: String,
        reason: String,
    },
    /// Genuine and over this head; no trust material was supplied, so
    /// nothing is said about the signer.
    Unverified { attested_at: Timestamp },
    /// A primitive this implementation lacks stands between the verifier
    /// and a judgement. Not a verdict on the proof.
    Unsupported { detail: String },
    /// Judged, and wrong: malformed, refused by the authority, not over
    /// this head, or a signature that does not verify.
    Invalid { detail: String },
}

/// Verify a stored RFC 3161 response over the head's witness payload,
/// against the anchors the verifier chose (`None`: no trust material).
pub fn verify_rfc3161(proof: &[u8], payload: &[u8], anchors: Option<&Anchors>) -> WitnessStatus {
    let examined = match examine(proof, payload, None) {
        Ok(e) => e,
        Err(status) => return status,
    };
    let Some(anchors) = anchors else {
        return WitnessStatus::Unverified {
            attested_at: examined.attested_at,
        };
    };
    match validate_path(
        &examined.signer,
        &examined.carried,
        anchors,
        examined.validity_at,
    ) {
        Ok(()) => WitnessStatus::Verified {
            attested_at: examined.attested_at,
        },
        Err(PathFailure::Untrusted(reason)) => WitnessStatus::Untrusted {
            attested_at: examined.attested_at,
            signer: common_name(&examined.signer),
            reason,
        },
        Err(PathFailure::Unsupported(detail)) => WitnessStatus::Unsupported { detail },
    }
}

fn common_name(cert: &CapturedX509Certificate) -> String {
    cert.subject_common_name()
        .unwrap_or_else(|| "<no common name>".to_string())
}

/// A request awaiting an authority's answer: the DER to post and the
/// nonce the answer must echo.
pub struct Request {
    pub der: Vec<u8>,
    nonce: Integer,
}

/// Build a request over the head's witness payload: a SHA-256 imprint,
/// a fresh nonce, and the signer certificate asked for.
pub fn build_request(payload: &[u8]) -> Request {
    let nonce = Integer::from(rand::random::<u64>() >> 1);
    let req = TimeStampReq {
        version: Integer::from(1_u8),
        message_imprint: MessageImprint {
            hash_algorithm: AlgorithmIdentifier {
                algorithm: Oid(Bytes::from_static(OID_SHA256)),
                parameters: None,
            },
            hashed_message: OctetString::new(Bytes::copy_from_slice(&Sha256::digest(payload))),
        },
        req_policy: None,
        nonce: Some(nonce.clone()),
        cert_req: Some(true),
        extensions: None,
    };
    Request {
        der: req
            .encode_ref()
            .to_captured(Mode::Der)
            .into_bytes()
            .to_vec(),
        nonce,
    }
}

/// Why a response was not accepted for storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Judged and wrong; never store it.
    Invalid { detail: String },
    /// Structurally a response over this head, but its signature uses a
    /// primitive this implementation lacks. Storing it is the caller's
    /// call: a later build may verify it, and it must never be reported
    /// as verified now.
    Unsupported { detail: String },
}

/// What a self-checked response established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub attested_at: Timestamp,
}

/// Self-check an authority's response before storing it: everything
/// `verify_rfc3161` checks, plus that the nonce echoes the request. Trust
/// is not judged here - it belongs to the verifier and its anchors.
pub fn check_response(
    request: &Request,
    response: &[u8],
    payload: &[u8],
) -> Result<Checked, Refusal> {
    match examine(response, payload, Some(&request.nonce)) {
        Ok(e) => Ok(Checked {
            attested_at: e.attested_at,
        }),
        Err(WitnessStatus::Unsupported { detail }) => Err(Refusal::Unsupported { detail }),
        Err(WitnessStatus::Invalid { detail }) => Err(Refusal::Invalid { detail }),
        Err(other) => Err(Refusal::Invalid {
            detail: format!("unexpected examination result {other:?}"),
        }),
    }
}

struct Examined {
    attested_at: Timestamp,
    /// The same instant in the certificate library's clock type, for
    /// judging certificate validity.
    validity_at: DateTime<Utc>,
    signer: CapturedX509Certificate,
    carried: Vec<CapturedX509Certificate>,
}

fn invalid(detail: impl Into<String>) -> WitnessStatus {
    WitnessStatus::Invalid {
        detail: detail.into(),
    }
}

/// Everything about a response that needs no trust material: decode,
/// status, content, imprint, nonce, CMS signature and digest, the signer
/// certificate and its timestamping key usage, and the attested time.
fn examine(
    response: &[u8],
    payload: &[u8],
    nonce: Option<&Integer>,
) -> Result<Examined, WitnessStatus> {
    // BER, not DER: authorities answer in either, and the bytes are kept
    // as received, so nothing here re-encodes.
    let resp = Constructed::decode(response, Mode::Ber, TimeStampResp::take_from)
        .map_err(|e| invalid(format!("not an RFC 3161 response: {e}")))?;
    if !matches!(
        resp.status.status,
        PkiStatus::Granted | PkiStatus::GrantedWithMods
    ) {
        return Err(invalid(format!(
            "the authority did not grant the request (status {:?})",
            resp.status.status
        )));
    }
    let token = resp
        .time_stamp_token
        .as_ref()
        .ok_or_else(|| invalid("granted, but no token in the response"))?;
    if token.content_type != OID_ID_SIGNED_DATA {
        return Err(invalid("the token is not CMS SignedData"));
    }
    let asn1_sd: Asn1SignedData = token
        .content
        .clone()
        .decode(Asn1SignedData::take_from)
        .map_err(|e| invalid(format!("malformed SignedData: {e}")))?;
    if asn1_sd.content_info.content_type != OID_CONTENT_TYPE_TST_INFO {
        return Err(invalid("the signed content is not a TSTInfo"));
    }
    let tst_bytes = asn1_sd
        .content_info
        .content
        .as_ref()
        .ok_or_else(|| invalid("the TSTInfo is absent"))?
        .to_bytes();
    let tst: TstInfo = Constructed::decode(tst_bytes, Mode::Ber, TstInfo::take_from)
        .map_err(|e| invalid(format!("malformed TSTInfo: {e}")))?;

    // The request asked for a SHA-256 imprint, so any other algorithm is
    // not an answer to it - judged, not unsupported, or a token over
    // another head could be stored unchecked.
    if tst.message_imprint.hash_algorithm.algorithm.as_ref() != OID_SHA256 {
        return Err(invalid(format!(
            "imprint algorithm {} is not the SHA-256 the request asked for",
            tst.message_imprint.hash_algorithm.algorithm
        )));
    }
    if tst.message_imprint.hashed_message.to_bytes().as_ref() != Sha256::digest(payload).as_slice()
    {
        return Err(invalid("the imprint is not over this tree head"));
    }
    if let Some(expected) = nonce {
        let echoed = tst
            .nonce
            .as_ref()
            .ok_or_else(|| invalid("the response echoes no nonce"))?;
        if echoed != expected {
            return Err(invalid("the response's nonce is not the request's"));
        }
    }
    let validity_at: DateTime<Utc> = tst.gen_time.clone().into();
    let attested_at = Timestamp::new(
        validity_at.timestamp(),
        i32::try_from(validity_at.timestamp_subsec_nanos()).unwrap_or(i32::MAX),
    )
    .map_err(|_| WitnessStatus::Unsupported {
        detail: format!("the token's time `{validity_at}` is not a representable instant"),
    })?;

    let route = signature_route(&asn1_sd);
    let sd = match route {
        SignatureRoute::Cms => SignedData::try_from(&asn1_sd),
        SignatureRoute::EcdsaSha512 => cms_view_without_signature_verification(&asn1_sd),
    }
    .map_err(classify_cms)?;
    // RFC 3161 section 2.4.2: the token carries the authority's signature
    // and no other.
    let mut signers = sd.signers();
    let signer_info = signers
        .next()
        .ok_or_else(|| invalid("the token has no signer"))?;
    if signers.next().is_some() {
        return Err(invalid(
            "the token carries more than one signature; a timestamp token carries only \
             the authority's",
        ));
    }
    if route == SignatureRoute::Cms {
        signer_info
            .verify_signature_with_signed_data(&sd)
            .map_err(classify_cms)?;
    }
    signer_info
        .verify_message_digest_with_signed_data(&sd)
        .map_err(classify_cms)?;
    // A subjectKeyIdentifier signer identifier is lawful CMS this
    // implementation does not resolve; not a judgement on the token.
    let (issuer, serial) =
        signer_info
            .certificate_issuer_and_serial()
            .ok_or_else(|| WitnessStatus::Unsupported {
                detail: "the signer is identified by subject key identifier, which this \
                         verifier does not resolve"
                    .to_string(),
            })?;
    let carried = carried_certificates(&token.content)?;
    let signer = carried
        .iter()
        .find(|c| c.issuer_name() == issuer && c.serial_number_asn1() == serial)
        .ok_or_else(|| invalid("the signer's certificate is not carried in the token"))?
        .clone();
    if route == SignatureRoute::EcdsaSha512 {
        // One signer, checked on the view above, so the raw one is it.
        let raw = asn1_sd
            .signer_infos
            .first()
            .ok_or_else(|| invalid("the token has no signer"))?;
        verify_ecdsa_sha512(&signer, raw)?;
    }
    check_signing_certificate(signer_info, &signer)?;
    if !has_exact_critical_timestamping_eku(&signer) {
        return Err(invalid(
            "the signer's certificate does not carry the critical timestamping extended key \
             usage as its only usage",
        ));
    }
    Ok(Examined {
        attested_at,
        validity_at,
        signer,
        carried,
    })
}

/// The certificates a token carries, each kept as the exact bytes the token holds and parsed
/// from them. The CMS crate re-encodes carried certificates, and its encoding is not always the
/// one signed (an ECDSA signature algorithm gains a NULL parameter), so a hash or signature
/// over its copy judges bytes nobody signed. Entries that are not X.509 certificates are
/// skipped, as the CMS crate skips them.
fn carried_certificates(
    signed_data: &bcder::Captured,
) -> Result<Vec<CapturedX509Certificate>, WitnessStatus> {
    let elements = signed_data
        .clone()
        .decode(|cons| {
            cons.take_sequence(|sd| {
                // version, digestAlgorithms, encapContentInfo
                for _ in 0..3 {
                    sd.capture_one()?;
                }
                let mut elements = Vec::new();
                sd.take_opt_constructed_if(bcder::Tag::CTX_0, |set| {
                    loop {
                        let element = set.capture(|c| c.skip_one().map(|_| ()))?;
                        if element.as_slice().is_empty() {
                            return Ok(());
                        }
                        elements.push(element);
                    }
                })?;
                sd.skip_all()?;
                Ok(elements)
            })
        })
        .map_err(|e| invalid(format!("malformed SignedData certificates: {e}")))?;
    Ok(elements
        .into_iter()
        .filter_map(|element| CapturedX509Certificate::from_ber(element.as_slice().to_vec()).ok())
        .collect())
}

/// Who judges a token's signature: the CMS crate, for every algorithm it knows, or this crate,
/// for ecdsa-with-SHA512, which the CMS crate cannot parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignatureRoute {
    Cms,
    EcdsaSha512,
}

fn signature_route(token: &Asn1SignedData) -> SignatureRoute {
    if token
        .signer_infos
        .iter()
        .any(|s| s.signature_algorithm.algorithm.as_ref() == OID_ECDSA_WITH_SHA512)
    {
        SignatureRoute::EcdsaSha512
    } else {
        SignatureRoute::Cms
    }
}

/// The CMS crate's reading of a token signed with ecdsa-with-SHA512, for every check but the
/// signature: each such signer's algorithm is replaced by ecdsa-with-SHA384 so the crate parses
/// it, and nothing else changes. The stand-in is not evidence. The view's signatures must never
/// be verified; `verify_ecdsa_sha512` judges the untouched raw signer instead.
fn cms_view_without_signature_verification(token: &Asn1SignedData) -> Result<SignedData, CmsError> {
    SignedData::try_from(&with_stand_in_algorithm(token))
}

/// `token` with each ecdsa-with-SHA512 signer's algorithm replaced by the stand-in.
fn with_stand_in_algorithm(token: &Asn1SignedData) -> Asn1SignedData {
    let mut view = token.clone();
    for signer in view.signer_infos.iter_mut() {
        if signer.signature_algorithm.algorithm.as_ref() == OID_ECDSA_WITH_SHA512 {
            signer.signature_algorithm.algorithm = Oid(Bytes::from_static(OID_ECDSA_WITH_SHA384));
        }
    }
    view
}

/// The signature of a raw signer whose algorithm is ecdsa-with-SHA512, over the exact bytes CMS
/// signs (the DER `SET OF` of its signed attributes), with the signer certificate's key.
fn verify_ecdsa_sha512(
    signer: &CapturedX509Certificate,
    raw: &Asn1SignerInfo,
) -> Result<(), WitnessStatus> {
    let signed = raw
        .signed_attributes_digested_content()
        .map_err(|e| invalid(format!("the signed attributes cannot be encoded: {e}")))?
        .ok_or_else(|| invalid("the signature covers no attributes"))?;
    let key = &signer.tbs_certificate().subject_public_key_info;
    if key.algorithm.algorithm.as_ref() != OID_EC_PUBLIC_KEY {
        return Err(invalid(
            "an ECDSA signature by a certificate whose key is not an elliptic-curve key",
        ));
    }
    let curve = key
        .algorithm
        .parameters
        .as_ref()
        .and_then(|p| p.decode_oid().ok())
        .ok_or_else(|| invalid("the signer's elliptic-curve key names no curve"))?;
    check_ecdsa_sha512(
        curve.as_ref(),
        &signer.public_key_data(),
        &Sha512::digest(&signed),
        raw.signature.to_bytes().as_ref(),
    )
}

/// ECDSA over a SHA-512 digest on P-256 or P-384, the digest truncated to the curve order's
/// length as FIPS 186 specifies. Another curve is `unsupported`; a malformed key or signature,
/// or one that does not verify, is `invalid`.
fn check_ecdsa_sha512(
    curve: &[u8],
    public_key: &[u8],
    digest: &[u8],
    der_signature: &[u8],
) -> Result<(), WitnessStatus> {
    use ecdsa::signature::hazmat::PrehashVerifier as _;
    let wrong = |what: &str| invalid(format!("the token's signature does not verify: {what}"));
    let verified = match curve {
        OID_P256 => {
            let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key)
                .map_err(|_| wrong("malformed P-256 key"))?;
            let signature = p256::ecdsa::Signature::from_der(der_signature)
                .map_err(|_| wrong("malformed signature"))?;
            key.verify_prehash(digest, &signature).is_ok()
        }
        OID_P384 => {
            let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(public_key)
                .map_err(|_| wrong("malformed P-384 key"))?;
            let signature = p384::ecdsa::Signature::from_der(der_signature)
                .map_err(|_| wrong("malformed signature"))?;
            key.verify_prehash(digest, &signature).is_ok()
        }
        other => {
            return Err(WitnessStatus::Unsupported {
                detail: format!(
                    "signature algorithm not implemented here: ecdsa-with-SHA512 on curve {}",
                    Oid(Bytes::copy_from_slice(other))
                ),
            });
        }
    };
    if verified {
        Ok(())
    } else {
        Err(wrong("ecdsa-with-SHA512"))
    }
}

/// A CMS failure is `unsupported` when the implementation lacks the
/// algorithm, `invalid` when it judged and refused. Matched on the error
/// type, never its wording.
fn classify_cms(err: CmsError) -> WitnessStatus {
    let cannot_judge = match &err {
        CmsError::UnknownKeyAlgorithm(_)
        | CmsError::UnknownDigestAlgorithm(_)
        | CmsError::UnknownSignatureAlgorithm(_) => true,
        CmsError::X509Certificate(X509CertificateError::UnsupportedSignatureVerification(..)) => {
            true
        }
        CmsError::X509Certificate(err) => lacks_primitive(err),
        _ => false,
    };
    if cannot_judge {
        WitnessStatus::Unsupported {
            detail: format!("signature algorithm not implemented here: {err}"),
        }
    } else {
        invalid(format!("the token's signature does not verify: {err}"))
    }
}

/// Whether the certificate library failed for want of an algorithm, rather than judging and
/// refusing: the one definition of a missing primitive, used by the token's signature and the
/// certificate path alike.
fn lacks_primitive(err: &X509CertificateError) -> bool {
    matches!(
        err,
        X509CertificateError::UnknownDigestAlgorithm(_)
            | X509CertificateError::UnknownSignatureAlgorithm(_)
            | X509CertificateError::UnknownKeyAlgorithm(_)
            | X509CertificateError::UnknownEllipticCurve(_)
    )
}

/// RFC 3161 section 2.4.2: the signature names its certificate by hash in
/// a SigningCertificate (ESSCertID, SHA-1) or SigningCertificateV2
/// (ESSCertIDv2, SHA-256 by default) signed attribute, and that hash must
/// be of the certificate that actually signed. Without it, a token's
/// signer is whichever carried certificate matches the issuer and serial,
/// which the signature does not cover.
fn check_signing_certificate(
    signer_info: &SignerInfo,
    signer: &CapturedX509Certificate,
) -> Result<(), WitnessStatus> {
    let attributes = signer_info
        .signed_attributes()
        .ok_or_else(|| invalid("the signature covers no attributes"))?;
    let find = |oid: &[u8]| {
        attributes
            .attributes()
            .iter()
            .find(|a| a.typ.as_ref() == oid)
    };
    let (attribute, v2) = match (
        find(OID_AA_SIGNING_CERTIFICATE_V2),
        find(OID_AA_SIGNING_CERTIFICATE),
    ) {
        (Some(a), _) => (a, true),
        (None, Some(a)) => (a, false),
        (None, None) => {
            return Err(invalid(
                "the signature does not name its certificate (no SigningCertificate attribute)",
            ));
        }
    };
    let value = attribute
        .values
        .first()
        .ok_or_else(|| invalid("an empty SigningCertificate attribute"))?;
    // The first ESSCertID is the signer's (RFC 5035 section 5.4).
    let (hash_algorithm, expected): (Option<Oid>, OctetString) = value
        .deref()
        .clone()
        .decode(|cons| {
            cons.take_sequence(|signing_certificate| {
                let first = signing_certificate.take_sequence(|certs| {
                    let first = certs.take_sequence(|id| {
                        let algorithm = if v2 {
                            id.take_opt_sequence(|a| {
                                let oid = Oid::take_from(a)?;
                                a.skip_all()?;
                                Ok(oid)
                            })?
                        } else {
                            None
                        };
                        let hash = OctetString::take_from(id)?;
                        id.skip_all()?;
                        Ok((algorithm, hash))
                    })?;
                    certs.skip_all()?;
                    Ok(first)
                })?;
                signing_certificate.skip_all()?;
                Ok(first)
            })
        })
        .map_err(|e| invalid(format!("malformed SigningCertificate attribute: {e}")))?;
    let der = signer.constructed_data();
    let actual: Vec<u8> = match (v2, hash_algorithm.as_ref().map(AsRef::as_ref)) {
        (false, _) | (true, Some(OID_SHA1)) => Sha1::digest(der).to_vec(),
        (true, None | Some(OID_SHA256)) => Sha256::digest(der).to_vec(),
        (true, Some(OID_SHA384)) => Sha384::digest(der).to_vec(),
        (true, Some(OID_SHA512)) => Sha512::digest(der).to_vec(),
        (true, Some(other)) => {
            return Err(WitnessStatus::Unsupported {
                detail: format!(
                    "ESSCertIDv2 hash algorithm {} is not one this verifier computes",
                    Oid(Bytes::copy_from_slice(other))
                ),
            });
        }
    };
    if actual.as_slice() != expected.to_bytes().as_ref() {
        return Err(invalid(
            "the certificate the signature names (ESSCertID) is not the certificate that signed",
        ));
    }
    Ok(())
}

fn extension<'a>(cert: &'a CapturedX509Certificate, oid: &str) -> Option<&'a Extension> {
    cert.iter_extensions().find(|ext| ext.id.to_string() == oid)
}

/// RFC 3161 section 2.3: the signer's certificate carries the extended
/// key usage extension, critical, with id-kp-timeStamping as its only
/// member.
fn has_exact_critical_timestamping_eku(cert: &CapturedX509Certificate) -> bool {
    let Some(ext) = extension(cert, OID_EXTENDED_KEY_USAGE) else {
        return false;
    };
    if !ext.critical.unwrap_or(false) {
        return false;
    }
    Constructed::decode(ext.value.to_bytes(), Mode::Der, |cons| {
        cons.take_sequence(|cons| {
            let mut usages = Vec::new();
            while let Some(oid) = Oid::take_opt_from(cons)? {
                usages.push(oid);
            }
            Ok(usages)
        })
    })
    .is_ok_and(|usages| usages.len() == 1 && usages[0].as_ref() == OID_KP_TIMESTAMPING)
}

/// keyUsage, if the certificate carries one.
fn key_usage(cert: &CapturedX509Certificate) -> Option<BitString> {
    let ext = extension(cert, OID_KEY_USAGE)?;
    Constructed::decode(ext.value.to_bytes(), Mode::Der, BitString::take_from).ok()
}

/// basicConstraints as `(cA, pathLenConstraint)`, if the certificate
/// carries one; a malformed extension reads as absent.
fn basic_constraints(cert: &CapturedX509Certificate) -> Option<(bool, Option<u64>)> {
    let ext = extension(cert, OID_BASIC_CONSTRAINTS)?;
    Constructed::decode(ext.value.to_bytes(), Mode::Der, |cons| {
        cons.take_sequence(|cons| {
            let ca = cons.take_opt_bool()?.unwrap_or(false);
            let path_len = cons.take_opt_u64()?;
            Ok((ca, path_len))
        })
    })
    .ok()
}

fn valid_at(cert: &CapturedX509Certificate, at: DateTime<Utc>) -> bool {
    cert.validity_not_before() <= at && at <= cert.validity_not_after()
}

/// Why no certification path validated.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathFailure {
    /// Every candidate path was judged, and none validates.
    Untrusted(String),
    /// A candidate path needs a primitive this implementation lacks, so it might have
    /// validated. Outranks every judged failure.
    Unsupported(String),
}

/// Certification path validation from the signer to an anchor (RFC 5280 section 6.1).
///
/// Validity is judged at the attested time: what matters is whether the signer was valid when
/// it signed, not when someone checks. An anchor is trusted as given; only its key and name are
/// used. Below it, every certificate must have been valid then, each issuer must be a
/// certification authority whose path length and key usage allow the path, and the signer's key
/// usage, if declared, must allow signing. Revocation and policies are not checked.
///
/// Every candidate issuer is tried, since several certificates may share a name: any path that
/// validates wins, then any that could not be judged, and only a search judged throughout says
/// that no path validates.
fn validate_path(
    signer: &CapturedX509Certificate,
    carried: &[CapturedX509Certificate],
    anchors: &Anchors,
    at: DateTime<Utc>,
) -> Result<(), PathFailure> {
    if anchors.0.iter().any(|a| same(a, signer)) {
        return Ok(());
    }
    if !valid_at(signer, at) {
        return Err(PathFailure::Untrusted(format!(
            "the signer's certificate `{}` was not valid at the attested time",
            common_name(signer)
        )));
    }
    if let Some(usage) = key_usage(signer)
        && !(usage.bit(KU_DIGITAL_SIGNATURE) || usage.bit(KU_NON_REPUDIATION))
    {
        return Err(PathFailure::Untrusted(format!(
            "the signer's certificate `{}` is not permitted to sign (keyUsage)",
            common_name(signer)
        )));
    }
    PathSearch::new(carried, anchors, at)
        .upwards(signer, 0)
        .map_err(|failure| {
            failure.unwrap_or_else(|| {
                PathFailure::Untrusted(format!(
                    "no certification path from the signer `{}` to a supplied anchor",
                    common_name(signer)
                ))
            })
        })
}

fn same(a: &CapturedX509Certificate, b: &CapturedX509Certificate) -> bool {
    a.constructed_data() == b.constructed_data()
}

/// What a search from one certificate found. `Err(None)`: no candidate issuer at all.
type Searched = Result<(), Option<PathFailure>>;

/// A search upwards from the signer. The certificates come from the token, so whoever made it
/// chooses them: each distinct carried certificate is searched at most once per depth, which
/// keeps the work polynomial in what the token carries.
struct PathSearch<'a> {
    carried: Vec<&'a CapturedX509Certificate>,
    anchors: &'a Anchors,
    at: DateTime<Utc>,
    searched: std::collections::HashMap<(usize, usize), Searched>,
    signature_checks: usize,
}

impl<'a> PathSearch<'a> {
    fn new(
        carried: &'a [CapturedX509Certificate],
        anchors: &'a Anchors,
        at: DateTime<Utc>,
    ) -> Self {
        let mut distinct: Vec<&CapturedX509Certificate> = Vec::new();
        for cert in carried {
            if !distinct.iter().any(|seen| same(seen, cert)) {
                distinct.push(cert);
            }
        }
        Self {
            carried: distinct,
            anchors,
            at,
            searched: std::collections::HashMap::new(),
            signature_checks: 0,
        }
    }

    /// From `current`, with `below` certificates already under it on the path.
    fn upwards(&mut self, current: &CapturedX509Certificate, below: usize) -> Searched {
        if below == MAX_PATH_LENGTH {
            return Err(Some(PathFailure::Untrusted(
                "the certification path is longer than this verifier follows".to_string(),
            )));
        }
        let issues = |c: &CapturedX509Certificate| {
            !same(current, c) && c.subject_name() == current.issuer_name()
        };
        let mut failure: Option<PathFailure> = None;
        let anchors = self.anchors;
        for anchor in anchors.0.iter().filter(|a| issues(a)) {
            self.signature_checks += 1;
            match signed_by(current, anchor) {
                Signature::Signed => return Ok(()),
                Signature::NotSigned => {}
                Signature::CannotJudge(detail) => {
                    note(&mut failure, PathFailure::Unsupported(detail));
                }
            }
        }
        for index in 0..self.carried.len() {
            let issuer = self.carried[index];
            if !issues(issuer) {
                continue;
            }
            self.signature_checks += 1;
            let unjudged = match signed_by(current, issuer) {
                Signature::Signed => None,
                Signature::NotSigned => continue,
                Signature::CannotJudge(detail) => Some(detail),
            };
            // The rest of the path is judged first: an edge that could not be judged matters
            // only if the path above it could validate.
            let rest = match may_issue(issuer, self.at, below) {
                Err(reason) => Err(Some(PathFailure::Untrusted(reason))),
                Ok(()) => self.upwards_from_carried(index, below + 1),
            };
            match (unjudged, rest) {
                (None, Ok(())) => return Ok(()),
                (Some(detail), Ok(())) => note(&mut failure, PathFailure::Unsupported(detail)),
                (_, Err(Some(found))) => note(&mut failure, found),
                (_, Err(None)) => {}
            }
        }
        Err(failure)
    }

    fn upwards_from_carried(&mut self, index: usize, below: usize) -> Searched {
        if let Some(found) = self.searched.get(&(index, below)) {
            return found.clone();
        }
        let found = self.upwards(self.carried[index], below);
        self.searched.insert((index, below), found.clone());
        found
    }
}

/// Keep the first failure, unless a later one could not be judged and the kept one was.
fn note(failure: &mut Option<PathFailure>, found: PathFailure) {
    let outranks = matches!(
        (&*failure, &found),
        (None, _) | (Some(PathFailure::Untrusted(_)), PathFailure::Unsupported(_))
    );
    if outranks {
        *failure = Some(found);
    }
}

/// Whether `issuer` could issue a certificate with `below` certificates under it on the path.
fn may_issue(
    issuer: &CapturedX509Certificate,
    at: DateTime<Utc>,
    below: usize,
) -> Result<(), String> {
    let name = common_name(issuer);
    if !valid_at(issuer, at) {
        return Err(format!(
            "the issuing certificate `{name}` was not valid at the attested time"
        ));
    }
    match basic_constraints(issuer) {
        Some((true, path_len)) => {
            if path_len.is_some_and(|max| (below as u64) > max) {
                return Err(format!(
                    "the issuing certificate `{name}` allows a shorter path than this one"
                ));
            }
        }
        _ => {
            return Err(format!(
                "the issuing certificate `{name}` is not a certification authority \
                 (basicConstraints)"
            ));
        }
    }
    if let Some(usage) = key_usage(issuer)
        && !usage.bit(KU_KEY_CERT_SIGN)
    {
        return Err(format!(
            "the issuing certificate `{name}` is not permitted to sign certificates (keyUsage)"
        ));
    }
    Ok(())
}

/// Whether an issuer signed a certificate, as far as this implementation can tell.
#[derive(Debug, PartialEq, Eq)]
enum Signature {
    Signed,
    NotSigned,
    /// The check needs a primitive this implementation lacks.
    CannotJudge(String),
}

/// Whether `issuer`'s key signed `cert`, checked with the algorithm `cert` names for its
/// signature and the issuer's key type. The certificate library's own shortcut pairs that
/// algorithm with `cert`'s key type instead, so an RSA issuer of an ECDSA certificate (FreeTSA's
/// chain) could never verify.
fn signed_by(cert: &CapturedX509Certificate, issuer: &CapturedX509Certificate) -> Signature {
    let raw: &x509_certificate::rfc5280::Certificate = cert.as_ref();
    let issuer_raw: &x509_certificate::rfc5280::Certificate = issuer.as_ref();
    let resolved = SignatureAlgorithm::try_from(&raw.signature_algorithm).and_then(|signature| {
        let key =
            KeyAlgorithm::try_from(&issuer_raw.tbs_certificate.subject_public_key_info.algorithm)?;
        signature.resolve_verification_algorithm(key)
    });
    let algorithm = match resolved {
        Ok(algorithm) => algorithm,
        Err(err) if lacks_primitive(&err) => {
            return Signature::CannotJudge(format!(
                "signature algorithm not implemented here: `{}` issuing `{}`: {err}",
                common_name(issuer),
                common_name(cert)
            ));
        }
        // Includes a key of another family than the signature (an ECDSA key and an RSA
        // signature): that key cannot have made it.
        Err(_) => return Signature::NotSigned,
    };
    let Some(tbs) = raw.tbs_certificate.raw_data.as_ref() else {
        return Signature::NotSigned;
    };
    if issuer
        .verify_signed_data_with_algorithm(tbs, raw.signature.octet_bytes(), algorithm)
        .is_ok()
    {
        Signature::Signed
    } else {
        Signature::NotSigned
    }
}

/// Test helpers: rebuild a `Request` from recorded DER, so a recorded response can be
/// self-checked against the nonce it echoes.
pub mod testing {
    use super::{Constructed, Mode, Request, TimeStampReq, WitnessError};

    /// Errors if the DER is not a request or carries no nonce.
    pub fn request_from_der(der: &[u8]) -> Result<Request, WitnessError> {
        let req = Constructed::decode(der, Mode::Ber, TimeStampReq::take_from)
            .map_err(|e| WitnessError::Anchors(format!("not a timestamp request: {e}")))?;
        let nonce = req
            .nonce
            .ok_or_else(|| WitnessError::Anchors("the request carries no nonce".to_string()))?;
        Ok(Request {
            der: der.to_vec(),
            nonce,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/rfc3161/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// Leaf, intermediate, root - the order the PEM holds them.
    fn digicert() -> Vec<CapturedX509Certificate> {
        CapturedX509Certificate::from_pem_multiple(fixture("digicert_chain.pem")).unwrap()
    }

    fn anchors_of(certs: &[CapturedX509Certificate]) -> Anchors {
        Anchors(certs.to_vec())
    }

    #[test]
    fn the_timestamping_usage_must_be_critical_and_alone() {
        let chain = digicert();
        assert!(has_exact_critical_timestamping_eku(&chain[0]));
        // The intermediate carries the usage too, but not critical.
        assert!(!has_exact_critical_timestamping_eku(&chain[1]));
        assert!(!has_exact_critical_timestamping_eku(&chain[2]));
    }

    #[test]
    fn a_path_is_judged_at_the_attested_time() {
        let chain = digicert();
        let (leaf, carried) = (&chain[0], &chain[..2]);
        let root = anchors_of(&chain[2..]);
        let attested = Utc.with_ymd_and_hms(2026, 9, 16, 10, 20, 5).unwrap();
        assert_eq!(validate_path(leaf, carried, &root, attested), Ok(()));

        // Before the leaf was issued, the same path does not validate.
        let too_early = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let err = untrusted(validate_path(leaf, carried, &root, too_early));
        assert!(err.contains("not valid at the attested time"), "{err}");

        // An anchor nothing chains to.
        let unrelated = Anchors::from_pem(&fixture("unrelated.pem")).unwrap();
        let err = untrusted(validate_path(leaf, carried, &unrelated, attested));
        assert!(err.contains("no certification path"), "{err}");

        // A pinned leaf is trusted as given, whatever the time.
        assert_eq!(
            validate_path(leaf, carried, &anchors_of(&chain[..1]), too_early),
            Ok(())
        );

        // Without the intermediate in hand, the root is unreachable.
        let err = untrusted(validate_path(leaf, &chain[..1], &root, attested));
        assert!(err.contains("no certification path"), "{err}");
    }

    /// A certificate's signature is checked with its issuer's key type: an
    /// RSA issuer signing an ECDSA certificate verifies, where the library's
    /// shortcut, keyed on the subject's key, cannot.
    #[test]
    fn a_certificate_is_checked_with_its_issuers_key() {
        let pair =
            CapturedX509Certificate::from_pem_multiple(fixture("rsa_issuer_ecdsa_subject.pem"))
                .unwrap();
        let (issuer, subject) = (&pair[0], &pair[1]);
        assert_eq!(signed_by(subject, issuer), Signature::Signed);
        assert!(subject.verify_signed_by_certificate(issuer).is_err());
        // Not signed by itself, and not by the other way round.
        assert_eq!(signed_by(subject, subject), Signature::NotSigned);
        assert_eq!(signed_by(issuer, subject), Signature::NotSigned);
        // DigiCert's all-RSA chain still links.
        let chain = digicert();
        assert_eq!(signed_by(&chain[0], &chain[1]), Signature::Signed);
        assert_eq!(signed_by(&chain[1], &chain[2]), Signature::Signed);
    }

    fn untrusted(result: Result<(), PathFailure>) -> String {
        match result {
            Err(PathFailure::Untrusted(reason)) => reason,
            other => panic!("expected untrusted, got {other:?}"),
        }
    }

    /// Root, a P-521 intermediate, its leaf A, a P-256 intermediate under the same name as a
    /// certification authority and again as not one, and its leaf C: the order the PEM holds.
    fn branches() -> Vec<CapturedX509Certificate> {
        CapturedX509Certificate::from_pem_multiple(fixture("path_branches.pem")).unwrap()
    }

    fn in_range() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()
    }

    /// The only issuer of leaf A has a P-521 key, which this build cannot check a signature
    /// with: the path might validate, so it is unsupported, not untrusted. An unrelated anchor
    /// beside the real one does not make that edge judgeable.
    #[test]
    fn a_path_through_a_signature_this_build_cannot_check_is_unsupported() {
        let c = branches();
        let (root, mid_p521, leaf_a) = (&c[0], &c[1], &c[2]);
        let unrelated = Anchors::from_pem(&fixture("unrelated.pem")).unwrap();
        let with_unrelated = anchors_of(&[unrelated.0[0].clone(), root.clone()]);
        for anchors in [anchors_of(std::slice::from_ref(root)), with_unrelated] {
            let result =
                validate_path(leaf_a, std::slice::from_ref(mid_p521), &anchors, in_range());
            let Err(PathFailure::Unsupported(detail)) = result else {
                panic!("expected unsupported, got {result:?}");
            };
            assert!(detail.contains("Morpholog test intermediate"), "{detail}");
        }
    }

    /// Without the unjudgeable issuer, a search judged throughout that finds no trusted path
    /// is untrusted, as before.
    #[test]
    fn a_path_judged_throughout_is_untrusted() {
        let c = branches();
        let (root, mid_not_ca, leaf_c) = (&c[0], &c[4], &c[5]);
        let err = untrusted(validate_path(
            leaf_c,
            std::slice::from_ref(mid_not_ca),
            &anchors_of(std::slice::from_ref(root)),
            in_range(),
        ));
        assert!(err.contains("not a certification authority"), "{err}");
    }

    /// One candidate cannot be judged; another signed but may not issue. The second path is
    /// certainly bad, but the first might have validated, so the answer is unsupported.
    #[test]
    fn a_judged_failure_does_not_hide_a_path_that_could_not_be_judged() {
        let c = branches();
        let (root, mid_p521, mid_not_ca, leaf_c) = (&c[0], &c[1], &c[4], &c[5]);
        for carried in [
            [mid_p521.clone(), mid_not_ca.clone()],
            [mid_not_ca.clone(), mid_p521.clone()],
        ] {
            let result = validate_path(
                leaf_c,
                &carried,
                &anchors_of(std::slice::from_ref(root)),
                in_range(),
            );
            assert!(
                matches!(result, Err(PathFailure::Unsupported(_))),
                "{result:?}"
            );
        }
    }

    /// Any path that validates wins, whatever was tried before it: neither a candidate that
    /// cannot be judged nor one that signed but may not issue stops the search.
    #[test]
    fn any_path_that_validates_is_found() {
        let c = branches();
        let (root, mid_p521, mid_p256, mid_not_ca, leaf_c) = (&c[0], &c[1], &c[3], &c[4], &c[5]);
        let carried = [mid_p521.clone(), mid_not_ca.clone(), mid_p256.clone()];
        assert_eq!(
            validate_path(
                leaf_c,
                &carried,
                &anchors_of(std::slice::from_ref(root)),
                in_range()
            ),
            Ok(())
        );
    }

    /// An issuer that could not be judged, but whose own path reaches no supplied anchor, could
    /// not have completed a trusted path whatever its signature: the search is still judged
    /// throughout, so untrusted.
    #[test]
    fn an_unjudgeable_issuer_on_a_dead_path_does_not_make_it_unsupported() {
        let c = branches();
        let (mid_p521, mid_not_ca, leaf_c) = (&c[1], &c[4], &c[5]);
        let unrelated = Anchors::from_pem(&fixture("unrelated.pem")).unwrap();
        for carried in [
            vec![mid_p521.clone()],
            vec![mid_p521.clone(), mid_not_ca.clone()],
        ] {
            untrusted(validate_path(leaf_c, &carried, &unrelated, in_range()));
        }
    }

    /// Authorities that all share one name, each a candidate issuer of every other, with keys
    /// this build cannot check. The token chooses its certificates, so the search must stay
    /// polynomial in them: each certificate is searched at most once per depth.
    #[test]
    fn a_token_full_of_candidate_issuers_cannot_make_the_search_explode() {
        let c = CapturedX509Certificate::from_pem_multiple(fixture("path_loop.pem")).unwrap();
        let (leaf, carried) = (&c[0], &c[..]);
        let unrelated = Anchors::from_pem(&fixture("unrelated.pem")).unwrap();
        let mut search = PathSearch::new(carried, &unrelated, in_range());
        let found = search.upwards(leaf, 0);
        assert!(
            matches!(found, Err(Some(PathFailure::Untrusted(_)))),
            "{found:?}"
        );
        let n = carried.len();
        let bound = (n * (MAX_PATH_LENGTH + 1) + 1) * (n + unrelated.len());
        assert!(
            search.signature_checks <= bound,
            "{} signature checks for {n} certificates",
            search.signature_checks
        );
    }

    #[test]
    fn the_issuing_certificates_constraints_are_read() {
        let chain = digicert();
        assert_eq!(basic_constraints(&chain[0]), Some((false, None)));
        assert_eq!(basic_constraints(&chain[1]), Some((true, Some(0))));
        assert_eq!(basic_constraints(&chain[2]), Some((true, None)));
        assert!(key_usage(&chain[1]).unwrap().bit(KU_KEY_CERT_SIGN));
        assert!(!key_usage(&chain[0]).unwrap().bit(KU_KEY_CERT_SIGN));
        assert!(key_usage(&chain[0]).unwrap().bit(KU_DIGITAL_SIGNATURE));
    }

    fn raw_token(name: &str) -> Asn1SignedData {
        let response = fixture(name);
        let resp =
            Constructed::decode(response.as_slice(), Mode::Ber, TimeStampResp::take_from).unwrap();
        resp.time_stamp_token
            .unwrap()
            .content
            .decode(Asn1SignedData::take_from)
            .unwrap()
    }

    fn der(token: &Asn1SignedData) -> Vec<u8> {
        token
            .encode_ref()
            .to_captured(Mode::Ber)
            .into_bytes()
            .to_vec()
    }

    /// The stand-in changes one OID and nothing else: restore it and the
    /// token re-encodes exactly as the original.
    #[test]
    fn the_stand_in_view_changes_only_the_signature_algorithm() {
        let raw = raw_token("genesis_freetsa.tsr");
        let view = with_stand_in_algorithm(&raw);
        assert_eq!(
            view.signer_infos[0].signature_algorithm.algorithm.as_ref(),
            OID_ECDSA_WITH_SHA384
        );
        assert_ne!(der(&view), der(&raw));
        let mut restored = view.clone();
        restored.signer_infos[0].signature_algorithm.algorithm =
            raw.signer_infos[0].signature_algorithm.algorithm.clone();
        assert_eq!(der(&restored), der(&raw));
    }

    /// Every token the CMS crate already judges stays on its route.
    #[test]
    fn only_ecdsa_with_sha512_leaves_the_cms_route() {
        for name in ["genesis_digicert.tsr", "chained_digicert.tsr"] {
            assert_eq!(
                signature_route(&raw_token(name)),
                SignatureRoute::Cms,
                "{name}"
            );
        }
        for name in [
            "genesis_freetsa.tsr",
            "ecdsa_sha512_prime256v1.tsr",
            "ecdsa_sha512_secp521r1.tsr",
        ] {
            assert_eq!(
                signature_route(&raw_token(name)),
                SignatureRoute::EcdsaSha512,
                "{name}"
            );
        }
    }

    /// What the SHA-512 check is handed for a token's one signer: the
    /// signer's curve and key, the digest of the exact signed bytes, and
    /// the raw signature.
    fn sha512_inputs(name: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        let raw = raw_token(name);
        let sd = cms_view_without_signature_verification(&raw).unwrap();
        let (issuer, serial) = sd
            .signers()
            .next()
            .unwrap()
            .certificate_issuer_and_serial()
            .unwrap();
        let signer = sd
            .certificates()
            .find(|c| c.issuer_name() == issuer && c.serial_number_asn1() == serial)
            .unwrap();
        let key = &signer.tbs_certificate().subject_public_key_info;
        let curve = key
            .algorithm
            .parameters
            .as_ref()
            .unwrap()
            .decode_oid()
            .unwrap();
        let signed = raw.signer_infos[0]
            .signed_attributes_digested_content()
            .unwrap()
            .unwrap();
        (
            curve.as_ref().to_vec(),
            signer.public_key_data().to_vec(),
            Sha512::digest(&signed).to_vec(),
            raw.signer_infos[0].signature.to_bytes().to_vec(),
        )
    }

    #[test]
    fn the_sha512_check_judges_the_exact_digest_and_signature() {
        let (curve, key, digest, signature) = sha512_inputs("genesis_freetsa.tsr");
        assert_eq!(curve, OID_P384);
        assert_eq!(
            check_ecdsa_sha512(&curve, &key, &digest, &signature),
            Ok(())
        );

        let mut other_digest = digest.clone();
        other_digest[0] ^= 1;
        assert!(matches!(
            check_ecdsa_sha512(&curve, &key, &other_digest, &signature),
            Err(WitnessStatus::Invalid { .. })
        ));
        // A byte inside `r`, still well-formed DER.
        let mut other_signature = signature.clone();
        other_signature[6] ^= 1;
        assert!(matches!(
            check_ecdsa_sha512(&curve, &key, &digest, &other_signature),
            Err(WitnessStatus::Invalid { .. })
        ));
        assert!(matches!(
            check_ecdsa_sha512(&curve, &key, &digest, b"not der"),
            Err(WitnessStatus::Invalid { .. })
        ));
    }

    /// One byte of the stored signature changed, found by its own bytes in
    /// the token, makes the token wrong, not unsupported.
    #[test]
    fn a_freetsa_token_with_a_changed_signature_is_invalid() {
        let proof = fixture("genesis_freetsa.tsr");
        let signature = raw_token("genesis_freetsa.tsr").signer_infos[0]
            .signature
            .to_bytes();
        let at = proof
            .windows(signature.len())
            .position(|w| w == signature.as_ref())
            .expect("the signature is in the token as stored");
        let mut changed = proof.clone();
        changed[at + 6] ^= 1;
        assert!(matches!(
            verify_rfc3161(&changed, &fixture("genesis_payload.bin"), None),
            WitnessStatus::Invalid { .. }
        ));
    }

    /// The signature names its certificate by a hash of the bytes the token
    /// carries. The CMS crate's re-encoded copy of an ECDSA-signed
    /// certificate is other bytes, so the name matches only the carried one.
    #[test]
    fn the_signature_names_the_carried_bytes_not_a_reencoding() {
        let raw = raw_token("ecdsa_sha512_prime256v1.tsr");
        let response = fixture("ecdsa_sha512_prime256v1.tsr");
        let token = Constructed::decode(response.as_slice(), Mode::Ber, TimeStampResp::take_from)
            .unwrap()
            .time_stamp_token
            .unwrap();
        let sd = cms_view_without_signature_verification(&raw).unwrap();
        let signer_info = sd.signers().next().unwrap();
        let (issuer, serial) = signer_info.certificate_issuer_and_serial().unwrap();
        let is_signer = |c: &&CapturedX509Certificate| {
            c.issuer_name() == issuer && c.serial_number_asn1() == serial
        };
        let carried = carried_certificates(&token.content).unwrap();
        let carried_signer = carried.iter().find(is_signer).unwrap();
        let reencoded_signer = sd.certificates().find(is_signer).unwrap();
        assert_ne!(
            carried_signer.constructed_data(),
            reencoded_signer.constructed_data(),
            "the CMS crate re-encodes this certificate"
        );
        assert_eq!(
            check_signing_certificate(signer_info, carried_signer),
            Ok(())
        );
        assert!(matches!(
            check_signing_certificate(signer_info, reencoded_signer),
            Err(WitnessStatus::Invalid { detail }) if detail.contains("ESSCertID")
        ));
        // The carried bytes are the token's own: each is a slice of it.
        for certificate in &carried {
            assert!(
                response
                    .windows(certificate.constructed_data().len())
                    .any(|w| w == certificate.constructed_data())
            );
        }
    }

    /// A curve this crate does not implement is `unsupported`, with a
    /// well-formed key and signature: not a judgement against them.
    #[test]
    fn sha512_on_another_curve_is_unsupported_not_wrong() {
        let (curve, key, digest, signature) = sha512_inputs("ecdsa_sha512_secp521r1.tsr");
        assert_eq!(curve, [0x2b, 0x81, 0x04, 0x00, 0x23], "P-521");
        assert!(matches!(
            check_ecdsa_sha512(&curve, &key, &digest, &signature),
            Err(WitnessStatus::Unsupported { detail }) if detail.contains("1.3.132.0.35")
        ));
    }

    #[test]
    fn the_named_signing_certificate_must_be_the_one_that_signed() {
        let response = fixture("genesis_digicert.tsr");
        let resp =
            Constructed::decode(response.as_slice(), Mode::Ber, TimeStampResp::take_from).unwrap();
        let asn1_sd: Asn1SignedData = resp
            .time_stamp_token
            .unwrap()
            .content
            .decode(Asn1SignedData::take_from)
            .unwrap();
        let sd = SignedData::try_from(&asn1_sd).unwrap();
        let signer_info = sd.signers().next().unwrap();
        let chain = digicert();
        assert_eq!(check_signing_certificate(signer_info, &chain[0]), Ok(()));
        let err = check_signing_certificate(signer_info, &chain[1]).unwrap_err();
        assert!(
            matches!(&err, WitnessStatus::Invalid { detail } if detail.contains("ESSCertID")),
            "{err:?}"
        );
    }
}
