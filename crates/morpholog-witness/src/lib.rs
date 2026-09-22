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
    OID_ID_SIGNED_DATA, SignedData as Asn1SignedData,
};
use cryptographic_message_syntax::{CmsError, SignedData, SignerInfo};
use jiff::Timestamp;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use x509_certificate::rfc5280::{AlgorithmIdentifier, Extension};
use x509_certificate::{CapturedX509Certificate, X509CertificateError};

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

/// The certificates a verifier deliberately trusts, every one an anchor:
/// a signer is accepted when it is one of them, or when a certification
/// path from it to one of them validates - issuer names chain, each
/// signature verifies, every certificate on the path was valid at the
/// attested time, and each issuing certificate is a certification
/// authority whose constraints permit the path (RFC 5280 section 6.1).
/// Revocation and certificate policies are not checked: a verifier that
/// needs them holds them in the anchors it chooses. A pinned leaf is the
/// narrowest choice and survives no rotation; a timestamping intermediate
/// or root permits controlled rotation; a broad commercial root is broad
/// trust.
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
        Err(reason) => WitnessStatus::Untrusted {
            attested_at: examined.attested_at,
            signer: common_name(&examined.signer),
            reason,
        },
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

    let sd = SignedData::try_from(&asn1_sd).map_err(classify_cms)?;
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
    signer_info
        .verify_signature_with_signed_data(&sd)
        .map_err(classify_cms)?;
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
    let signer = sd
        .certificates()
        .find(|c| c.issuer_name() == issuer && c.serial_number_asn1() == serial)
        .ok_or_else(|| invalid("the signer's certificate is not carried in the token"))?
        .clone();
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
        carried: sd.certificates().cloned().collect(),
    })
}

/// A CMS failure is `unsupported` when the implementation lacks the
/// algorithm, `invalid` when it judged and refused. Matched on the error
/// type, never its wording.
fn classify_cms(err: CmsError) -> WitnessStatus {
    let lacks_primitive = matches!(
        &err,
        CmsError::UnknownKeyAlgorithm(_)
            | CmsError::UnknownDigestAlgorithm(_)
            | CmsError::UnknownSignatureAlgorithm(_)
            | CmsError::X509Certificate(
                X509CertificateError::UnknownDigestAlgorithm(_)
                    | X509CertificateError::UnknownSignatureAlgorithm(_)
                    | X509CertificateError::UnknownKeyAlgorithm(_)
                    | X509CertificateError::UnknownEllipticCurve(_)
                    | X509CertificateError::UnsupportedSignatureVerification(_, _)
            )
    );
    if lacks_primitive {
        WitnessStatus::Unsupported {
            detail: format!("signature algorithm not implemented here: {err}"),
        }
    } else {
        invalid(format!("the token's signature does not verify: {err}"))
    }
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

/// Certification path validation from the signer to one of the anchors,
/// RFC 5280 section 6.1, evaluated at the attested time: a timestamp is
/// judged by whether its signer was valid when it signed, not when a
/// reader happens to check. An anchor is trusted as given, whether it is
/// the signer itself (a pinned leaf) or the issuer at the top of the
/// path; only its key and name are used. Every certificate below the
/// anchor must have been valid at that time; each issuing certificate
/// must be a certification authority (basicConstraints cA, a path length
/// that admits what lies below it, keyCertSign if it declares key usage);
/// and the signer, if it declares key usage, must be permitted to sign.
/// Revocation and policies are not checked here.
fn validate_path(
    signer: &CapturedX509Certificate,
    carried: &[CapturedX509Certificate],
    anchors: &Anchors,
    at: DateTime<Utc>,
) -> Result<(), String> {
    let same = |a: &CapturedX509Certificate, b: &CapturedX509Certificate| {
        a.constructed_data() == b.constructed_data()
    };
    if anchors.0.iter().any(|a| same(a, signer)) {
        return Ok(());
    }
    if !valid_at(signer, at) {
        return Err(format!(
            "the signer's certificate `{}` was not valid at the attested time",
            common_name(signer)
        ));
    }
    if let Some(usage) = key_usage(signer)
        && !(usage.bit(KU_DIGITAL_SIGNATURE) || usage.bit(KU_NON_REPUDIATION))
    {
        return Err(format!(
            "the signer's certificate `{}` is not permitted to sign (keyUsage)",
            common_name(signer)
        ));
    }
    let issued_by = |cert: &CapturedX509Certificate, issuer: &CapturedX509Certificate| {
        !same(cert, issuer)
            && issuer.subject_name() == cert.issuer_name()
            && cert.verify_signed_by_certificate(issuer).is_ok()
    };
    let mut current = signer.clone();
    for below in 0..MAX_PATH_LENGTH {
        if anchors.0.iter().any(|a| issued_by(&current, a)) {
            return Ok(());
        }
        let Some(issuer) = carried.iter().find(|c| issued_by(&current, c)) else {
            return Err(format!(
                "no certification path from the signer `{}` to a supplied anchor",
                common_name(signer)
            ));
        };
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
                "the issuing certificate `{name}` is not permitted to sign certificates \
                 (keyUsage)"
            ));
        }
        current = issuer.clone();
    }
    Err("the certification path is longer than this verifier follows".to_string())
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
        let err = validate_path(leaf, carried, &root, too_early).unwrap_err();
        assert!(err.contains("not valid at the attested time"), "{err}");

        // An anchor nothing chains to.
        let unrelated = Anchors::from_pem(&fixture("unrelated.pem")).unwrap();
        let err = validate_path(leaf, carried, &unrelated, attested).unwrap_err();
        assert!(err.contains("no certification path"), "{err}");

        // A pinned leaf is trusted as given, whatever the time.
        assert_eq!(
            validate_path(leaf, carried, &anchors_of(&chain[..1]), too_early),
            Ok(())
        );

        // Without the intermediate in hand, the root is unreachable.
        let err = validate_path(leaf, &chain[..1], &root, attested).unwrap_err();
        assert!(err.contains("no certification path"), "{err}");
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
