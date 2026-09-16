//! External witnesses to an audit tree head, rung one: RFC 3161 timestamp
//! tokens. A witness proves that a commitment existed no later than a
//! time, from a root the log operator does not control. This crate builds
//! the request, self-checks the authority's response before it is stored,
//! and verifies a stored proof offline against anchors the verifier chose.
//!
//! The contract is narrow on purpose: tokens are recognised, the
//! algorithm suite this implementation supports is verified, a proof it
//! cannot judge is reported as `unsupported`, and inability is never
//! misreported as cryptographic failure. Nothing here touches a network;
//! the caller posts the request bytes and hands back the response bytes.

use bcder::decode::Constructed;
use bcder::encode::Values;
use bcder::{Integer, Mode, OctetString, Oid};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use cryptographic_message_syntax::asn1::rfc3161::{
    MessageImprint, OID_CONTENT_TYPE_TST_INFO, PkiStatus, TimeStampReq, TimeStampResp, TstInfo,
};
use cryptographic_message_syntax::asn1::rfc5652::{
    OID_ID_SIGNED_DATA, SignedData as Asn1SignedData,
};
use cryptographic_message_syntax::{CmsError, SignedData};
use sha2::{Digest, Sha256};
use x509_certificate::CapturedX509Certificate;
use x509_certificate::rfc5280::AlgorithmIdentifier;

/// The media type of a request body, RFC 3161 section 3.4.
pub const REQUEST_CONTENT_TYPE: &str = "application/timestamp-query";
/// The media type of a response body.
pub const REPLY_CONTENT_TYPE: &str = "application/timestamp-reply";

const OID_SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
const OID_EXTENDED_KEY_USAGE: &str = "2.5.29.37";
const OID_KP_TIMESTAMPING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x08];
/// Certificates a chain may pass through between the signer and an anchor.
const MAX_CHAIN_DEPTH: usize = 6;

#[derive(Debug, thiserror::Error)]
pub enum WitnessError {
    #[error("{0}")]
    Anchors(String),
}

/// The certificates a verifier deliberately trusts, every one an anchor:
/// a signer is accepted when it is one of them or chains to one through
/// the certificates its token carries. A pinned leaf is the narrowest
/// choice and survives no rotation; a timestamping intermediate or root
/// permits controlled rotation; a broad commercial root is broad trust.
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
    Verified { attested_at: DateTime<Utc> },
    /// Genuine and over this head, but the signer chains to none of the
    /// supplied anchors.
    Untrusted {
        attested_at: DateTime<Utc>,
        signer: String,
    },
    /// Genuine and over this head; no trust material was supplied, so
    /// nothing is said about the signer.
    Unverified { attested_at: DateTime<Utc> },
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
    if chains_to_anchor(&examined.signer, &examined.carried, anchors) {
        WitnessStatus::Verified {
            attested_at: examined.attested_at,
        }
    } else {
        WitnessStatus::Untrusted {
            attested_at: examined.attested_at,
            signer: examined
                .signer
                .subject_common_name()
                .unwrap_or_else(|| "<no common name>".to_string()),
        }
    }
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
    pub attested_at: DateTime<Utc>,
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
    attested_at: DateTime<Utc>,
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

    if tst.message_imprint.hash_algorithm.algorithm.as_ref() != OID_SHA256 {
        return Err(WitnessStatus::Unsupported {
            detail: format!(
                "imprint algorithm {} is not SHA-256",
                tst.message_imprint.hash_algorithm.algorithm
            ),
        });
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
    let attested_at: DateTime<Utc> = tst.gen_time.clone().into();

    let sd = SignedData::try_from(&asn1_sd).map_err(classify_cms)?;
    let mut signer_cert = None;
    for signer in sd.signers() {
        signer
            .verify_signature_with_signed_data(&sd)
            .map_err(classify_cms)?;
        signer
            .verify_message_digest_with_signed_data(&sd)
            .map_err(classify_cms)?;
        let (issuer, serial) = signer
            .certificate_issuer_and_serial()
            .ok_or_else(|| invalid("the signer is not identified by issuer and serial"))?;
        let cert = sd
            .certificates()
            .find(|c| c.issuer_name() == issuer && c.serial_number_asn1() == serial)
            .ok_or_else(|| invalid("the signer's certificate is not carried in the token"))?;
        signer_cert = Some(cert.clone());
    }
    let signer = signer_cert.ok_or_else(|| invalid("the token has no signer"))?;
    if !has_critical_timestamping_eku(&signer) {
        return Err(invalid(
            "the signer's certificate lacks the critical timestamping extended key usage",
        ));
    }
    Ok(Examined {
        attested_at,
        signer,
        carried: sd.certificates().cloned().collect(),
    })
}

/// A CMS failure is `unsupported` when the backend lacks the algorithm,
/// `invalid` when it judged and refused.
fn classify_cms(err: CmsError) -> WitnessStatus {
    let rendered = format!("{err:?}");
    if rendered.contains("Unknown") && rendered.contains("Algorithm") {
        WitnessStatus::Unsupported {
            detail: format!("signature algorithm not implemented here: {err}"),
        }
    } else {
        invalid(format!("the token's signature does not verify: {err}"))
    }
}

/// RFC 3161 section 2.3: the signer's certificate must carry exactly the
/// id-kp-timeStamping extended key usage, marked critical.
fn has_critical_timestamping_eku(cert: &CapturedX509Certificate) -> bool {
    cert.iter_extensions().any(|ext| {
        ext.id.to_string() == OID_EXTENDED_KEY_USAGE
            && ext.critical.unwrap_or(false)
            && Constructed::decode(ext.value.to_bytes(), Mode::Der, |cons| {
                cons.take_sequence(|cons| {
                    let mut found = false;
                    while let Some(oid) = Oid::take_opt_from(cons)? {
                        found |= oid.as_ref() == OID_KP_TIMESTAMPING;
                    }
                    Ok(found)
                })
            })
            .unwrap_or(false)
    })
}

/// The signer is an anchor, or chains to one through the certificates the
/// token carries, within a bounded depth. Identity is the captured bytes,
/// never a re-encoding.
fn chains_to_anchor(
    signer: &CapturedX509Certificate,
    carried: &[CapturedX509Certificate],
    anchors: &Anchors,
) -> bool {
    let mut current = signer.clone();
    for _ in 0..MAX_CHAIN_DEPTH {
        if anchors
            .0
            .iter()
            .any(|a| a.constructed_data() == current.constructed_data())
            || anchors
                .0
                .iter()
                .any(|a| current.verify_signed_by_certificate(a).is_ok())
        {
            return true;
        }
        let Some(issuer) = carried.iter().find(|c| {
            c.constructed_data() != current.constructed_data()
                && c.subject_name() == current.issuer_name()
                && current.verify_signed_by_certificate(c).is_ok()
        }) else {
            return false;
        };
        current = issuer.clone();
    }
    false
}

/// Helpers for tests in this and other crates: a `Request` rebuilt from a
/// recorded request's DER, so a recorded response can be self-checked
/// against the nonce it actually echoes.
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
