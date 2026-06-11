use rust_miniss::{
    AsyncFile, CompletionKind, Executor, IoProvider, MultiCoreRuntime, Op, Runtime, TaskError,
};
use std::sync::Arc;
use std::task::{Context, Poll};
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
    all(target_os = "linux", any(feature = "epoll", io_backend = "epoll"))
))]
#[test]
fn mio_backend_close_completes_successfully_once() {
    use rust_miniss::cpu::{clear_current_io_state, set_current_io_state, CpuIoState};
    use rust_miniss::io::epoll::EpollBackend;
    use std::os::fd::IntoRawFd;

    struct IoStateGuard;

    impl Drop for IoStateGuard {
        fn drop(&mut self) {
            clear_current_io_state();
        }
    }

    let backend = Arc::new(EpollBackend::new().expect("backend"));
    let mut state = CpuIoState::default();
    state.io_backend = backend.clone();
    set_current_io_state(Arc::new(state));
    let _guard = IoStateGuard;

    let fd = tempfile::tempfile().expect("temp file").into_raw_fd();
    let token = backend.submit(Op::Close { fd });
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);

    match backend.poll_complete(&mut cx) {
        Poll::Ready(completions) => {
            let (_, _, result) = completions
                .into_iter()
                .find(|(completion_token, _, _)| *completion_token == token)
                .expect("close completion");

            assert!(
                matches!(result, Ok(CompletionKind::Close)),
                "close should complete successfully, got {result:?}"
            );
        }
        Poll::Pending => panic!("close should complete synchronously"),
    }
}
