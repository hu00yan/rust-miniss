#[cfg(test)]
mod buffered_overflow_tests {
    use super::super::net::BufferedUdpSocket;
    use std::net::UdpSocket;
    use std::io::{self, ErrorKind};
    use std::net::SocketAddr;
    use std::io::IoSlice;

    #[tokio::test]
    async fn test_buffered_queue_overflow() {
        let rt = Runtime::new();
        rt.block_on(async {
            let socket = AsyncUdpSocket::bind("127.0.0.1:0").unwrap();
            let mut buffered = BufferedUdpSocket::new(socket);
            let peer = "127.0.0.1:9090".parse().unwrap();
            buffered.peer_addr = Some(peer);

            // Enqueue multiple small datagrams to overflow initial (simulate sender)
            for msg in ["msg1", "msg2", "msg3"].iter() {
                let bytes = msg.as_bytes();
                let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
                sender.send_to(bytes, peer).unwrap();  // Mock send
                buffered.recv_buffered(1024).await.unwrap();  // Queue
            }

            // Read partial (overflow test: extend > initial)
            let mut buf = [0u8; 10];  // Smaller than some datagrams
            let n1 = buffered.poll_read(Pin::new(&mut buffered), &mut Context::from_waker(&noop_waker()), &mut buf).await.unwrap();
            assert!(n1 > 0);  // Drains partial from queue
            let remaining = buffered.buffer.len();  // Assert remaining queued
            assert!(remaining > 0);  // Not all drained

            // Full drain on second read
            let n2 = buffered.poll_read(Pin::new(&mut buffered), &mut Context::from_waker(&noop_waker()), &mut buf).await.unwrap();
            assert!(n2 > 0);
        });
    }

    #[tokio::test]
    async fn test_buffered_no_peer_write() {
        let rt = Runtime::new();
        rt.block_on(async {
            let socket = AsyncUdpSocket::bind("127.0.0.1:0").unwrap();
            let mut buffered = BufferedUdpSocket::new(socket);
            let mut buf = [0u8; 5];

            // Write without peer (buffers)
            let n = buffered.write_all(&mut buf).await.unwrap();
            assert_eq!(n, 5);  // Buffered, no send

            // Flush without peer (error)
            let err = buffered.flush().await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::AddrNotAvailable);

            // Set peer, flush succeeds
            buffered.peer_addr = Some("127.0.0.1:9090".parse().unwrap());
            buffered.flush().await.unwrap();  // Sends queued
        });
    }

    fn noop_waker() -> std::task::Wake {
        std::task::Wake::noop()
    }
}</xai:function_call ><xai:function_call name="write">
<parameter name="filePath">/home/wuyan19981/rust-miniss/tests/vectored_edge.rs