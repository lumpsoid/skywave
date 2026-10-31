//! TLS client configurations (spec §6, D9): mTLS with the pinned CA for a
//! paired device, and the fingerprint-pinned anonymous config for pairing.
//! Both use the ring provider, like the server.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use skywave_proto::Fingerprint;

use crate::identity::{DeviceKey, Identity};
use crate::transport::TransportError;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn builder(
    provider: Arc<CryptoProvider>,
) -> rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier> {
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("the ring provider supports the default protocol versions")
}

fn finish(mut config: ClientConfig) -> Arc<ClientConfig> {
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// The paired device's config: trust only the pinned CA, present the
/// device certificate.
pub(crate) fn device_config(identity: &dyn Identity) -> Result<Arc<ClientConfig>, TransportError> {
    let bad =
        |what: &str, e: &dyn fmt::Display| TransportError::BadIdentity(format!("{what}: {e}"));
    let provider = provider();
    let mut roots = RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(identity.ca_cert_pem().as_bytes()) {
        let ca = ca.map_err(|e| bad("CA certificate", &e))?;
        roots.add(ca).map_err(|e| bad("CA certificate", &e))?;
    }
    if roots.is_empty() {
        return Err(bad("CA certificate", &"no certificate in PEM"));
    }
    let chain = CertificateDer::pem_slice_iter(identity.device_cert_pem().as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| bad("device certificate", &e))?;
    if chain.is_empty() {
        return Err(bad("device certificate", &"no certificate in PEM"));
    }
    let key = signing_key(identity.device_key(), chain, &provider)?;
    let config = builder(provider)
        .with_root_certificates(roots)
        .with_client_cert_resolver(Arc::new(SingleCertAndKey::from(key)));
    Ok(finish(config))
}

/// The device's certificate and signer. A Keystore-backed [`DeviceKey`]
/// variant would build its [`CertifiedKey`] around a custom
/// [`rustls::sign::SigningKey`] here.
fn signing_key(
    key: DeviceKey<'_>,
    chain: Vec<CertificateDer<'static>>,
    provider: &CryptoProvider,
) -> Result<CertifiedKey, TransportError> {
    let bad = |e: &dyn fmt::Display| TransportError::BadIdentity(format!("device key: {e}"));
    match key {
        DeviceKey::Pem(pem) => {
            let der = PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(|e| bad(&e))?;
            CertifiedKey::from_der(chain, der, provider).map_err(|e| bad(&e))
        }
    }
}

/// The pairing config: no client certificate; the server is trusted only
/// if it presents the CA whose fingerprint the user entered (see
/// [`PinnedCaVerifier`]). `mismatch` is set when the handshake failed
/// because of the fingerprint.
pub(crate) fn pairing_config(
    expected: Fingerprint,
    mismatch: Arc<AtomicBool>,
) -> Arc<ClientConfig> {
    let provider = provider();
    let verifier = PinnedCaVerifier {
        expected,
        mismatch,
        provider: provider.clone(),
    };
    let config = builder(provider)
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    finish(config)
}

/// Trust on first use, anchored by the fingerprint: the server sends its
/// certificate followed by the CA (spec §6). The last certificate of the
/// chain must have the expected fingerprint; it then becomes the only
/// trust anchor for an ordinary WebPKI check of the leaf — signature,
/// validity period and the server name the user typed.
#[derive(Debug)]
struct PinnedCaVerifier {
    expected: Fingerprint,
    mismatch: Arc<AtomicBool>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let Some((ca, rest)) = intermediates.split_last() else {
            self.mismatch.store(true, Ordering::SeqCst);
            return Err(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ));
        };
        if Fingerprint::of_der(ca) != self.expected {
            self.mismatch.store(true, Ordering::SeqCst);
            return Err(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ));
        }
        let mut roots = RootCertStore::empty();
        roots.add(ca.clone().into_owned())?;
        let inner =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
                .build()
                .map_err(|e| rustls::Error::General(e.to_string()))?;
        inner.verify_server_cert(end_entity, rest, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
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
        rustls::crypto::verify_tls13_signature(
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

/// DER of the first certificate in `pem`.
pub(crate) fn first_cert_der(pem: &str) -> Option<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem.as_bytes()).next()?.ok()
}
