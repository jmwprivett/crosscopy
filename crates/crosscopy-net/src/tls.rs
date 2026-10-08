//! rustls configuration that trusts device identities rather than CAs.
//!
//! Certificate-chain validation is replaced by a hash comparison, but the
//! handshake signature is still verified, which proves the peer holds the
//! private key for the certificate it presented.

use crate::identity::Identity;
use anyhow::Result;
use crosscopy_core::DeviceId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Name presented in SNI. Never validated; identity comes from the pin.
pub const SERVER_NAME: &str = "crosscopy";

/// The set of paired devices, shared with the verifier so pairing changes
/// apply to new connections without rebuilding the endpoint.
pub type PinSet = Arc<RwLock<HashSet<DeviceId>>>;

#[derive(Debug)]
pub enum Trust {
    /// Any paired device (sync server).
    Pinned(PinSet),
    /// Exactly this device (sync client dialing a known peer).
    Exactly(DeviceId),
    /// Anyone holding the key for their certificate. Only for pairing,
    /// where the user then verifies the confirmation code.
    Any,
}

impl Trust {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let actual = DeviceId::from_cert(cert);
        let ok = match self {
            Trust::Pinned(pins) => pins.read().unwrap().contains(&actual),
            Trust::Exactly(expected) => *expected == actual,
            Trust::Any => true,
        };
        if ok {
            return Ok(());
        }
        match self {
            Trust::Exactly(expected) => {
                tracing::warn!(%expected, %actual, "peer presented an unexpected identity");
            }
            _ => tracing::warn!(code = %actual, "rejected connection from unpaired device"),
        }
        Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn server_config(identity: &Identity, trust: Trust, alpn: &[u8]) -> Result<rustls::ServerConfig> {
    let provider = provider();
    let verifier = Arc::new(Verifier { trust, provider: provider.clone() });
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.cert.clone()], identity.key())?;
    config.alpn_protocols = vec![alpn.to_vec()];
    // A resumed session skips certificate verification, which would let an
    // unpaired device back in. Handshakes are rare and cheap; always do full ones.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(config)
}

pub fn client_config(identity: &Identity, trust: Trust, alpn: &[u8]) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let verifier = Arc::new(Verifier { trust, provider: provider.clone() });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key())?;
    config.alpn_protocols = vec![alpn.to_vec()];
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

#[derive(Debug)]
struct Verifier {
    trust: Trust,
    provider: Arc<CryptoProvider>,
}

impl Verifier {
    fn tls12(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn tls13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.trust.check(end_entity).map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Verifier {
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
        self.trust.check(end_entity).map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

/// Reads the verified peer identity off an established connection.
pub fn peer_id(conn: &quinn::Connection) -> Option<DeviceId> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(|cert| DeviceId::from_cert(cert))
}
