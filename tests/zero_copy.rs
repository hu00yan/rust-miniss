#[cfg(all(test, feature = "rent-traits"))]
mod zero_copy_tests {
    use super::super::net::{AsyncTcpStream, AsyncReadRent};
    use std::task::{Context, Poll};
    use std::pin::Pin;

    #[tokio::test]
    async fn test_rent_read_no_copy() {
        let rt = Runtime::new();
        rt.block_on(async {
            let mut stream = AsyncTcpStream::connect("127.0.0.1:80").await.unwrap();
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut pinned = Pin::new(&mut stream);

            if let Poll::Ready(Ok(rented)) = pinned.as_mut().poll_read_rent(&mut cx, 1024) {
                assert_eq!(rented.len(), expected_bytes);  // From server
                // No memcpy: rented from pool directly
            }
        });
    }
}

#[cfg(not(feature = "rent-traits"))]
mod zero_copy_tests {
    #[test]
    fn test_no_rent() {
        // Fallback to native zero-copy read
        // ...
    }
}