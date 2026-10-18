//! TLS support via rustls.
//!
//! Provides [`TlsStream`] for making HTTPS connections over [`AsyncTcpStream`].
//!
//! # Example
//!
//! ```rust,no_run
//! use rust_miniss::net::AsyncTcpStream;
//! use rust_miniss::tls::TlsStream;
//!
//! # async fn example() {
//! let addr: std::net::SocketAddr = "1.1.1.1:443".parse().unwrap();
//! let tcp = AsyncTcpStream::connect(addr).await.unwrap();
//! let tls = TlsStream::connect("cloudflare-dns.com", tcp).await.unwrap();
//! # }
//! ```

use crate::compat::FuturesAsyncReadCompatExt;
use crate::net::AsyncTcpStream;
use rustls::pki_types::ServerName;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// A TLS-encrypted TCP stream.
pub struct TlsStream {
    inner: Compat<tokio_rustls::client::TlsStream<Compat<AsyncTcpStream>>>,
}

impl TlsStream {
    /// Performs a TLS handshake over the given TCP stream.
    ///
    /// Uses the bundled Mozilla/WebPKI root certificates for server verification.
    pub async fn connect(domain: &str, stream: AsyncTcpStream) -> io::Result<Self> {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let config = default_client_config(root_store)?;

        Self::connect_with_config(domain, stream, config).await
    }

    /// Performs a TLS handshake with a custom [`rustls::ClientConfig`].
    pub async fn connect_with_config(
        domain: &str,
        stream: AsyncTcpStream,
        config: rustls::ClientConfig,
    ) -> io::Result<Self> {
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let domain_name = ServerName::try_from(domain.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let compat_stream = stream.compat();
        let tls_stream = connector
            .connect(domain_name, compat_stream)
            .await
            .map_err(io::Error::other)?;

        Ok(Self {
            inner: tls_stream.compat(),
        })
    }
}

impl futures::io::AsyncRead for TlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl futures::io::AsyncWrite for TlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

fn default_client_config(root_store: rustls::RootCertStore) -> io::Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)
        .map(|builder| {
            builder
                .with_root_certificates(root_store)
                .with_no_client_auth()
        })
}

#[cfg(test)]
mod tests {
    use super::{default_client_config, TlsStream};
    use crate::{net::AsyncTcpStream, timer::timeout, Runtime};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn default_client_configuration_negotiates_tls12_with_tls12_only_peer() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let certificate: CertificateDer<'static> = cert.cert.into();
        let key = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS12])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .unwrap();
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add(certificate).unwrap();
        let client_config = default_client_config(root_store).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::time::timeout(Duration::from_secs(5), async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let (stream, _) = listener.accept().await.unwrap();
                    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
                    let stream = acceptor.accept(stream).await.unwrap();
                    assert_eq!(
                        stream.get_ref().1.protocol_version(),
                        Some(rustls::ProtocolVersion::TLSv1_2)
                    );
                })
                .await
                .expect("TLS 1.2-only server timed out");
            });
        });

        let runtime = Runtime::new();
        let client_result = runtime.block_on(timeout(Duration::from_secs(4), async move {
            let tcp = AsyncTcpStream::connect(addr).await.unwrap();
            let _stream = TlsStream::connect_with_config("localhost", tcp, client_config)
                .await
                .unwrap();
        }));
        server.join().expect("TLS 1.2 server thread panicked");
        client_result.expect("TLS 1.2 client handshake timed out");
    }
}
