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
