#[cfg(test)]
mod metrics_mock_tests {
    use super::super::net::AsyncTcpStream;
    use metrics::testing::{init_test_recorder, TestHandler};
    use metrics::query::Query;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_metrics_query_mock() {
        let recorder = init_test_recorder();
        let handler = Arc::new(TestHandler::new());
        metrics::set_global_recorder(recorder).unwrap();

        let rt = Runtime::new();
        rt.block_on(async {
            let stream = AsyncTcpStream::connect("127.0.0.1:80").await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;  // Emits read_latency, read_bytes_total

            // Query with mock
            let hist = Query::histogram("net.read_latency").unwrap();
            assert!(hist.samples().len() > 0);  // At least 1 sample

            let counter = Query::counter("net.read_bytes_total").unwrap();
            assert!(counter.total() > 0);  // Bytes emitted
        });
    }
}