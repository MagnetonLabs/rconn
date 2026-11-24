//! How a client decides to trust the signaling server's TLS certificate.

use anyhow::{Result, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerTrust {
    /// Public CAs (hosted service, or a self-hosted server behind a real certificate).
    WebPki,
    /// Self-hosted: accept exactly the server key with this `sha256:<hex>` SPKI fingerprint.
    Pinned(String),
}

impl ServerTrust {
    /// From a server profile's optional fingerprint.
    pub fn from_fingerprint(fp: Option<&str>) -> Self {
        fp.map_or(Self::WebPki, |f| Self::Pinned(f.to_ascii_lowercase()))
    }

    pub(crate) fn client_config(&self) -> Result<ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?;
        Ok(match self {
            Self::WebPki => {
                let roots = RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                };
                builder.with_root_certificates(roots).with_no_client_auth()
            }
            Self::Pinned(fp) => {
                if !fp.starts_with("sha256:") || fp.len() != 7 + 64 {
                    bail!("pinned fingerprint must look like sha256:<64 hex chars>");
                }
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(PinnedKey {
                        fingerprint: fp.clone(),
                        provider,
                    }))
                    .with_no_client_auth()
            }
        })
    }
}

/// Accepts only the pinned public key. Hostname and CA chain are deliberately not checked:
/// the pin *is* the identity. Handshake signatures are still verified, so possession of the
/// pinned key's private half is required.
#[derive(Debug)]
struct PinnedKey {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = rconn_core::tls::spki_fingerprint(end_entity).map_err(|_| {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        })?;
        if got == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            tracing::error!(expected = %self.fingerprint, %got, "server key does not match the pinned fingerprint");
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
