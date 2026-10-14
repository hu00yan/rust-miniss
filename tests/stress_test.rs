use rust_miniss::net::{AsyncTcpListener, AsyncUdpSocket};
use rust_miniss::timer;
use rust_miniss::MultiCoreRuntime;
use rust_miniss::Runtime;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn test_udp_stress() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let recv_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        let send_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();

        let count = 1000;
        for i in 0..count {
            let data = format!("packet {i}").into_bytes();
            send_socket.send_to(&data, recv_addr).await.unwrap();
        }

        for i in 0..count {
            let mut buf = [0u8; 64];
            let (n, _from) = recv_socket.recv_from(&mut buf).await.unwrap();
            let expected = format!("packet {i}");
            assert_eq!(&buf[..n], expected.as_bytes());
        }
    });
}

#[test]
fn test_long_running() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        for _ in 0..10 {
            timer::sleep(Duration::from_millis(10)).await;
        }
    });
}

#[test]
fn test_udp_stress_multiple_senders() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let recv_socket =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        let sender_a =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let sender_b =
            AsyncUdpSocket::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();

        let n = 500;
        for i in 0..n {
            let data = format!("A{i}").into_bytes();
            sender_a.send_to(&data, recv_addr).await.unwrap();
            let data = format!("B{i}").into_bytes();
            sender_b.send_to(&data, recv_addr).await.unwrap();
        }

        let mut count_a = 0;
        let mut count_b = 0;
        for _ in 0..n * 2 {
            let mut buf = [0u8; 64];
            let (_len, _) = recv_socket.recv_from(&mut buf).await.unwrap();
            if buf[0] == b'A' {
                count_a += 1;
            } else if buf[0] == b'B' {
                count_b += 1;
            }
        }
        assert_eq!(count_a, n);
        assert_eq!(count_b, n);
    });
}

#[test]
fn test_multicore_sustained_admission_pressure() {
    let duration_ms = std::env::var("MINISS_STRESS_DURATION_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(100);
    let runtime = MultiCoreRuntime::new(Some(4)).expect("create multicore runtime");
    let completed = Arc::new(AtomicUsize::new(0));
    let deadline = std::time::Instant::now() + Duration::from_millis(duration_ms);

    runtime.block_on(async {
        while std::time::Instant::now() < deadline {
            for _ in 0..64 {
                let completed = completed.clone();
                runtime
                    .spawn(async move {
                        completed.fetch_add(1, Ordering::Relaxed);
                    })
                    .expect("sustained admission pressure task");
            }
            rust_miniss::timer::sleep(Duration::from_millis(1)).await;
        }
    });

    runtime
        .shutdown_with_timeout(Duration::from_secs(2))
        .expect("sustained pressure shutdown request");
    runtime.join().expect("sustained pressure workers joined");
    assert!(completed.load(Ordering::Acquire) > 0);
    assert!(runtime
        .termination_snapshot()
        .expect("sustained pressure termination snapshot")
        .all_zero());
}

#[test]
fn test_multicore_loaded_shutdown_reclaims_all_state() {
    let duration_ms = std::env::var("MINISS_SHUTDOWN_STRESS_DURATION_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(100);
    let rounds = std::env::var("MINISS_SHUTDOWN_STRESS_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3);
    assert!(rounds > 0, "shutdown stress requires at least one round");

    for round in 0..rounds {
        let runtime = MultiCoreRuntime::new(Some(4)).expect("create multicore runtime");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        for _ in 0..64 {
            let stop = stop.clone();
            runtime
                .spawn(async move {
                    while !stop.load(Ordering::Acquire) {
                        rust_miniss::timer::sleep(Duration::from_millis(1)).await;
                    }
                })
                .expect("loaded shutdown task admission");
        }

        runtime.block_on(async {
            rust_miniss::timer::sleep(Duration::from_millis(duration_ms)).await;
        });
        stop.store(true, Ordering::Release);
        runtime
            .shutdown_with_timeout(Duration::from_secs(2))
            .expect("loaded shutdown request");
        runtime.join().expect("loaded shutdown workers joined");
        assert!(
            runtime
                .termination_snapshot()
                .expect("loaded shutdown termination snapshot")
                .all_zero(),
            "loaded shutdown left state after round {round}"
        );
    }
}

#[test]
fn test_multicore_loaded_shutdown_worker_and_task_matrix() {
    let duration_ms = std::env::var("MINISS_MATRIX_STRESS_DURATION_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(100);

    for workers in [1, 2, 4] {
        for tasks in [16, 64] {
            for timer_tasks in [false, true] {
                let runtime = MultiCoreRuntime::new(Some(workers)).expect("create matrix runtime");
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

                for _ in 0..tasks {
                    let stop = stop.clone();
                    runtime
                        .spawn(async move {
                            if timer_tasks {
                                while !stop.load(Ordering::Acquire) {
                                    rust_miniss::timer::sleep(Duration::from_millis(1)).await;
                                }
                            }
                        })
                        .expect("matrix shutdown task admission");
                }

                runtime.block_on(async {
                    rust_miniss::timer::sleep(Duration::from_millis(duration_ms)).await;
                });
                stop.store(true, Ordering::Release);
                runtime
                    .shutdown_with_timeout(Duration::from_secs(2))
                    .expect("matrix shutdown request");
                runtime.join().expect("matrix workers joined");
                let snapshot = runtime
                    .termination_snapshot()
                    .expect("matrix termination snapshot");
                assert!(
                    snapshot.all_zero(),
                    "matrix left state for workers={workers}, tasks={tasks}, timer_tasks={timer_tasks}: {snapshot:?}"
                );
            }
        }
    }
}

#[test]
fn test_multicore_pending_udp_shutdown_reclaims_socket_leases() {
    let rounds = std::env::var("MINISS_RETAINED_FD_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2);
    assert!(rounds > 0, "retained-fd stress requires at least one round");

    for round in 0..rounds {
        for workers in [1, 2, 4] {
            for sockets in [8, 32] {
                let runtime =
                    MultiCoreRuntime::new(Some(workers)).expect("create UDP matrix runtime");
                let ready = Arc::new(AtomicUsize::new(0));

                for _ in 0..sockets {
                    let ready = ready.clone();
                    runtime
                        .spawn(async move {
                            let socket = AsyncUdpSocket::bind(
                                "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
                            )
                            .expect("bind matrix UDP socket");
                            ready.fetch_add(1, Ordering::Release);
                            let mut buffer = [0u8; 64];
                            let _ = socket.recv_from(&mut buffer).await;
                        })
                        .expect("admit matrix UDP task");
                }

                runtime.block_on(async {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while ready.load(Ordering::Acquire) != sockets {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "UDP matrix tasks did not become ready for workers={workers}, sockets={sockets}"
                    );
                    rust_miniss::timer::sleep(Duration::from_millis(1)).await;
                }
            });

                runtime
                    .shutdown_with_timeout(Duration::from_secs(2))
                    .expect("UDP matrix shutdown request");
                runtime.join().expect("UDP matrix workers joined");
                assert!(
                    runtime
                        .termination_snapshot()
                        .expect("UDP matrix termination snapshot")
                        .all_zero(),
                    "UDP matrix retained state for round={round}, workers={workers}, sockets={sockets}"
                );
            }
        }
    }
}

#[test]
fn test_multicore_udp_receive_close_race_reclaims_state() {
    let sockets = std::env::var("MINISS_UDP_CLOSE_RACE_SOCKETS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64);
    assert!(sockets > 0, "UDP close-race test requires sockets");

    let runtime = MultiCoreRuntime::new(Some(4)).expect("create UDP close-race runtime");
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    for _ in 0..sockets {
        let addr_tx = addr_tx.clone();
        runtime
            .spawn(async move {
                let socket = AsyncUdpSocket::bind(
                    "127.0.0.1:0"
                        .parse::<std::net::SocketAddr>()
                        .expect("UDP bind address"),
                )
                .expect("bind UDP close-race socket");
                addr_tx
                    .send(socket.local_addr().expect("UDP close-race address"))
                    .expect("close-race address receiver");
                let mut buffer = [0u8; 64];
                let _ = socket.recv_from(&mut buffer).await;
            })
            .expect("admit UDP close-race task");
    }
    drop(addr_tx);

    let addresses = (0..sockets)
        .map(|_| {
            addr_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("UDP close-race socket did not become ready")
        })
        .collect::<Vec<_>>();
    let senders = addresses
        .into_iter()
        .map(|address| {
            std::thread::spawn(move || {
                let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind sender");
                for _ in 0..4 {
                    let _ = sender.send_to(b"close-race", address);
                }
            })
        })
        .collect::<Vec<_>>();

    runtime
        .shutdown_with_timeout(Duration::from_secs(2))
        .expect("UDP close-race shutdown request");
    runtime.join().expect("UDP close-race workers joined");
    for sender in senders {
        sender.join().expect("UDP close-race sender panicked");
    }
    assert!(runtime
        .termination_snapshot()
        .expect("UDP close-race termination snapshot")
        .all_zero());
}

#[test]
fn test_multicore_accepted_tcp_shutdown_reclaims_descriptor_leases() {
    let rounds = std::env::var("MINISS_TCP_RETAINED_FD_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    assert!(
        rounds > 0,
        "TCP retained-fd stress requires at least one round"
    );

    for round in 0..rounds {
        for workers in [1, 2, 4] {
            for connections in [8, 32] {
                let runtime =
                    MultiCoreRuntime::new(Some(workers)).expect("create TCP matrix runtime");
                let (addr_tx, addr_rx) = std::sync::mpsc::channel();
                let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
                let local_runtime = runtime.clone();
                runtime
                    .spawn_on(0, async move {
                        local_runtime
                            .spawn_local(async move {
                                let mut listener = AsyncTcpListener::bind(
                                    "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
                                )
                                .expect("bind matrix TCP listener");
                                addr_tx
                                    .send(listener.local_addr().expect("matrix listener address"))
                                    .expect("matrix listener address receiver");
                                let mut accepted = Vec::with_capacity(connections);
                                for _ in 0..connections {
                                    let (stream, _) = rust_miniss::timer::timeout(
                                        Duration::from_secs(2),
                                        listener.accept(),
                                    )
                                    .await
                                    .expect("matrix accept timed out")
                                    .expect("matrix accept failed");
                                    accepted.push(stream);
                                }
                                accepted_tx.send(()).expect("matrix accepted receiver");
                                std::future::pending::<()>().await;
                            })
                            .expect("admit local matrix TCP listener task");
                    })
                    .expect("admit matrix TCP listener task");

                let addr = addr_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("matrix listener did not start");
                let clients = (0..connections)
                    .map(|_| {
                        std::thread::spawn(move || {
                            std::net::TcpStream::connect(addr).expect("matrix TCP client connect")
                        })
                    })
                    .collect::<Vec<_>>();
                accepted_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("matrix listener did not retain all connections");

                runtime
                    .shutdown_with_timeout(Duration::from_secs(2))
                    .expect("TCP matrix shutdown request");
                runtime.join().expect("TCP matrix workers joined");
                for client in clients {
                    client.join().expect("matrix TCP client thread");
                }
                assert!(
                    runtime
                        .termination_snapshot()
                        .expect("TCP matrix termination snapshot")
                        .all_zero(),
                    "TCP matrix retained state for round={round}, workers={workers}, connections={connections}"
                );
            }
        }
    }
}
