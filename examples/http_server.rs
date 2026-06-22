//! HTTP server example using hyper on rust-miniss.
//!
//! Supports HTTP/1.1, HTTP/2, and HTTP/3.
//!
//! Run with:
//! ```
//! cargo run --example http_server -- [OPTIONS]
//! ```
//!
//! Options:
//!   --http1       Run HTTP/1.1 server (default)
//!   --http2       Run HTTP/2 server
//!   --http3       Run HTTP/3 server (requires TLS)
//!   --port PORT   Port to listen on (default: 8080)
//!   --addr ADDR   Address to bind to (default: 127.0.0.1)
//!
//! Test with:
//! ```
//! curl http://127.0.0.1:8080           # HTTP/1.1
//! curl --http2 http://127.0.0.1:8080   # HTTP/2
//! ```

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use rust_miniss::hyper_compat::{HyperExecutor, TcpIo};
use rust_miniss::net::AsyncTcpListener;
use rust_miniss::MultiCoreRuntime;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

async fn handle(_req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from(
        "Hello from rust-miniss!",
    ))))
}

fn main() {
    let mut http_version = "http1";
    let mut port = 8080u16;
    let mut addr = "127.0.0.1".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http1" => http_version = "http1",
            "--http2" => http_version = "http2",
            "--http3" => http_version = "http3",
            "--port" => {
                if let Some(p) = args.next() {
                    port = p.parse().expect("Invalid port");
                }
            }
            "--addr" => {
                if let Some(a) = args.next() {
                    addr = a;
                }
            }
            _ => {
                eprintln!("Unknown option: {arg}");
                eprintln!(
                    "Usage: http_server [--http1|--http2|--http3] [--port PORT] [--addr ADDR]"
                );
                std::process::exit(1);
            }
        }
    }

    let listen_addr: SocketAddr = format!("{addr}:{port}").parse().unwrap();

    match http_version {
        "http1" => run_http1(listen_addr),
        "http2" => run_http2(listen_addr),
        "http3" => run_http3(listen_addr),
        _ => unreachable!(),
    }
}

fn run_http1(addr: SocketAddr) {
    println!("Starting HTTP/1.1 server on {addr}");
    println!("Test with: curl http://{addr}");

    let executor = HyperExecutor::new();
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    runtime.block_on(async {
        let listener = AsyncTcpListener::bind(addr).unwrap();
        println!("Listening on {addr}");

        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    println!("Connection from {peer:?}");
                    executor.spawn(async move {
                        let io = TcpIo::from_async(stream);
                        if let Err(e) = hyper::server::conn::http1::Builder::new()
                            .serve_connection(io, service_fn(handle))
                            .await
                        {
                            eprintln!("HTTP error: {e}");
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

fn run_http2(addr: SocketAddr) {
    println!("Starting HTTP/2 server on {addr}");
    println!("Note: HTTP/2 requires TLS for proper operation");
    println!("Use http_tls_server example for HTTP/2 with TLS");

    let executor = HyperExecutor::new();
    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    // Generate self-signed certificate
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
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
    let tls_config = std::sync::Arc::new(tls_config);

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
                                let io = hyper_util::rt::TokioIo::new(tls_stream);
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

fn run_http3(addr: SocketAddr) {
    println!("Starting HTTP/3 server on {addr}");
    println!("Note: HTTP/3 requires a valid TLS certificate");

    let runtime = MultiCoreRuntime::new(Some(1)).unwrap();

    runtime.block_on(async {
        // Generate self-signed certificate for testing
        let cert_key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = cert_key.cert.der().clone();
        let key_der =
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert_key.key_pair.serialize_der());

        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert_der],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
            )
            .unwrap();
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls_config).unwrap(),
        ));

        let endpoint = quinn::Endpoint::server(server_config, addr).unwrap();
        println!("HTTP/3 server listening on {addr}");

        while let Some(conn) = endpoint.accept().await {
            println!("New QUIC connection");
            tokio::spawn(async move {
                match conn.await {
                    Ok(conn) => {
                        println!("QUIC connection established");
                        let mut h3_conn =
                            h3::server::Connection::new(h3_quinn::Connection::new(conn))
                                .await
                                .unwrap();

                        loop {
                            match h3_conn.accept().await {
                                Ok(Some(resolver)) => {
                                    let (req, mut stream) =
                                        resolver.resolve_request().await.unwrap();
                                    println!("H3 request: {} {}", req.method(), req.uri());

                                    let response = Response::builder()
                                        .status(http::StatusCode::OK)
                                        .body(())
                                        .unwrap();
                                    stream.send_response(response).await.unwrap();
                                    stream
                                        .send_data(Bytes::from("Hello from rust-miniss HTTP/3!"))
                                        .await
                                        .unwrap();
                                    stream.finish().await.unwrap();
                                }
                                Ok(None) => break,
                                Err(e) => {
                                    eprintln!("H3 accept error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("QUIC connection error: {e}");
                    }
                }
            });
        }
    });

    runtime.shutdown().unwrap();
}
