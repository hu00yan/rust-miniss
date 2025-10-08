#[cfg(all(test, feature = "rent-traits"))]
mod zero_copy_pin_tests {
    use super::super::net::{AsyncTcpStream, AsyncReadRent};
    use std::task::{Context, Poll};
    use std::pin::Pin;
    use std::net::TcpListener;
    use std::io::Write;

    #[tokio::test]
    async fn test_rent_buffer_pinning() {
        let rt = Runtime::new();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let mut server = listener.accept().unwrap().0;
            server.set_nonblocking(true).unwrap();
            let mut server_stream = AsyncTcpStream::from(server);

            let client = std::net::TcpStream::connect(addr).unwrap();
            client.write_all(b"pinned data").unwrap();

            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut pinned = Pin::new(&mut server_stream);

            if let Poll::Ready(Ok(rented)) = pinned.as_mut().poll_read_rent(&mut cx, 1024) {
                // Pin<Box<Buffer>> test: Box to pin, poll on pinned
                let boxed = Box::pin(rented.clone());
                let mut pinned_buf = Box::new(rented);  // Pin for rent
                let _ = pinned_buf.as_mut().poll_read_rent(&mut cx, 1024);  // No panic, pinned
                assert_eq!(pinned_buf.len(), expected_len);  // From server
                // Drop: Recycle via Drop impl
                drop(boxed);  // Assert pool counter (if added in buffer.rs)
            }
        });
    }

    #[tokio::test]
    async fn test_rent_drop_recycle() {
        let rt = Runtime::new();
        rt.block_on(async {
            let mut stream = AsyncTcpStream::connect("127.0.0.1:80").await.unwrap();
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            let mut pinned = Pin::new(&mut stream);

            // Rent and drop (assert recycle via pool counter if added)
            if let Poll::Ready(Ok(rented)) = pinned.as_mut().poll_read_rent(&mut cx, 1024) {
                drop(rented);  // Should recycle to pool
                // Assert pool hits increased (mock pool counter)
            }
        });
    }
}

#[cfg(not(feature = "rent-traits"))]
mod zero_copy_pin_tests {
    #[test]
    fn test_no_rent_fallback() {
        // Native zero-copy test (existing)
        // ...
    }
}