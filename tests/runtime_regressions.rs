use rust_miniss::{AsyncFile, Executor, MultiCoreRuntime, Runtime, TaskError};
use std::sync::Arc;
use std::time::Duration;

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
// `cpu::tests::cpu_io_slots_do_not_alias_different_token_generations` (rm-00):
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
    let runtime = MultiCoreRuntime::new(Some(1)).expect("runtime");
    let (tx, rx) = std::sync::mpsc::channel();

    runtime
        .spawn(async move {
            rust_miniss::sleep(Duration::from_millis(5)).await;
            tx.send(()).expect("send completion");
        })
        .expect("spawn");

    rx.recv_timeout(Duration::from_secs(1))
        .expect("timer task completed");
    runtime.shutdown().expect("shutdown");
    Arc::try_unwrap(runtime)
        .expect("runtime still referenced")
        .join()
        .expect("join");
}

#[test]
fn multicore_block_on_drives_timer_future() {
    let runtime = MultiCoreRuntime::new(Some(1)).expect("runtime");

    let result = runtime.block_on(async {
        rust_miniss::sleep(Duration::from_millis(5)).await;
        7
    });

    assert_eq!(result, 7);
    runtime.shutdown().expect("shutdown");
    Arc::try_unwrap(runtime)
        .expect("runtime still referenced")
        .join()
        .expect("join");
}

#[test]
fn multicore_spawned_async_file_future_completes() {
    let runtime = MultiCoreRuntime::new(Some(1)).expect("runtime");
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let temp_path = temp_dir.path().join("multicore-io.txt");
    let task_path = temp_path.clone();
    let (tx, rx) = std::sync::mpsc::channel();

    runtime
        .spawn(async move {
            let file = AsyncFile::create(&task_path).expect("create");
            file.write_at(0, b"multicore io").await.expect("write");
            file.sync_all().await.expect("sync");
            tx.send(()).expect("send completion");
        })
        .expect("spawn");

    rx.recv_timeout(Duration::from_secs(1))
        .expect("async file task completed");
    assert_eq!(
        std::fs::read(&temp_path).expect("read back"),
        b"multicore io"
    );

    runtime.shutdown().expect("shutdown");
    Arc::try_unwrap(runtime)
        .expect("runtime still referenced")
        .join()
        .expect("join");
}

#[cfg(any(
    all(unix, not(target_os = "linux")),
    all(target_os = "linux", io_backend = "epoll")
))]
#[test]
fn mio_backend_close_completes_successfully_once() {
    use rust_miniss::cpu::{clear_current_io_state, set_current_io_state, CpuIoState};
    use rust_miniss::io::epoll::EpollBackend;
    use rust_miniss::{CompletionKind, IoProvider, Op};
    use std::os::fd::IntoRawFd;
    use std::task::Poll;

    struct IoStateGuard;

    impl Drop for IoStateGuard {
        fn drop(&mut self) {
            clear_current_io_state();
        }
    }

    let backend = Arc::new(EpollBackend::new().expect("backend"));
    let state = CpuIoState {
        io_backend: backend.clone(),
        io_slots: Default::default(),
    };
    set_current_io_state(&state);
    let _guard = IoStateGuard;

    let fd = tempfile::tempfile().expect("temp file").into_raw_fd();
    let token = backend.submit(Op::Close { fd });
    backend.flush();
    match backend.drain_completions() {
        Poll::Ready(completions) => {
            let (_, result) = completions
                .into_iter()
                .find(|(completion_token, _)| *completion_token == token)
                .expect("close completion");

            assert!(
                matches!(result, Ok(CompletionKind::Close)),
                "close should complete successfully, got {result:?}"
            );
        }
        Poll::Pending => panic!("close should complete synchronously"),
    }
}
