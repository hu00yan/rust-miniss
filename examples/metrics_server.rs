use rust_miniss::{Runtime};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use std::convert::Infallible;
use std::net::SocketAddr;
use metrics_exporter_prometheus::{Encoder, PrometheusBuilder};

#[tokio::main]
async fn main() {
    let builder = PrometheusBuilder::new();
    builder.install_recorder().expect("Failed to install metrics recorder");

    // Expose /metrics
    async fn metrics_handler(_req: Request<Body>) -> Result<Response<Body>, Infallible> {
        let encoder = Encoder::new();
        let metric_families = metrics::describe_all();  // Collect all metrics
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).unwrap();
        Ok(Response::new(Body::from(buffer)))
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], 9090));
    let make_svc = make_service_fn(|_conn| async { Ok::<_, Infallible>(service_fn(metrics_handler)) });
    let server = Server::bind(&addr).serve(make_svc);

    println!("Metrics server running at http://{}", addr);

    // Run miniss runtime alongside (example IO to emit metrics)
    let rt = Runtime::new();
    rt.block_on(async {
        // Example: Connect and read to emit net.read_latency etc.
        if let Ok(stream) = rust_miniss::net::AsyncTcpStream::connect("127.0.0.1:80").await {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;  // Emits metrics
        }
    });

    if let Err(e) = server.await {
        eprintln!("Server error: {}", e);
    }
}