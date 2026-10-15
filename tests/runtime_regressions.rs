use rust_miniss::multicore::RuntimeTerminationSnapshot;
use rust_miniss::{AsyncFile, Executor, MultiCoreRuntime, Runtime, TaskError};
use std::sync::Arc;
use std::time::Duration;

/// Keep worker cleanup ahead of assertions.  Dropping a live runtime joins
/// workers, so an assertion panic must first send the bounded force-shutdown
/// signal instead of turning the failure into an unbounded hang.
struct RuntimeGuard {
    runtime: Option<Arc<MultiCoreRuntime>>,
    timeout: Duration,
}

impl RuntimeGuard {
    fn new(runtime: Arc<MultiCoreRuntime>) -> Self {
        Self {
            runtime: Some(runtime),
            timeout: Duration::from_secs(1),
        }
    }

    fn runtime(&self) -> &Arc<MultiCoreRuntime> {
        self.runtime.as_ref().expect("runtime guard is active")
    }

    fn finish(mut self) {
        let runtime = self.runtime.take().expect("runtime guard is active");
        let shutdown = runtime.shutdown_with_timeout(self.timeout);
        let joined = runtime.join();
        shutdown.expect("runtime shutdown request");
        joined.expect("runtime workers joined");
    }

    fn finish_and_snapshot(mut self) -> RuntimeTerminationSnapshot {
        let runtime = self.runtime.take().expect("runtime guard is active");
        let shutdown = runtime.shutdown_with_timeout(self.timeout);
        let joined = runtime.join();
        shutdown.expect("runtime shutdown request");
        joined.expect("runtime workers joined");
        runtime
            .termination_snapshot()
            .expect("termination snapshot after join")
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let _ = runtime.shutdown_with_timeout(self.timeout);
        let _ = runtime.join();
    }
}

#[test]
fn join_handle_is_finished_does_not_consume_result() {
    let mut executor = Executor::new();
    let handle = executor.spawn(async { 42 });

    executor.run();

    assert!(handle.is_finished());
    let result = futures::executor::block_on(handle);
    assert_eq!(result.unwrap(), 42);
}

#[test]
fn join_handle_reports_task_panic() {
    let mut executor = Executor::new();
    let handle = executor.spawn(async {
        panic!("boom");
    });

    executor.run();

    match futures::executor::block_on(handle) {
        Err(TaskError::Panic(payload)) => {
            assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom"));
        }
        other => panic!("expected TaskError::Panic, got {other:?}"),
    }
}

#[test]
fn async_file_sync_all_completes_after_write() {
    let runtime = Runtime::new();

    runtime.block_on(async {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let temp_path = temp_dir.path().join("sync_all.txt");
        let file = AsyncFile::create(&temp_path).expect("create async file");
        let data = b"sync me";

        assert_eq!(file.write_at(0, data).await.expect("write"), data.len());
        file.sync_all().await.expect("sync_all");

        assert_eq!(std::fs::read(&temp_path).expect("read back"), data);
    });
}

// Token-generation slot aliasing is covered by the crate-private unit test
// `cpu::tests::cpu_io_slots_do_not_alias_different_token_generations`:
// integration tests cannot call `IoToken::from_id` without exposing a public footgun.

#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn tcp_read_rent_reads_into_empty_capacity_buffer() {
    use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
    use std::net::SocketAddr;

    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (stream, _) = listener.accept().await.unwrap();
            let buf = Vec::with_capacity(64);
            assert_eq!(buf.len(), 0);
            assert!(buf.capacity() >= 64);

            let (n, buf) = stream.read_rent(buf).await.unwrap();
            assert_eq!(n, 5);
            assert_eq!(buf.len(), 5);
            assert_eq!(&buf[..], b"hello");
        };

        let client = async move {
            let stream = AsyncTcpStream::connect(addr).await.unwrap();
            stream
                .write_rent(Vec::from(b"hello".as_slice()))
                .await
                .unwrap();
        };

        futures::future::join(server, client).await;
    });
}

#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn tcp_accept_multishot_completes_after_delayed_connect() {
    use futures::future::{select, Either};
    use futures::pin_mut;
    use rust_miniss::net::AsyncTcpListener;
    use std::net::{SocketAddr, TcpStream};
    use std::thread;

    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let _client = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            TcpStream::connect(addr).expect("connect");
        });

        let accept = listener.accept();
        let timeout = rust_miniss::sleep(Duration::from_secs(1));
        pin_mut!(accept);
        pin_mut!(timeout);

        match select(accept, timeout).await {
            Either::Left((Ok((stream, _addr)), _timeout)) => {
                drop(stream);
            }
            Either::Left((Err(err), _timeout)) => {
                panic!("accept failed unexpectedly: {err}");
            }
            Either::Right((_timeout, _accept)) => {
                panic!("timed out waiting for async accept completion");
            }
        }
    });
}

#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn tcp_accept_multishot_rearms_for_a_bounded_burst() {
    use rust_miniss::net::AsyncTcpListener;
    use std::net::{SocketAddr, TcpStream};
    use std::thread;

    const CLIENTS: usize = 64;
    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let clients = (0..CLIENTS)
            .map(|_| {
                thread::spawn(move || {
                    TcpStream::connect(addr).expect("connect burst client");
                })
            })
            .collect::<Vec<_>>();

        for _ in 0..CLIENTS {
            let accepted = rust_miniss::timer::timeout(Duration::from_secs(1), listener.accept())
                .await
                .expect("accept burst timed out")
                .expect("accept burst failed");
            drop(accepted.0);
        }
        for client in clients {
            client.join().expect("burst client panicked");
        }
    });
}

#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn tcp_multishot_accept_shutdown_drains_pending_listener() {
    use rust_miniss::net::AsyncTcpListener;
    use std::net::{SocketAddr, TcpStream};
    use std::thread;

    const CLIENTS: usize = 64;
    let runtime = MultiCoreRuntime::new(Some(1)).expect("runtime");
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let task_runtime = runtime.clone();
    task_runtime
        .spawn_on(0, async move {
            let mut listener = AsyncTcpListener::bind(
                "127.0.0.1:0"
                    .parse::<SocketAddr>()
                    .expect("listener address"),
            )
            .expect("bind listener");
            addr_tx
                .send(listener.local_addr().expect("listener local address"))
                .expect("send listener address");
            let _ = listener.accept().await;
        })
        .expect("admit pending accept task");

    let addr = addr_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("listener did not start");
    thread::sleep(Duration::from_millis(20));
    let clients = (0..CLIENTS)
        .map(|_| {
            thread::spawn(move || {
                let _ = TcpStream::connect(addr);
            })
        })
        .collect::<Vec<_>>();

    runtime
        .shutdown_with_timeout(Duration::from_secs(2))
        .expect("shutdown pending accept listener");
    runtime.join().expect("join pending accept worker");
    for client in clients {
        client.join().expect("client thread");
    }
    assert!(runtime
        .termination_snapshot()
        .expect("termination snapshot")
        .all_zero());
}

#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn tcp_multishot_accept_reclaims_multiple_overflow_bursts_on_listener_drop() {
    use rust_miniss::net::AsyncTcpListener;
    use std::net::{SocketAddr, TcpStream};
    use std::thread;

    const BURSTS: usize = 3;
    const CLIENTS_PER_BURST: usize = 128;
    let runtime = MultiCoreRuntime::new(Some(1)).expect("runtime");
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let (armed_tx, armed_rx) = std::sync::mpsc::channel();
    runtime
        .spawn_on(0, async move {
            let mut listener = AsyncTcpListener::bind(
                "127.0.0.1:0"
                    .parse::<SocketAddr>()
                    .expect("listener address"),
            )
            .expect("bind listener");
            addr_tx
                .send(listener.local_addr().expect("listener local address"))
                .expect("send listener address");
            let (stream, _) = listener.accept().await.expect("arm multishot accept");
            drop(stream);
            armed_tx.send(()).expect("accept arm signal");
            rust_miniss::timer::sleep(Duration::from_millis(500)).await;
            drop(listener);
        })
        .expect("admit overflow accept task");

    let addr = addr_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("listener did not start");
    let _ = TcpStream::connect(addr).expect("arm client connection");
    armed_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("multishot accept did not arm");

    for _ in 0..BURSTS {
        let clients = (0..CLIENTS_PER_BURST)
            .map(|_| {
                thread::spawn(move || {
                    let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(1));
                })
            })
            .collect::<Vec<_>>();
        for client in clients {
            client.join().expect("overflow client thread");
        }
        thread::sleep(Duration::from_millis(20));
    }

    runtime
        .shutdown_with_timeout(Duration::from_secs(2))
        .expect("shutdown overflow listener");
    runtime.join().expect("join overflow accept worker");
    assert!(runtime
        .termination_snapshot()
        .expect("termination snapshot")
        .all_zero());
}

#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn tcp_pending_read_drop_is_drained_before_late_data_arrives() {
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
    use std::net::SocketAddr;

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 4];
            let result =
                rust_miniss::timer::timeout(Duration::from_millis(20), stream.read(&mut buffer))
                    .await;
            assert!(result.is_err(), "read completed before the late write");
        };

        let client = async move {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            rust_miniss::sleep(Duration::from_millis(50)).await;
            stream.write_all(b"late").await.unwrap();
        };

        futures::future::join(server, client).await;
    });
}

#[test]
fn multicore_spawned_timer_future_completes() {
    let guard = RuntimeGuard::new(MultiCoreRuntime::new(Some(1)).expect("runtime"));
    let (tx, rx) = std::sync::mpsc::channel();

    guard
        .runtime()
        .spawn(async move {
            rust_miniss::sleep(Duration::from_millis(5)).await;
            tx.send(()).expect("send completion");
        })
        .expect("spawn");

    rx.recv_timeout(Duration::from_secs(1))
        .expect("timer task completed");
    guard.finish();
}

#[test]
fn multicore_block_on_drives_timer_future() {
    let guard = RuntimeGuard::new(MultiCoreRuntime::new(Some(1)).expect("runtime"));

    let result = guard.runtime().block_on(async {
        rust_miniss::sleep(Duration::from_millis(5)).await;
        7
    });

    assert_eq!(result, 7);
    guard.finish();
}

#[test]
fn multicore_spawned_async_file_future_completes() {
    let guard = RuntimeGuard::new(MultiCoreRuntime::new(Some(1)).expect("runtime"));
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let temp_path = temp_dir.path().join("multicore-io.txt");
    let task_path = temp_path.clone();
    let (tx, rx) = std::sync::mpsc::channel();

    guard
        .runtime()
        .spawn(async move {
            let file = AsyncFile::create(&task_path).expect("create");
            file.write_at(0, b"multicore io").await.expect("write");
            file.sync_all().await.expect("sync");
            tx.send(()).expect("send completion");
        })
        .expect("spawn");

    // Always shut the runtime down before asserting.  A failed receive used
    // to panic while the worker was still alive; dropping the runtime then
    // waited indefinitely on a stuck I/O task and made this regression test
    // look like an unbounded hang.
    let completion = rx.recv_timeout(Duration::from_secs(1));
    guard.finish();
    completion.expect("async file task completed");
    assert_eq!(
        std::fs::read(&temp_path).expect("read back"),
        b"multicore io"
    );
}

#[test]
fn multicore_loaded_timer_shutdown_publishes_zero_termination_snapshot() {
    const TASKS: usize = 256;
    let guard = RuntimeGuard::new(MultiCoreRuntime::new(Some(4)).expect("runtime"));
    let (tx, rx) = std::sync::mpsc::channel();

    for task_number in 0..TASKS {
        let tx = tx.clone();
        guard
            .runtime()
            .spawn(async move {
                rust_miniss::sleep(Duration::from_millis((task_number % 4 + 1) as u64)).await;
                tx.send(task_number)
                    .expect("completion receiver remains live");
            })
            .expect("timer task admitted");
    }
    drop(tx);

    let mut completed = 0;
    while rx.recv_timeout(Duration::from_secs(2)).is_ok() {
        completed += 1;
    }

    let snapshot = guard.finish_and_snapshot();
    assert_eq!(completed, TASKS, "not all loaded timer tasks completed");
    assert!(
        snapshot.all_zero(),
        "loaded shutdown left non-zero resources: {snapshot:?}"
    );
}

/// The `AsyncRead` async path used to submit `Op::Read`, which made the backend
/// own a `Buffer`, read a copy into it, and then copy a second time into the
/// caller's buffer on completion. It now arms readiness and re-enters
/// `poll_read`, so a read that had to wait costs the same one copy as one that
/// found data immediately.
///
/// This pins the contract that change depends on: a read that arrives *after*
/// the poll went `Pending` must come back complete, and readiness must re-arm
/// correctly across several such waits. The client deliberately sleeps so the
/// server's read cannot take the `WouldBlock` fast path — without the delay the
/// test would pass against the old code too.
#[test]
fn async_read_that_waits_returns_the_full_payload_over_several_waits() {
    use futures::future::join;
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
    use std::net::SocketAddr;

    let runtime = Runtime::new();

    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let server = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];

            // Three reads, each of which must go Pending first: the client waits
            // 50ms before every write, long enough that the server's read has
            // already armed POLLIN when the bytes arrive.
            for expected in ["first".as_bytes(), "second".as_bytes(), "third".as_bytes()] {
                let n = stream.read(&mut buf).await.unwrap();
                assert_eq!(n, expected.len());
                assert_eq!(&buf[..n], expected);
            }
        };

        let client = async {
            let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
            for chunk in ["first", "second", "third"] {
                rust_miniss::sleep(Duration::from_millis(50)).await;
                stream.write_all(chunk.as_bytes()).await.unwrap();
            }
        };

        join(server, client).await;
    });
}
