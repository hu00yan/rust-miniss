#![cfg(test)]

use rust_miniss::net::{AsyncTcpListener, AsyncTcpStream};
use rust_miniss::Runtime;

/// Stand-in for `libc::iovec` so this integration test stays free of direct
/// dependency on `libc` / `smallvec` (not re-exported by the library crate).
#[repr(C)]
#[derive(Clone, Copy)]
struct FakeIovec {
    base: *mut u8,
    len: usize,
}

/// Structural proof (S0-4 / rm-06): kernel-visible iovec metadata must live in
/// immovable heap storage. `Box<[T]>` keeps `as_ptr()` stable across moves that
/// mimic PendingOp construction + slab insertion. Covers counts that would have
/// been inline (1–4) and spilled (5+) under `SmallVec<[_; 4]>`.
#[test]
fn test_boxed_iovec_stable_across_owner_move() {
    for count in [1usize, 2, 3, 4, 5, 8] {
        let mut chunks: Vec<Vec<u8>> = (0..count).map(|i| vec![(i + 1) as u8; 32]).collect();
        let iovecs: Box<[FakeIovec]> = chunks
            .iter_mut()
            .map(|c| FakeIovec {
                base: c.as_mut_ptr(),
                len: c.len(),
            })
            .collect();

        let sqe_ptr = iovecs.as_ptr();
        // Move owner the way submit() does (local → PendingOp → slab).
        let owned = iovecs;
        assert_eq!(
            sqe_ptr,
            owned.as_ptr(),
            "Box<[iovec]> pointer must survive move (count={count})"
        );
        for i in 0..count {
            let iov = unsafe { *sqe_ptr.add(i) };
            assert_eq!(iov.len, 32);
            assert_eq!(unsafe { *iov.base }, (i + 1) as u8);
        }
        let _keep_chunks = chunks;
        let _keep_owned = owned;
    }
}

/// Documents why inline array storage is unsafe for SQE arguments: the storage
/// relocates when the owning struct is moved into heap containers (slab-like),
/// invalidating any previously captured `as_ptr()`.
/// (Mirrors the SmallVec<[_; 4]> inline hazard fixed in rm-06.)
#[test]
fn test_inline_array_iovec_ptr_moves_with_owner() {
    struct InlineOwner {
        data: [FakeIovec; 4],
    }
    let owner = InlineOwner {
        data: [FakeIovec {
            base: std::ptr::null_mut(),
            len: 7,
        }; 4],
    };
    let ptr_before = owner.data.as_ptr() as usize;
    // Move into a Vec (slab-like heap storage) — inline field address changes.
    let mut slab = Vec::with_capacity(4);
    slab.push(owner);
    let ptr_after = slab[0].data.as_ptr() as usize;
    assert_ne!(
        ptr_before, ptr_after,
        "inline array as_ptr must change when owner is moved into a slab — S0-4 hazard"
    );
}

#[test]
fn test_tcp_read_vectored_returns_pending() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
        let mut buf1 = [0u8; 64];
        let mut buf2 = [0u8; 64];
        let mut bufs = [
            std::io::IoSliceMut::new(&mut buf1),
            std::io::IoSliceMut::new(&mut buf2),
        ];
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        use rust_miniss::AsyncRead;
        let result = std::pin::Pin::new(&mut stream).poll_read_vectored(&mut cx, &mut bufs);
        assert!(
            matches!(result, std::task::Poll::Pending),
            "expected Pending on first poll, got {result:?}"
        );
    });
}

#[test]
fn test_tcp_write_vectored_returns_pending() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut stream = AsyncTcpStream::connect(addr).await.unwrap();
        let bufs = [
            std::io::IoSlice::new(b"hello"),
            std::io::IoSlice::new(b"world"),
        ];
        let waker = futures::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        use rust_miniss::AsyncWrite;
        let result = std::pin::Pin::new(&mut stream).poll_write_vectored(&mut cx, &bufs);
        assert!(
            matches!(
                result,
                std::task::Poll::Pending | std::task::Poll::Ready(Ok(_))
            ),
            "expected Pending or Ready(Ok(_)) on first poll, got {result:?}"
        );
    });
}

#[test]
fn test_tcp_vectored_write_read_roundtrip() {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut writer = AsyncTcpStream::connect(addr).await.unwrap();
        let (mut reader, _) = listener.accept().await.unwrap();

        let bufs = [
            std::io::IoSlice::new(b"hello "),
            std::io::IoSlice::new(b"world"),
        ];
        let n = writer.write_vectored(&bufs).await.unwrap();
        assert_eq!(n, 11);

        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf[..11], b"hello world");
    });
}

/// Round-trip with four iovecs (former SmallVec inline boundary).
#[test]
fn test_tcp_vectored_write_read_four_iovecs() {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut writer = AsyncTcpStream::connect(addr).await.unwrap();
        let (mut reader, _) = listener.accept().await.unwrap();

        let parts = [b"aa".as_slice(), b"bb", b"cc", b"dd"];
        let bufs: Vec<std::io::IoSlice<'_>> =
            parts.iter().map(|p| std::io::IoSlice::new(p)).collect();
        let expected: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        let n = writer.write_vectored(&bufs).await.unwrap();
        assert_eq!(n, expected.len());
        writer.close().await.unwrap();

        let mut buf = [0u8; 64];
        let mut total = 0usize;
        while total < expected.len() {
            let n = reader.read(&mut buf[total..]).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(&buf[..total], expected.as_slice());
    });
}

/// Round-trip with five iovecs (would spill past SmallVec<[_; 4]> inline).
#[test]
fn test_tcp_vectored_write_read_five_iovecs_spill() {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        let mut writer = AsyncTcpStream::connect(addr).await.unwrap();
        let (mut reader, _) = listener.accept().await.unwrap();

        let parts = [b"1".as_slice(), b"2", b"3", b"4", b"5"];
        let bufs: Vec<std::io::IoSlice<'_>> =
            parts.iter().map(|p| std::io::IoSlice::new(p)).collect();
        let expected: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
        let n = writer.write_vectored(&bufs).await.unwrap();
        assert_eq!(n, expected.len());
        writer.close().await.unwrap();

        let mut buf = [0u8; 64];
        let mut total = 0usize;
        while total < expected.len() {
            let n = reader.read(&mut buf[total..]).await.unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        assert_eq!(&buf[..total], expected.as_slice());
    });
}

// These tests deliberately submit `readv`/`writev` through io_uring instead of
// merely proving the pointer shape.  The public `Async*` API normalizes a
// single IoSlice to read/write, so the kernel tests cover the former inline
// SmallVec range (2--4) plus the first spilled count (5); the one-slice case is
// covered by the boxed-storage structural test above.
//
// `Runtime::new()` currently panics when an io_uring-capable Linux build is
// executed in a sandbox that denies `io_uring_setup`.  Catch that specific
// availability boundary so this remains a Linux regression test rather than a
// false negative for restricted CI containers.
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn uring_runtime_or_skip() -> Option<Runtime> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(Runtime::new))
        .map_err(|_| eprintln!("skipping io_uring delayed-submit regression: io_uring unavailable"))
        .ok()
}

/// Allocator and Vec growth happen after the SQE has captured the iovec pointer
/// but before the task yields to the runtime's park/flush boundary.  This makes
/// a SmallVec-inline implementation fail under Miri/ASan or a real kernel,
/// while boxed iovec metadata remains valid.
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn relocation_pressure() {
    let mut allocations = Vec::with_capacity(1);
    for size in 1..=256 {
        allocations.push(vec![0xA5u8; size * 97]);
    }
    assert_eq!(allocations.len(), 256);
}

/// Kernel-executed delayed-submit readv regression for former inline and spill
/// iovec counts.  The peer cannot write until the Readv SQE is queued; only
/// after allocation/vector relocation pressure does this task yield, causing
/// the runtime's parked flush to submit the SQE to the kernel.
#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn linux_uring_delayed_submit_readv_survives_relocation_pressure() {
    use futures::future::poll_fn;
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::mpsc;
    use std::time::Duration;

    let Some(runtime) = uring_runtime_or_skip() else {
        return;
    };

    for count in 2usize..=5 {
        let mut listener =
            AsyncTcpListener::bind("127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let expected: Vec<u8> = (0..count)
            .flat_map(|part| std::iter::repeat_n((part + 1) as u8, 23))
            .collect();
        let (connected_tx, connected_rx) = mpsc::sync_channel(1);
        let (write_tx, write_rx) = mpsc::sync_channel(1);
        let peer_payload = expected.clone();
        let peer = std::thread::spawn(move || {
            let mut socket = TcpStream::connect(addr).expect("peer connect");
            connected_tx.send(()).expect("notify accepted peer");
            write_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release delayed peer write");
            socket.write_all(&peer_payload).expect("peer write payload");
        });

        connected_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("peer connected before accept");

        runtime.block_on(async {
            use rust_miniss::AsyncRead;

            let (mut reader, _) = listener.accept().await.expect("accept peer");
            let mut chunks: Vec<Vec<u8>> = (0..count).map(|_| vec![0; 23]).collect();
            let mut bufs: Vec<std::io::IoSliceMut<'_>> = chunks
                .iter_mut()
                .map(|chunk| std::io::IoSliceMut::new(chunk))
                .collect();
            let mut queued = false;
            let n = poll_fn(|cx| {
                let poll = std::pin::Pin::new(&mut reader).poll_read_vectored(cx, &mut bufs);
                if !queued {
                    assert!(poll.is_pending(), "readv must queue before peer writes");
                    queued = true;
                    relocation_pressure();
                    write_tx
                        .send(())
                        .expect("release peer after SQE construction");
                }
                poll
            })
            .await
            .expect("kernel readv completion");
            assert_eq!(n, expected.len(), "readv count={count}");
            assert_eq!(chunks.into_iter().flatten().collect::<Vec<_>>(), expected);
        });

        peer.join().expect("peer thread");
    }
}

/// Fill a nonblocking socket without involving the runtime, so the next public
/// `poll_write_vectored` cannot complete on its optimistic `writev(2)` fast
/// path and must queue an io_uring Writev SQE.
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn fill_send_buffer_until_would_block(fd: std::os::fd::RawFd) {
    let bytes = [0x3Cu8; 8192];
    loop {
        // SAFETY: `fd` belongs to the live AsyncTcpStream and `bytes` remains
        // valid for this synchronous syscall.
        let written = unsafe { libc::send(fd, bytes.as_ptr().cast(), bytes.len(), 0) };
        if written >= 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        assert!(
            matches!(error.raw_os_error(), Some(libc::EAGAIN)),
            "unexpected send-buffer fill error: {error}"
        );
        return;
    }
}

/// Kernel-executed delayed-submit writev regression.  The server does not
/// drain until the Writev SQE has been constructed and relocation pressure has
/// run, which exercises the actual SQE-to-terminal-CQE lifetime rather than a
/// structural stand-in.
#[test]
#[cfg(all(target_os = "linux", io_backend = "io_uring"))]
fn linux_uring_delayed_submit_writev_survives_relocation_pressure() {
    use futures::future::poll_fn;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::sync::mpsc;
    use std::time::Duration;

    let Some(runtime) = uring_runtime_or_skip() else {
        return;
    };

    for count in 2usize..=5 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind peer listener");
        let addr = listener.local_addr().expect("peer listener address");
        let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
        let (drain_tx, drain_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept client");
            let receive_buffer = 4096i32;
            // SAFETY: socket fd is live and the option value has the expected
            // type and size for SO_RCVBUF.
            let set_rcvbuf = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&receive_buffer as *const i32).cast(),
                    std::mem::size_of_val(&receive_buffer) as libc::socklen_t,
                )
            };
            assert_eq!(set_rcvbuf, 0, "set peer receive buffer");
            accepted_tx.send(()).expect("notify peer acceptance");
            drain_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release peer drain");
            let mut scratch = [0u8; 8192];
            loop {
                match socket.read(&mut scratch) {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(error) => panic!("peer drain failed: {error}"),
                }
                if done_rx.try_recv().is_ok() {
                    return;
                }
            }
        });
        let client = TcpStream::connect(addr).expect("connect client");
        accepted_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("peer accepted client");

        runtime.block_on(async {
            use rust_miniss::AsyncWrite;

            let mut writer = AsyncTcpStream::from(client);
            let send_buffer = 4096i32;
            // SAFETY: writer owns this live fd; the option value is valid for
            // SO_SNDBUF and only alters this test socket's kernel buffering.
            let set_sndbuf = unsafe {
                libc::setsockopt(
                    writer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&send_buffer as *const i32).cast(),
                    std::mem::size_of_val(&send_buffer) as libc::socklen_t,
                )
            };
            assert_eq!(set_sndbuf, 0, "set client send buffer");
            fill_send_buffer_until_would_block(writer.as_raw_fd());

            let parts: Vec<Vec<u8>> = (0..count).map(|part| vec![(part + 1) as u8; 23]).collect();
            let expected_len: usize = parts.iter().map(Vec::len).sum();
            let bufs: Vec<std::io::IoSlice<'_>> = parts
                .iter()
                .map(|part| std::io::IoSlice::new(part))
                .collect();
            let mut queued = false;
            let n = poll_fn(|cx| {
                let poll = std::pin::Pin::new(&mut writer).poll_write_vectored(cx, &bufs);
                if !queued {
                    assert!(
                        poll.is_pending(),
                        "writev must queue with a full send buffer"
                    );
                    queued = true;
                    relocation_pressure();
                    drain_tx
                        .send(())
                        .expect("release peer after SQE construction");
                }
                poll
            })
            .await
            .expect("kernel writev completion");
            assert_eq!(n, expected_len, "writev count={count}");
            done_tx.send(()).expect("finish peer drain");
            drop(writer);
        });

        peer.join().expect("peer thread");
    }
}
