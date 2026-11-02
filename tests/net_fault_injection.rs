use futures::io::{AsyncReadExt, AsyncWriteExt};
use rust_miniss::cpu::with_io_state;
use rust_miniss::net::fault::{
    fault_tcp_pair, fault_udp_pair, FaultInjector, FaultSchedule, SocketFault,
};
use rust_miniss::timer;
use rust_miniss::{Buffer, Runtime};
use std::time::Duration;

#[test]
fn test_delayed_packet_delivery() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let injector =
            FaultInjector::from_sequence(vec![SocketFault::Delay(Duration::from_millis(5))]);
        let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
        let (client, server) = fault_tcp_pair(no_fault, injector).expect("tcp pair");

        let sender = async move {
            let buf = Buffer::from_slice(b"delayed packet test");
            let (n, _) = client.write_rent(buf).await.expect("write_rent");
            assert_eq!(n, 19);
        };

        let receiver = async move {
            let buf = Buffer::with_capacity(64);
            let (n, received) = server.read_rent(buf).await.expect("read_rent");
            assert_eq!(n, 19);
            assert_eq!(&received[..n], b"delayed packet test");
        };

        futures::future::join(sender, receiver).await;

        with_io_state(|state| {
            assert_eq!(state.io_backend.pending_operation_count(), 0);
            assert_eq!(state.io_backend.retained_descriptor_count(), 0);
            assert!(state.io_backend.resource_balance().is_balanced_and_empty());
        });
    });
}

#[test]
fn test_truncated_packet() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let injector = FaultInjector::from_sequence(vec![SocketFault::Truncate(4)]);
        let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
        let (client, server) = fault_tcp_pair(no_fault, injector).expect("tcp pair");

        let sender = async move {
            let buf = Buffer::from_slice(b"0123456789");
            let (n, _) = client.write_rent(buf).await.expect("write_rent");
            assert_eq!(n, 10);
        };

        let receiver = async move {
            let buf = Buffer::with_capacity(64);
            let (n, received) = server.read_rent(buf).await.expect("read_rent");
            assert_eq!(n, 4, "packet should be truncated to exactly 4 bytes");
            assert_eq!(&received[..n], b"0123");
        };

        futures::future::join(sender, receiver).await;

        with_io_state(|state| {
            assert_eq!(state.io_backend.pending_operation_count(), 0);
            assert_eq!(state.io_backend.retained_descriptor_count(), 0);
            assert!(state.io_backend.resource_balance().is_balanced_and_empty());
        });
    });
}

#[test]
fn test_connection_reset_econnreset() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let injector = FaultInjector::from_sequence(vec![SocketFault::Reset]);
        let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
        let (_client, server) = fault_tcp_pair(no_fault, injector).expect("tcp pair");

        let buf = Buffer::with_capacity(64);
        let err = server
            .read_rent(buf)
            .await
            .expect_err("must fail with reset");
        assert_eq!(
            err.raw_os_error(),
            Some(libc::ECONNRESET),
            "expected ECONNRESET, got: {err:?}"
        );

        with_io_state(|state| {
            assert_eq!(state.io_backend.pending_operation_count(), 0);
            assert_eq!(state.io_backend.retained_descriptor_count(), 0);
            assert!(state.io_backend.resource_balance().is_balanced_and_empty());
        });
    });
}

#[test]
fn test_timeout_cancellation_race_read_rent() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // Test case 1: timeout fires before delivery -> cancelled
        {
            let injector = FaultInjector::from_sequence(vec![SocketFault::CancelRace {
                delay: Duration::from_millis(50),
                timeout: Duration::from_millis(1),
            }]);
            let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
            let (_client, server) = fault_tcp_pair(no_fault, injector).expect("tcp pair");

            let buf = Buffer::with_capacity(64);
            let res = server.read_rent(buf).await;
            assert!(
                res.is_err(),
                "should be cancelled by timeout before delivery"
            );
            assert_eq!(server.injector().stats().cancelled_ops, 1);

            with_io_state(|state| {
                assert_eq!(state.io_backend.pending_operation_count(), 0);
                assert_eq!(state.io_backend.retained_descriptor_count(), 0);
                assert!(state.io_backend.resource_balance().is_balanced_and_empty());
            });
        }

        // Test case 2: delivery finishes before timeout -> completed
        {
            let injector = FaultInjector::from_sequence(vec![SocketFault::CancelRace {
                delay: Duration::from_millis(0),
                timeout: Duration::from_millis(50),
            }]);
            let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
            let (client, server) = fault_tcp_pair(no_fault, injector).expect("tcp pair");

            let sender = async move {
                let buf = Buffer::from_slice(b"race data");
                client.write_rent(buf).await.expect("write_rent");
            };

            let receiver = async move {
                let buf = Buffer::with_capacity(64);
                let (n, received) = server.read_rent(buf).await.expect("read_rent completed");
                assert_eq!(&received[..n], b"race data");
                assert_eq!(server.injector().stats().completed_ops, 1);
            };

            futures::future::join(sender, receiver).await;

            with_io_state(|state| {
                assert_eq!(state.io_backend.pending_operation_count(), 0);
                assert_eq!(state.io_backend.retained_descriptor_count(), 0);
                assert!(state.io_backend.resource_balance().is_balanced_and_empty());
            });
        }
    });
}

#[test]
fn test_timeout_cancellation_race_borrowed_read() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
        let (mut client, mut server) =
            fault_tcp_pair(no_fault.clone(), no_fault).expect("tcp pair");

        // First read with timeout: peer hasn't sent data, timeout fires
        let mut buf = [0u8; 64];
        let read_future = server.read(&mut buf);
        let timeout_result = timer::timeout(Duration::from_millis(1), read_future).await;
        assert!(timeout_result.is_err(), "first borrowed read must time out");

        // Client now sends data
        client.write_all(b"resumed data").await.expect("write_all");

        // Second read on the same server stream: BorrowedRead should re-poll and receive the data!
        let n = server
            .read(&mut buf)
            .await
            .expect("read resumed data after cancellation");
        assert_eq!(&buf[..n], b"resumed data");

        // Clean drop of stream
        drop(client);
        drop(server);

        with_io_state(|state| {
            assert_eq!(state.io_backend.pending_operation_count(), 0);
            assert_eq!(state.io_backend.retained_descriptor_count(), 0);
            assert!(state.io_backend.resource_balance().is_balanced_and_empty());
        });
    });
}

#[test]
fn test_timeout_cancellation_race_udp() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let injector = FaultInjector::from_sequence(vec![SocketFault::CancelRace {
            delay: Duration::from_millis(50),
            timeout: Duration::from_millis(1),
        }]);
        let no_fault = FaultInjector::from_sequence(vec![SocketFault::None]);
        let (_client, server) = fault_udp_pair(no_fault, injector).expect("udp pair");

        let buf = Buffer::with_capacity(64);
        let res = server.recv_from_rent(buf).await;
        assert!(res.is_err(), "UDP recv should time out and cancel");
        assert_eq!(server.injector().stats().cancelled_ops, 1);

        with_io_state(|state| {
            assert_eq!(state.io_backend.pending_operation_count(), 0);
            assert_eq!(state.io_backend.retained_descriptor_count(), 0);
            assert!(state.io_backend.resource_balance().is_balanced_and_empty());
        });
    });
}

#[test]
fn test_500_randomized_cancellation_races() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        const SEED: u64 = 0x5EED_C0DE_1234_5678;
        const ITERATIONS: usize = 500;

        let schedule = FaultSchedule::random_cancellation_races(SEED, ITERATIONS);
        let injector = FaultInjector::new(schedule);

        // Pre-create connected pairs
        let mut client_opt = None;
        let mut server_opt = None;

        for i in 0..ITERATIONS {
            // Reconnect pair if previous was reset or not yet initialized
            if client_opt.is_none() {
                let no_fault = FaultInjector::pass_through();
                let (c, s) = fault_tcp_pair(no_fault, injector.clone()).expect("create pair");
                client_opt = Some(c);
                server_opt = Some(s);
            }

            let client = client_opt.as_mut().unwrap();
            let server = server_opt.as_mut().unwrap();

            // Interleave between read_rent and borrowed read
            if i % 2 == 0 {
                // Rented read
                let send_task = async {
                    let buf = Buffer::from_slice(b"race packet payload 12345678");
                    let _ = client.write_rent(buf).await;
                };

                let recv_task = async {
                    let buf = Buffer::with_capacity(64);
                    let res = server.read_rent(buf).await;
                    if let Err(err) = &res {
                        if err.raw_os_error() == Some(libc::ECONNRESET) {
                            // Connection reset observed: drop pair to recreate
                            return Err(());
                        }
                    }
                    Ok(())
                };

                let (_, recv_res) = futures::future::join(send_task, recv_task).await;
                if recv_res.is_err() {
                    client_opt = None;
                    server_opt = None;
                }
            } else {
                // Borrowed read
                let send_task = async {
                    let buf = Buffer::from_slice(b"borrowed race payload 987654");
                    let _ = client.write_rent(buf).await;
                };

                let recv_task = async {
                    let mut slice = [0u8; 64];
                    let res = server.read(&mut slice).await;
                    if let Err(err) = &res {
                        if err.raw_os_error() == Some(libc::ECONNRESET) {
                            return Err(());
                        }
                    }
                    Ok(())
                };

                let (_, recv_res) = futures::future::join(send_task, recv_task).await;
                if recv_res.is_err() {
                    client_opt = None;
                    server_opt = None;
                }
            }
        }

        // Cleanly drop any remaining streams
        drop(client_opt);
        drop(server_opt);

        // Verification of stats
        let stats = injector.stats();
        println!("=== 500 Randomized Cancellation Races Report ===");
        println!("Total faults injected: {}", stats.total_faults);
        println!("  - Cancellation races: {}", stats.cancel_races);
        println!("  - Delays:             {}", stats.delays);
        println!("  - Truncations:        {}", stats.truncations);
        println!("  - Resets (ECONNRESET):{}", stats.resets);
        println!("Operation outcomes:");
        println!("  - Completed ops:      {}", stats.completed_ops);
        println!("  - Cancelled ops:      {}", stats.cancelled_ops);

        assert_eq!(stats.total_faults, ITERATIONS);
        assert!(stats.cancel_races > 0, "must exercise cancellation races");
        assert!(stats.delays > 0, "must exercise delays");
        assert!(stats.truncations > 0, "must exercise truncations");
        assert!(stats.resets > 0, "must exercise resets");
        assert!(
            stats.cancelled_ops > 0,
            "must have cancelled operations due to timeout races"
        );
        assert!(
            stats.completed_ops > 0,
            "must have completed operations before timeout"
        );

        // Verify resource hygiene: NO memory leaks, NO dangling buffer leases, balanced ledger!
        with_io_state(|state| {
            assert_eq!(
                state.io_backend.pending_operation_count(),
                0,
                "dangling pending operations detected"
            );
            assert_eq!(
                state.io_backend.retained_descriptor_count(),
                0,
                "dangling descriptor leases detected"
            );
            let balance = state.io_backend.resource_balance();
            assert!(
                balance.is_balanced_and_empty(),
                "resource ledger must be balanced and empty: {balance:?}"
            );
        });
    });
}
