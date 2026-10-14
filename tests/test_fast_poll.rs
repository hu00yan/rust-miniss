//! Test Fast Poll (IORING_OP_POLL_ADD multishot) functionality

#[cfg(all(test, target_os = "linux", io_backend = "io_uring"))]
mod fast_poll_tests {
    use rust_miniss::io::Op;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    #[test]
    fn test_poll_add_oneshot() {
        let runtime = rust_miniss::Runtime::new();
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
            listener
                .set_nonblocking(true)
                .expect("Failed to set nonblocking");
            let fd = listener.as_raw_fd();
            let addr = listener.local_addr().expect("Failed to get listener addr");

            rust_miniss::cpu::with_io_state(|state| {
                // Submit a oneshot poll for readability
                let op = Op::PollAdd {
                    fd,
                    events: libc::POLLIN as u32,
                    multishot: false,
                };

                let _token = state.io_backend.submit(op).expect("submit oneshot poll");
                state.io_backend.flush();
                let _client = TcpStream::connect(addr).expect("Failed to connect");
                state.io_backend.park(Duration::from_millis(10));

                // Poll for completion
                let completions = match state.io_backend.drain_completions() {
                    std::task::Poll::Ready(comps) => comps,
                    std::task::Poll::Pending => {
                        // Try again after a short delay
                        std::thread::sleep(Duration::from_millis(10));
                        match state.io_backend.drain_completions() {
                            std::task::Poll::Ready(comps) => comps,
                            std::task::Poll::Pending => panic!("PollAdd did not complete"),
                        }
                    }
                };

                assert!(!completions.is_empty(), "Expected at least one completion");

                let (_, completion_result) = &completions[0];
                assert!(
                    completion_result.is_ok(),
                    "PollAdd failed: {:?}",
                    completion_result
                );
            });
        });
    }

    #[test]
    fn test_poll_add_multishot_completion_and_cancel() {
        let runtime = rust_miniss::Runtime::new();
        runtime.block_on(async {
            let (mut reader, mut writer) = UnixStream::pair().expect("create poll pair");
            reader.set_nonblocking(true).expect("set nonblocking");
            let fd = reader.as_raw_fd();

            rust_miniss::cpu::with_io_state(|state| {
                // Submit a multishot poll for readability
                let op = Op::PollAdd {
                    fd,
                    events: libc::POLLIN as u32,
                    multishot: true,
                };

                let token = state.io_backend.submit(op).expect("submit multishot poll");
                state.io_backend.flush();
                writer.write_all(b"x").expect("write readiness byte");
                let mut completions = None;
                for _ in 0..10 {
                    state.io_backend.park(Duration::from_millis(20));
                    if let std::task::Poll::Ready(comps) = state.io_backend.drain_completions() {
                        if !comps.is_empty() {
                            completions = Some(comps);
                            break;
                        }
                    }
                }
                let completions = completions.expect("multishot poll did not complete");
                assert_eq!(completions.len(), 1);
                assert!(completions[0].1.is_ok(), "multishot poll failed");
                let mut byte = [0u8; 1];
                reader
                    .read_exact(&mut byte)
                    .expect("consume readiness byte");

                // A MORE CQE must leave the operation and its descriptor
                // lease armed until explicit cancellation.
                assert!(state.io_backend.pending_operation_count() > 0);
                assert!(state.io_backend.cancel(token));
                state.io_backend.flush();
                for _ in 0..10 {
                    state.io_backend.park(Duration::from_millis(20));
                    let _ = state.io_backend.drain_completions();
                    if state.io_backend.pending_operation_count() == 0 {
                        break;
                    }
                }
                assert_eq!(state.io_backend.pending_operation_count(), 0);
            });
        });
    }
}
