//! Test Fast Poll (IORING_OP_POLL_ADD multishot) functionality

#[cfg(all(test, target_os = "linux", feature = "io_uring"))]
mod fast_poll_tests {
    use rust_miniss::io::{IoProvider, Op};
    use std::net::TcpListener;
    use std::os::unix::io::AsRawFd;
    use std::time::Duration;

    #[test]
    fn test_poll_add_oneshot() {
        let runtime = rust_miniss::Runtime::new();
        runtime.block_on(async {
            // Create a TCP listener
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
            listener
                .set_nonblocking(true)
                .expect("Failed to set nonblocking");

            let fd = listener.as_raw_fd();

            // Get the IO backend
            let state = rust_miniss::cpu::io_state();

            // Submit a oneshot poll for readability
            let op = Op::PollAdd {
                fd,
                events: libc::POLLIN as u32,
                multishot: false,
            };

            let token = state.io_backend.submit(op);

            // Poll for completion
            let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
            let completions = match state.io_backend.poll_complete(&mut cx) {
                std::task::Poll::Ready(comps) => comps,
                std::task::Poll::Pending => {
                    // Try again after a short delay
                    std::thread::sleep(Duration::from_millis(10));
                    match state.io_backend.poll_complete(&mut cx) {
                        std::task::Poll::Ready(comps) => comps,
                        std::task::Poll::Pending => panic!("PollAdd did not complete"),
                    }
                }
            };

            assert!(!completions.is_empty(), "Expected at least one completion");

            let (_, op_result, completion_result) = &completions[0];
            assert!(matches!(op_result, Op::PollAdd { .. }));
            assert!(
                completion_result.is_ok(),
                "PollAdd failed: {:?}",
                completion_result
            );
        });
    }

    #[test]
    fn test_poll_add_multishot() {
        let runtime = rust_miniss::Runtime::new();
        runtime.block_on(async {
            // Create a TCP listener
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
            listener
                .set_nonblocking(true)
                .expect("Failed to set nonblocking");

            let fd = listener.as_raw_fd();

            // Get the IO backend
            let state = rust_miniss::cpu::io_state();

            // Submit a multishot poll for readability
            let op = Op::PollAdd {
                fd,
                events: libc::POLLIN as u32,
                multishot: true,
            };

            let token = state.io_backend.submit(op);

            // Poll for completion - multishot should generate multiple completions
            let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
            let mut completion_count = 0;

            for _ in 0..3 {
                match state.io_backend.poll_complete(&mut cx) {
                    std::task::Poll::Ready(comps) => {
                        completion_count += comps.len();
                        if !comps.is_empty() {
                            let (_, op_result, completion_result) = &comps[0];
                            assert!(matches!(op_result, Op::PollAdd { .. }));
                            assert!(
                                completion_result.is_ok(),
                                "PollAdd failed: {:?}",
                                completion_result
                            );
                        }
                    }
                    std::task::Poll::Pending => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }

            // Multishot should generate at least one completion
            assert!(
                completion_count >= 1,
                "Expected at least one completion from multishot poll"
            );
        });
    }
}

#[cfg(any(not(target_os = "linux"), not(feature = "io_uring")))]
mod fast_poll_tests {
    #[test]
    fn test_poll_add_unsupported() {
        // Test is skipped on non-Linux or without io_uring feature
        println!("Fast Poll test skipped - requires Linux with io_uring feature");
    }
}
