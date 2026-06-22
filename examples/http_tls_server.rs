//! HTTP/2 TLS server example with self-signed certificates.
//!
//! Run with:
//! ```
//! cargo run --example http_tls_server
//! ```
//!
//! Test with:
//! ```
//! curl -k --http2 https://127.0.0.1:8443
//! ```

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rust_miniss::hyper_compat::{HyperExecutor, TcpIo};
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::MultiCoreRuntime;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

async fn handle(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from(
        "Hello from rust-miniss HTTP/2 TLS!",
    ))))
}

fn main() {
    // Install rustls crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let addr: SocketAddr = "127.0.0.1:8443".parse().unwrap();
    println!("Starting HTTP/2 TLS server on {addr}");
    println!("Test with: curl -k --http2 https://{addr}");

    // Generate self-signed certificate
    let cert_key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_der = cert_key.cert.der().clone();
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert_key.key_pair.serialize_der());

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert_der],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
        )
        .unwrap();
    tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    let tls_config = Arc::new(tls_config);
    let executor = HyperExecutor::new();
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    runtime.block_on(async {
        let listener = AsyncTcpListener::bind(addr).unwrap();
        println!("Listening on {addr}");

        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    println!("Connection from {peer:?}");
                    let tls_config = tls_config.clone();
                    executor.spawn(async move {
                        let io = TcpIo::from_async(stream);
                        let tls = tokio_rustls::TlsAcceptor::from(tls_config);
                        match tls.accept(io).await {
                            Ok(tls_stream) => {
                                let io = TokioIo::new(tls_stream);
                                if let Err(e) =
                                    hyper::server::conn::http2::Builder::new(HyperExecutor::new())
                                        .serve_connection(io, service_fn(handle))
                                        .await
                                {
                                    eprintln!("HTTP/2 error: {e}");
                                }
                            }
                            Err(e) => {
                                eprintln!("TLS error: {e}");
                            }
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Accept error: {e}");
                    break;
                }
            }
        }
    });

    runtime.shutdown().unwrap();
}
