//! Fleet CA and host identity.
//!
//! Each org has a fleet CA. Enrollment gives every host an Ed25519 key pair and an X.509
//! certificate from that CA, with subject `CN=<host id>, O=<org id>`. On every connect the
//! host proves possession of the key by signing [`host_proof_transcript`]; the server
//! checks the cert chains to the org's CA, then checks the signature.
//!
//! Transport TLS still authenticates the *server* to the host (WebPKI for hosted, pinned
//! fingerprint for self-hosted). Binding the server identity into the transcript means a
//! proof made for one server can't be replayed to another.

use crate::PROTOCOL_VERSION;
use crate::ids::{HostId, OrgId};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ED25519,
};
use sha2::{Digest, Sha256};
use std::time::Duration;
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use x509_parser::oid_registry::OID_SIG_ED25519;
use x509_parser::prelude::{FromDer, X509Certificate};

/// Domain separator for host proofs. A new transcript format gets a new tag.
pub const HOST_AUTH_DOMAIN: &[u8] = b"rconn/host-auth/v1";

/// How long a host certificate is valid. Hosts renew well before expiry.
pub const HOST_CERT_LIFETIME: Duration = Duration::from_secs(365 * 24 * 3600);

#[derive(Debug, Error)]
pub enum FleetError {
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("certificate is not valid PEM/X.509")]
    Parse,
    #[error("certificate was not issued by this org's fleet CA")]
    WrongIssuer,
    #[error("certificate is expired or not yet valid")]
    Expired,
    #[error("certificate subject is malformed")]
    BadSubject,
    #[error("certificate key is not Ed25519")]
    BadKey,
    #[error("proof signature is invalid")]
    BadSignature,
}

/// What a verified host certificate says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClaims {
    pub host_id: HostId,
    pub org_id: OrgId,
    pub public_key: [u8; 32],
}

/// A host's private key. Stored by the host service in an admin-only location.
pub struct HostKey(KeyPair);

impl HostKey {
    pub fn generate() -> Result<Self, FleetError> {
        Ok(Self(KeyPair::generate_for(&PKCS_ED25519)?))
    }
    pub fn from_pem(pem: &str) -> Result<Self, FleetError> {
        Ok(Self(KeyPair::from_pem(pem)?))
    }
    pub fn to_pem(&self) -> String {
        self.0.serialize_pem()
    }
    /// The public half (SubjectPublicKeyInfo DER), sent to the server to get a certificate.
    pub fn public_der(&self) -> Vec<u8> {
        use rcgen::PublicKeyData;
        self.0.subject_public_key_info()
    }
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, FleetError> {
        use rcgen::SigningKey;
        Ok(self.0.sign(msg)?)
    }
}

/// An org's certificate authority.
pub struct FleetCa {
    org_id: OrgId,
    issuer: Issuer<'static, KeyPair>,
    key_pem: String,
    cert_pem: String,
}

impl FleetCa {
    pub fn generate(org_id: OrgId) -> Result<Self, FleetError> {
        let key = KeyPair::generate_for(&PKCS_ED25519)?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, format!("rconn fleet CA {org_id}"));
        params
            .distinguished_name
            .push(DnType::OrganizationName, org_id.to_string());
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + time::Duration::days(3650);
        let cert_pem = params.self_signed(&key)?.pem();
        let key_pem = key.serialize_pem();
        Ok(Self {
            org_id,
            issuer: Issuer::new(params, key),
            key_pem,
            cert_pem,
        })
    }

    /// Reload a CA saved with [`cert_pem`](Self::cert_pem) and [`key_pem`](Self::key_pem).
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, FleetError> {
        let org_id = claimed_org(cert_pem)?;
        let issuer = Issuer::from_ca_cert_pem(cert_pem, KeyPair::from_pem(key_pem)?)?;
        Ok(Self {
            org_id,
            issuer,
            key_pem: key_pem.to_owned(),
            cert_pem: cert_pem.to_owned(),
        })
    }

    pub fn org_id(&self) -> OrgId {
        self.org_id
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// The CA private key. Store it admin-only; anyone holding it can enroll hosts.
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Issue a host certificate for `host_key`'s public half.
    pub fn issue_host_cert(
        &self,
        host_id: HostId,
        host_key: &HostKey,
    ) -> Result<String, FleetError> {
        self.issue_with_validity(host_id, host_key, HOST_CERT_LIFETIME)
    }

    /// Issue a host certificate from only the host's public key (SubjectPublicKeyInfo DER),
    /// so the private key never leaves the host. Only Ed25519 keys are accepted.
    pub fn issue_host_cert_for_public_key(
        &self,
        host_id: HostId,
        spki_der: &[u8],
    ) -> Result<String, FleetError> {
        let spki = rcgen::SubjectPublicKeyInfo::from_der(spki_der)?;
        let cert = self.issue_with_validity_for(host_id, &spki, HOST_CERT_LIFETIME)?;
        // Re-verify: rejects anything that isn't a valid Ed25519 host cert for this CA.
        verify_host_cert(&self.cert_pem, &cert)?;
        Ok(cert)
    }

    fn issue_with_validity(
        &self,
        host_id: HostId,
        host_key: &HostKey,
        life: Duration,
    ) -> Result<String, FleetError> {
        self.issue_with_validity_for(host_id, &host_key.0, life)
    }

    fn issue_with_validity_for(
        &self,
        host_id: HostId,
        subject_key: &impl rcgen::PublicKeyData,
        life: Duration,
    ) -> Result<String, FleetError> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(DnType::CommonName, host_id.to_string());
        params
            .distinguished_name
            .push(DnType::OrganizationName, self.org_id.to_string());
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + life;
        Ok(params.signed_by(subject_key, &self.issuer)?.pem())
    }
}

/// Verify that `cert_pem` was issued by the CA in `ca_pem` and is currently valid.
pub fn verify_host_cert(ca_pem: &str, cert_pem: &str) -> Result<HostClaims, FleetError> {
    let ca_der = pem_der(ca_pem)?;
    let cert_der = pem_der(cert_pem)?;
    let (_, ca) = X509Certificate::from_der(&ca_der).map_err(|_| FleetError::Parse)?;
    let (_, cert) = X509Certificate::from_der(&cert_der).map_err(|_| FleetError::Parse)?;

    if !ca.is_ca() || cert.issuer() != ca.subject() {
        return Err(FleetError::WrongIssuer);
    }
    cert.verify_signature(Some(ca.public_key()))
        .map_err(|_| FleetError::WrongIssuer)?;
    if !cert.validity().is_valid() || !ca.validity().is_valid() {
        return Err(FleetError::Expired);
    }

    let field = |c: &X509Certificate<'_>, cn: bool| -> Option<Uuid> {
        let subject = c.subject();
        let attr = if cn {
            subject.iter_common_name().next()
        } else {
            subject.iter_organization().next()
        };
        attr?.as_str().ok()?.parse().ok()
    };
    let host_id = field(&cert, true).ok_or(FleetError::BadSubject)?;
    let org_id = field(&cert, false).ok_or(FleetError::BadSubject)?;
    if field(&ca, false) != Some(org_id) {
        return Err(FleetError::WrongIssuer);
    }

    let spki = cert.public_key();
    if spki.algorithm.algorithm != OID_SIG_ED25519 {
        return Err(FleetError::BadKey);
    }
    let public_key: [u8; 32] = spki
        .subject_public_key
        .data
        .as_ref()
        .try_into()
        .map_err(|_| FleetError::BadKey)?;
    Ok(HostClaims {
        host_id: HostId(host_id),
        org_id: OrgId(org_id),
        public_key,
    })
}

/// Read the org a host certificate *claims*, without verifying it. Only for choosing
/// which org's CA to verify against; never trust the result on its own.
pub fn claimed_org(cert_pem: &str) -> Result<OrgId, FleetError> {
    let der = pem_der(cert_pem)?;
    let (_, cert) = X509Certificate::from_der(&der).map_err(|_| FleetError::Parse)?;
    let org = cert
        .subject()
        .iter_organization()
        .next()
        .and_then(|a| a.as_str().ok())
        .and_then(|s| s.parse().ok());
    org.map(OrgId).ok_or(FleetError::BadSubject)
}

fn pem_der(pem: &str) -> Result<Vec<u8>, FleetError> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|_| FleetError::Parse)?;
    Ok(p.contents)
}

/// The exact bytes a host signs to authenticate. Every field is length-prefixed, so no
/// two different inputs produce the same transcript.
///
/// * `server_id`: the server's canonical identity as the *host* has it configured (its
///   server profile URL). The server uses its own configured identity; if they differ,
///   verification fails.
/// * `cert_pem`: hashed, binding the proof to the certificate presented.
/// * both nonces: the server's makes the proof fresh; the client's stops a server from
///   choosing the whole signed message.
pub fn host_proof_transcript(
    server_id: &str,
    host_id: HostId,
    cert_pem: &str,
    client_nonce: &[u8],
    server_nonce: &[u8],
) -> Vec<u8> {
    let mut t = Vec::with_capacity(256);
    let mut field = |bytes: &[u8]| {
        t.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        t.extend_from_slice(bytes);
    };
    field(HOST_AUTH_DOMAIN);
    field(&PROTOCOL_VERSION.to_be_bytes());
    field(server_id.as_bytes());
    field(host_id.0.as_bytes());
    field(&Sha256::digest(cert_pem.as_bytes()));
    field(client_nonce);
    field(server_nonce);
    t
}

/// Check a host's signature over its transcript with the key from its verified cert.
pub fn verify_host_proof(
    claims: &HostClaims,
    transcript: &[u8],
    signature: &[u8],
) -> Result<(), FleetError> {
    let key = ed25519_dalek::VerifyingKey::from_bytes(&claims.public_key)
        .map_err(|_| FleetError::BadKey)?;
    let sig =
        ed25519_dalek::Signature::from_slice(signature).map_err(|_| FleetError::BadSignature)?;
    key.verify_strict(transcript, &sig)
        .map_err(|_| FleetError::BadSignature)
}

#[cfg(test)]
mod public_key_enroll_tests {
    use super::*;

    #[test]
    fn a_cert_issued_from_the_public_key_alone_verifies_for_that_key() {
        let ca = FleetCa::generate(OrgId::new()).unwrap();
        let key = HostKey::generate().unwrap();
        let id = HostId::new();
        let cert = ca
            .issue_host_cert_for_public_key(id, &key.public_der())
            .unwrap();
        let claims = verify_host_cert(ca.cert_pem(), &cert).unwrap();
        assert_eq!(claims.host_id, id);
        assert!(ca.issue_host_cert_for_public_key(id, b"not a key").is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enrolled() -> (FleetCa, HostId, HostKey, String) {
        let ca = FleetCa::generate(OrgId::new()).unwrap();
        let host_id = HostId::new();
        let key = HostKey::generate().unwrap();
        let cert = ca.issue_host_cert(host_id, &key).unwrap();
        (ca, host_id, key, cert)
    }

    #[test]
    fn issued_cert_verifies_and_carries_identity() {
        let (ca, host_id, _, cert) = enrolled();
        let claims = verify_host_cert(ca.cert_pem(), &cert).unwrap();
        assert_eq!(claims.host_id, host_id);
        assert_eq!(claims.org_id, ca.org_id());
    }

    #[test]
    fn cert_from_another_orgs_ca_is_rejected() {
        let (_, _, _, cert) = enrolled();
        let other = FleetCa::generate(OrgId::new()).unwrap();
        assert!(matches!(
            verify_host_cert(other.cert_pem(), &cert),
            Err(FleetError::WrongIssuer)
        ));
    }

    #[test]
    fn expired_cert_is_rejected() {
        let ca = FleetCa::generate(OrgId::new()).unwrap();
        let key = HostKey::generate().unwrap();
        // not_before is 5 minutes ago, so a zero lifetime yields a cert that already expired.
        let cert = ca
            .issue_with_validity(HostId::new(), &key, Duration::ZERO)
            .unwrap();
        assert!(matches!(
            verify_host_cert(ca.cert_pem(), &cert),
            Err(FleetError::Expired)
        ));
    }

    #[test]
    fn proof_round_trip_and_binding() {
        let (ca, host_id, key, cert) = enrolled();
        let claims = verify_host_cert(ca.cert_pem(), &cert).unwrap();
        let (cn, sn) = ([1u8; 32], [2u8; 32]);
        let t = host_proof_transcript("wss://fleet.example", host_id, &cert, &cn, &sn);
        let sig = key.sign(&t).unwrap();
        verify_host_proof(&claims, &t, &sig).unwrap();

        // Any change to the bound context invalidates the same signature.
        for other in [
            host_proof_transcript("wss://evil.example", host_id, &cert, &cn, &sn),
            host_proof_transcript("wss://fleet.example", HostId::new(), &cert, &cn, &sn),
            host_proof_transcript("wss://fleet.example", host_id, &cert, &cn, &[3u8; 32]),
            host_proof_transcript("wss://fleet.example", host_id, &cert, &[9u8; 32], &sn),
        ] {
            assert!(matches!(
                verify_host_proof(&claims, &other, &sig),
                Err(FleetError::BadSignature)
            ));
        }
    }

    #[test]
    fn a_different_hosts_key_cannot_prove_for_this_cert() {
        let (ca, host_id, _, cert) = enrolled();
        let claims = verify_host_cert(ca.cert_pem(), &cert).unwrap();
        let t = host_proof_transcript("s", host_id, &cert, &[0; 32], &[0; 32]);
        let thief = HostKey::generate().unwrap();
        assert!(verify_host_proof(&claims, &t, &thief.sign(&t).unwrap()).is_err());
    }

    #[test]
    fn host_key_pem_round_trips() {
        let key = HostKey::generate().unwrap();
        let again = HostKey::from_pem(&key.to_pem()).unwrap();
        assert_eq!(key.to_pem(), again.to_pem());
    }

    #[test]
    fn reloaded_ca_issues_certs_that_verify_against_the_original() {
        let ca = FleetCa::generate(OrgId::new()).unwrap();
        let again = FleetCa::from_pem(ca.cert_pem(), ca.key_pem()).unwrap();
        assert_eq!(again.org_id(), ca.org_id());
        let key = HostKey::generate().unwrap();
        let id = HostId::new();
        let cert = again.issue_host_cert(id, &key).unwrap();
        assert_eq!(verify_host_cert(ca.cert_pem(), &cert).unwrap().host_id, id);
    }
}
