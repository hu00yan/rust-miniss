use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn multicore_task_distribution() {
    let runtime = rust_miniss::MultiCoreRuntime::new(None).expect("create multicore runtime");

    let counters = Arc::new([
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ]);

    runtime.block_on(async {
        for i in 0..4 {
            let counters = counters.clone();
            runtime
                .spawn(async move {
                    for _j in 0..250 {
                        counters[i].fetch_add(1, Ordering::Relaxed);
                    }
                })
                .expect("spawn task");
        }

        rust_miniss::timer::sleep(std::time::Duration::from_millis(200)).await;
    });

    runtime
        .shutdown_with_timeout(std::time::Duration::from_secs(1))
        .expect("multicore shutdown request");
    runtime.join().expect("multicore workers joined");

    let counts: Vec<_> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let total: usize = counts.iter().sum();
    assert_eq!(
        total, 1000,
        "expected 1000 total tasks, got {total} with counts {counts:?}"
    );
}

#[test]
fn multicore_sustained_cross_core_wake_pressure_drains_cleanly() {
    let runtime = rust_miniss::MultiCoreRuntime::new(None).expect("create multicore runtime");
    let shard_count = runtime
        .metrics_snapshot(Duration::from_secs(1))
        .expect("read available shard count")
        .cores
        .len();
    assert!(
        shard_count > 1,
        "cross-core pressure requires at least two shards"
    );
    let completed = Arc::new(AtomicUsize::new(0));
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    const TASKS: usize = 1024;

    for index in 0..TASKS {
        let completed = completed.clone();
        let completed_tx = completed_tx.clone();
        runtime
            .spawn_on((index % (shard_count - 1)) + 1, async move {
                completed.fetch_add(1, Ordering::Relaxed);
                completed_tx
                    .send(())
                    .expect("completion receiver remains live");
            })
            .expect("cross-core task admission");
    }
    drop(completed_tx);
    for _ in 0..TASKS {
        completed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cross-core task did not complete");
    }
    assert_eq!(completed.load(Ordering::Acquire), TASKS);

    runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .expect("cross-core pressure shutdown request");
    runtime.join().expect("cross-core pressure workers joined");
    assert!(runtime
        .termination_snapshot()
        .expect("cross-core pressure termination snapshot")
        .all_zero());
}

#[test]
fn multicore_udp_owned_shards_exchange_and_close_cleanly() {
    let runtime = rust_miniss::MultiCoreRuntime::new(Some(2)).expect("create multicore runtime");
    let server_runtime = runtime.clone();
    let client_runtime = runtime.clone();
    let (addr_tx, addr_rx) = futures::channel::oneshot::channel();
    let (server_done_tx, server_done_rx) = std::sync::mpsc::channel();
    let (client_done_tx, client_done_rx) = std::sync::mpsc::channel();
    const MESSAGES: usize = 128;

    runtime
        .spawn_on(0, async move {
            let socket = rust_miniss::net::AsyncUdpSocket::bind(
                "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
            )
            .expect("bind server UDP socket");
            let socket = Arc::new(socket);
            let addr = socket.local_addr().expect("server UDP address");
            addr_tx.send(addr).expect("client shard remains live");

            let mut completions = Vec::with_capacity(MESSAGES);
            for _ in 0..MESSAGES {
                let socket = socket.clone();
                let (done_tx, done_rx) = futures::channel::oneshot::channel();
                server_runtime
                    .spawn_local(async move {
                        let result = async {
                            let mut buffer = [0_u8; 64];
                            let (len, peer) = socket.recv_from(&mut buffer).await?;
                            if !buffer[..len].starts_with(b"udp-") {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "invalid UDP payload",
                                ));
                            }
                            socket.send_to(&buffer[..len], peer).await?;
                            Ok(String::from_utf8_lossy(&buffer[..len]).into_owned())
                        }
                        .await
                        .map_err(|error: std::io::Error| error.to_string());
                        let _ = done_tx.send(result);
                    })
                    .expect("admit concurrent server UDP task");
                completions.push(done_rx);
            }
            let mut payloads = Vec::with_capacity(MESSAGES);
            for completion in completions {
                payloads.push(
                    completion
                        .await
                        .expect("server UDP task result")
                        .expect("server UDP task succeeded"),
                );
            }
            payloads.sort();
            let mut expected = (0..MESSAGES)
                .map(|index| format!("udp-{index}"))
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(payloads, expected, "server received an incomplete UDP set");
            server_done_tx.send(()).expect("server result receiver");
        })
        .expect("admit server shard task");

    runtime
        .spawn_on(1, async move {
            let server_addr = addr_rx.await.expect("server shard remains live");
            let mut completions = Vec::with_capacity(MESSAGES);
            for index in 0..MESSAGES {
                let (done_tx, done_rx) = futures::channel::oneshot::channel();
                client_runtime
                    .spawn_local(async move {
                        let result = async {
                            let socket = rust_miniss::net::AsyncUdpSocket::bind(
                                "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap(),
                            )
                            .expect("bind client UDP socket");
                            let mut buffer = [0_u8; 64];
                            let payload = format!("udp-{index}");
                            socket.send_to(payload.as_bytes(), server_addr).await?;
                            let (len, peer) = socket.recv_from(&mut buffer).await?;
                            assert_eq!(peer, server_addr);
                            assert_eq!(&buffer[..len], payload.as_bytes());
                            Ok::<_, std::io::Error>(())
                        }
                        .await
                        .map_err(|error: std::io::Error| error.to_string());
                        let _ = done_tx.send(result);
                    })
                    .expect("admit concurrent client UDP task");
                completions.push(done_rx);
            }
            for completion in completions {
                completion
                    .await
                    .expect("client UDP task result")
                    .expect("client UDP task succeeded");
            }
            client_done_tx.send(()).expect("client result receiver");
        })
        .expect("admit client shard task");

    server_done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("server shard did not finish UDP exchange");
    client_done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("client shard did not finish UDP exchange");

    runtime
        .shutdown_with_timeout(Duration::from_secs(1))
        .expect("multicore UDP shutdown request");
    runtime.join().expect("multicore UDP workers joined");
    assert!(runtime
        .termination_snapshot()
        .expect("UDP termination snapshot")
        .all_zero());
}
