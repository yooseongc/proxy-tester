use std::{io::Cursor, sync::Arc};

use anyhow::{Context, bail};
use proxy_tester_domain::{Protocol, Scenario, TlsVersion};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
    SupportedProtocolVersion,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
    version,
};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Debug)]
struct NoCertificateVerification;

impl ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

pub(crate) async fn connect(
    stream: TcpStream,
    scenario: &Scenario,
) -> anyhow::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let builder = ClientConfig::builder_with_provider(crypto_provider(scenario)?)
        .with_protocol_versions(protocol_versions(scenario.tls.version))?;
    let mut config = if scenario.tls.verify_peer {
        let mut roots = RootCertStore::empty();
        let pem = scenario.tls.ca_pem.as_deref().context("CA PEM required")?;
        let certificates = rustls_pemfile::certs(&mut Cursor::new(pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()?;
        let (added, _) = roots.add_parsable_certificates(certificates);
        if added == 0 {
            bail!("CA PEM contains no usable certificates")
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoCertificateVerification))
            .with_no_client_auth()
    };
    if scenario.protocol == Protocol::Http2 {
        config.alpn_protocols = vec![b"h2".to_vec()];
    }
    let server_name = ServerName::try_from(scenario.tls.server_name.clone())?;
    let stream = TlsConnector::from(Arc::new(config))
        .connect(server_name, stream)
        .await?;
    if scenario.protocol == Protocol::Http2 && stream.get_ref().1.alpn_protocol() != Some(b"h2") {
        bail!("TLS ALPN negotiation did not select h2");
    }
    Ok(stream)
}

pub(crate) fn acceptor(scenario: &Scenario) -> anyhow::Result<TlsAcceptor> {
    let certificate_pem = scenario
        .tls
        .server_cert_pem
        .as_deref()
        .context("server certificate required")?;
    let key_pem = scenario
        .tls
        .server_key_pem
        .as_deref()
        .context("server private key required")?;
    let certificates = rustls_pemfile::certs(&mut Cursor::new(certificate_pem.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut Cursor::new(key_pem.as_bytes()))?
        .context("server private key PEM is empty")?;
    let mut config = ServerConfig::builder_with_provider(crypto_provider(scenario)?)
        .with_protocol_versions(protocol_versions(scenario.tls.version))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    if scenario.protocol == Protocol::Http2 {
        config.alpn_protocols = vec![b"h2".to_vec()];
    }
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn crypto_provider(scenario: &Scenario) -> anyhow::Result<Arc<CryptoProvider>> {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    if let Some(cipher) = scenario.tls.cipher_suite.as_deref() {
        provider
            .cipher_suites
            .retain(|suite| format!("{:?}", suite.suite()) == cipher);
        if provider.cipher_suites.is_empty() {
            bail!("configured TLS cipher suite is unavailable: {cipher}");
        }
    }
    Ok(Arc::new(provider))
}

fn protocol_versions(version: TlsVersion) -> &'static [&'static SupportedProtocolVersion] {
    static TLS12: [&SupportedProtocolVersion; 1] = [&version::TLS12];
    static TLS13: [&SupportedProtocolVersion; 1] = [&version::TLS13];
    match version {
        TlsVersion::Tls12 => &TLS12,
        TlsVersion::Tls13 => &TLS13,
    }
}
