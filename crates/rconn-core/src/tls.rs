//! Server TLS identity for self-hosted signaling servers.
//!
//! A self-hosted server generates its own key pair and certificate at `init`. Clients
//! don't validate it against public CAs; they **pin** the SHA-256 of its public key
//! (SubjectPublicKeyInfo), printed at `init` and stored in each client's server profile.
//! Pinning the key rather than the certificate survives certificate renewal with the
//! same key. Hosted servers use ordinary WebPKI certificates instead.

use rcgen::{CertificateParams, DnType, KeyPair};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x509_parser::prelude::{FromDer, X509Certificate};

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("certificate is not valid X.509")]
    Parse,
}

/// A freshly generated server certificate and key, both PEM.
pub struct ServerTls {
    pub cert_pem: String,
    pub key_pem: String,
    /// `sha256:<hex>` of the certificate's public key.
    pub fingerprint: String,
}

/// Generate a self-signed server certificate (ECDSA P-256) naming `hostnames`.
/// Names don't matter to pinned clients but help browsers and debugging tools.
pub fn generate_server_tls(hostnames: &[String]) -> Result<ServerTls, TlsError> {
    let mut names = hostnames.to_vec();
    if names.is_empty() {
        names.push("rconn-signaling".into());
    }
    let mut params = CertificateParams::new(names)?;
    params
        .distinguished_name
        .push(DnType::CommonName, "rconn signaling");
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(3650);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let fingerprint = spki_fingerprint(cert.der())?;
    Ok(ServerTls {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        fingerprint,
    })
}

/// `sha256:<hex>` of a DER certificate's SubjectPublicKeyInfo.
pub fn spki_fingerprint(cert_der: &[u8]) -> Result<String, TlsError> {
    let (_, cert) = X509Certificate::from_der(cert_der).map_err(|_| TlsError::Parse)?;
    let digest = Sha256::digest(cert.public_key().raw);
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    ))
}

/// The same fingerprint from a PEM certificate.
pub fn spki_fingerprint_pem(cert_pem: &str) -> Result<String, TlsError> {
    let (_, pem) =
        x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).map_err(|_| TlsError::Parse)?;
    spki_fingerprint(&pem.contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_matches_pem() {
        let tls = generate_server_tls(&["fleet.example".into()]).unwrap();
        assert!(tls.fingerprint.starts_with("sha256:") && tls.fingerprint.len() == 7 + 64);
        assert_eq!(
            spki_fingerprint_pem(&tls.cert_pem).unwrap(),
            tls.fingerprint
        );
        let other = generate_server_tls(&[]).unwrap();
        assert_ne!(other.fingerprint, tls.fingerprint);
    }
}
