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
    /// Uses the system's default root certificates for server verification.
    pub async fn connect(domain: &str, stream: AsyncTcpStream) -> io::Result<Self> {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

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
