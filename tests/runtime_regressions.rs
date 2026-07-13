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
        let runtime = Arc::try_unwrap(runtime).expect("test retained a runtime reference");
        let joined = runtime.join();
        shutdown.expect("runtime shutdown request");
        joined.expect("runtime workers joined");
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let _ = runtime.shutdown_with_timeout(self.timeout);
        if let Ok(runtime) = Arc::try_unwrap(runtime) {
            let _ = runtime.join();
        }
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
