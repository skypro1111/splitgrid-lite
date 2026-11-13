//! TLS for `ssl: true`, over rustls with the ring provider.
//!
//! Matches the Electron driver (`rejectUnauthorized` unless the host is
//! loopback): a remote server's certificate is checked against the OS trust
//! store, while a local server — typically a dev container with a self-signed
//! certificate — is accepted unverified.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream};
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{self, WebPkiSupportedAlgorithms};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

pub fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[derive(Clone)]
pub struct MakeRustls(Arc<ClientConfig>);

impl MakeRustls {
    pub fn new(host: &str) -> anyhow::Result<Self> {
        let provider = Arc::new(crypto::ring::default_provider());
        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?;
        let config = if is_loopback(host) {
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAny(
                    provider.signature_verification_algorithms,
                )))
                .with_no_client_auth()
        } else {
            builder.with_platform_verifier()?.with_no_client_auth()
        };
        Ok(MakeRustls(Arc::new(config)))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> MakeTlsConnect<S> for MakeRustls {
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> io::Result<RustlsConnect> {
        let name = ServerName::try_from(domain.to_owned())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(RustlsConnect {
            config: self.0.clone(),
            name,
        })
    }
}

pub struct RustlsConnect {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> TlsConnect<S> for RustlsConnect {
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let connector = tokio_rustls::TlsConnector::from(self.config);
        Box::pin(async move { Ok(RustlsStream(connector.connect(self.name, stream).await?)) })
    }
}

pub struct RustlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> TlsStream for RustlsStream<S> {
    /// SCRAM channel binding is optional; servers fall back to plain SCRAM.
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

/// Skips the certificate check (loopback only) but still verifies the
/// handshake signatures, so the session keys are the server's.
#[derive(Debug)]
struct AcceptAny(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, tokio_rustls::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, tokio_rustls::rustls::Error> {
        crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}
