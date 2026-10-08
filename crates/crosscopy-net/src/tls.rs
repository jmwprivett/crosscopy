//! rustls configuration that trusts exactly the pinned peer certificates.
//!
//! Certificate-chain validation is replaced by a hash comparison, but the
//! handshake signature is still verified, which proves the peer holds the
//! private key for the certificate it presented.

use crate::identity::Identity;
use anyhow::Result;
use crosscopy_core::{ALPN, DeviceId};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::collections::HashSet;
use std::sync::Arc;

/// Name presented in SNI. Never validated; identity comes from the pin.
pub const SERVER_NAME: &str = "crosscopy";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn server_config(identity: &Identity, pinned: HashSet<DeviceId>) -> Result<rustls::ServerConfig> {
    let provider = provider();
    let verifier = Arc::new(PinnedClients { pinned, provider: provider.clone() });
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}

pub fn client_config(identity: &Identity, expected: DeviceId) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let verifier = Arc::new(PinnedServer { expected, provider: provider.clone() });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}

fn pin_mismatch() -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

#[derive(Debug)]
struct PinnedServer {
    expected: DeviceId,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = DeviceId::from_cert(end_entity);
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            tracing::warn!(expected = %self.expected, %actual, "peer presented an unexpected identity");
            Err(pin_mismatch())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedClients {
    pinned: HashSet<DeviceId>,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for PinnedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let actual = DeviceId::from_cert(end_entity);
        if self.pinned.contains(&actual) {
            Ok(ClientCertVerified::assertion())
        } else {
            tracing::warn!(code = %actual, "rejected connection from unknown device");
            Err(pin_mismatch())
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
