//! rm-08 fallback-backend regressions.
//!
//! These deliberately drive `EpollBackend` rather than `Runtime`: Linux
//! runtime builds normally select io_uring, while this test must exercise the
//! mio/epoll (and macOS kqueue-via-mio) containment path.

#![cfg(any(io_backend = "epoll", all(target_os = "macos", io_backend = "kqueue")))]

use rust_miniss::io::{CompletionKind, EpollBackend, IoError, IoProvider, Op};
use rust_miniss::Buffer;
use smallvec::smallvec;
use std::io::Write;
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::task::Poll;
use std::time::{Duration, Instant};

fn completion(
    backend: &EpollBackend,
    expected: rust_miniss::IoToken,
) -> Result<CompletionKind, IoError> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Poll::Ready(completions) = backend.drain_completions() {
            for (token, result) in completions {
                if token == expected {
                    return result;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for fallback completion"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn fallback_file_read_publishes_only_short_read_and_eof_lengths() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(b"abc").expect("write fixture");
    file.flush().expect("flush fixture");
    let backend = EpollBackend::new().expect("fallback backend");

    let token = backend.submit(Op::ReadFile {
        fd: file.as_raw_fd(),
        offset: 0,
        len: 32,
    });
    match completion(&backend, token).expect("short file read") {
        CompletionKind::ReadFile { bytes_read, data } => {
            assert_eq!(bytes_read, 3);
            assert_eq!(data.len(), 3, "logical length must equal initialized bytes");
            assert_eq!(&data[..], b"abc");
        }
        other => panic!("unexpected completion: {other:?}"),
    }

    let eof = backend.submit(Op::ReadFile {
        fd: file.as_raw_fd(),
        offset: 3,
        len: 32,
    });
    match completion(&backend, eof).expect("EOF read") {
        CompletionKind::ReadFile { bytes_read, data } => {
            assert_eq!(bytes_read, 0);
            assert!(data.is_empty(), "EOF must not publish spare capacity");
        }
        other => panic!("unexpected completion: {other:?}"),
    }
}

#[test]
fn fallback_file_read_reports_error_without_a_buffer_completion() {
    let backend = EpollBackend::new().expect("fallback backend");
    let token = backend.submit(Op::ReadFile {
        fd: -1,
        offset: 0,
        len: 8,
    });

    assert!(matches!(completion(&backend, token), Err(IoError::Io(_))));
}

#[test]
fn fallback_tcp_read_publishes_only_bytes_kernel_returned_and_eof_is_empty() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("listener address");
    let mut client = TcpStream::connect(address).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    server.set_nonblocking(true).expect("nonblocking server");
    let backend = EpollBackend::new().expect("fallback backend");

    let read = backend.submit(Op::Read {
        fd: server.as_raw_fd(),
        offset: 0,
        len: 32,
    });
    client.write_all(b"short").expect("write short payload");
    match completion(&backend, read).expect("short TCP read") {
        CompletionKind::Read { bytes_read, data } => {
            assert_eq!(bytes_read, 5);
            assert_eq!(data.len(), 5);
            assert_eq!(&data[..], b"short");
        }
        other => panic!("unexpected completion: {other:?}"),
    }

    client.shutdown(Shutdown::Write).expect("close writer");
    let eof = backend.submit(Op::Read {
        fd: server.as_raw_fd(),
        offset: 0,
        len: 32,
    });
    match completion(&backend, eof).expect("TCP EOF") {
        CompletionKind::Read { bytes_read, data } => {
            assert_eq!(bytes_read, 0);
            assert!(
                data.is_empty(),
                "EOF must not expose initialized-looking spare bytes"
            );
        }
        other => panic!("unexpected completion: {other:?}"),
    }
}

#[test]
fn fallback_tcp_readv_publishes_each_buffer_only_up_to_short_read() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let address = listener.local_addr().expect("listener address");
    let mut client = TcpStream::connect(address).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    server.set_nonblocking(true).expect("nonblocking server");
    let backend = EpollBackend::new().expect("fallback backend");

    let readv = backend.submit(Op::ReadVectored {
        fd: server.as_raw_fd(),
        buffers: smallvec![Buffer::with_capacity(3), Buffer::with_capacity(5)],
    });
    client.write_all(b"vector").expect("write vector payload");
    match completion(&backend, readv).expect("short readv") {
        CompletionKind::ReadVectored {
            bytes_read,
            buffers,
        } => {
            assert_eq!(bytes_read, 6);
            assert_eq!(buffers.iter().map(Buffer::len).sum::<usize>(), 6);
            assert_eq!(&buffers[0][..], b"vec");
            assert_eq!(&buffers[1][..], b"tor");
        }
        other => panic!("unexpected completion: {other:?}"),
    }
}

#[test]
fn fallback_udp_read_zero_fills_before_safe_recv_and_publishes_datagram_length() {
    let receiver = UdpSocket::bind("127.0.0.1:0").expect("UDP receiver");
    receiver
        .set_nonblocking(true)
        .expect("nonblocking receiver");
    let sender = UdpSocket::bind("127.0.0.1:0").expect("UDP sender");
    let backend = EpollBackend::new().expect("fallback backend");

    let token = backend.submit(Op::UdpRecv {
        fd: receiver.as_raw_fd(),
        buffer: Buffer::new_uninitialized(32),
    });
    sender
        .send_to(b"udp", receiver.local_addr().expect("receiver address"))
        .expect("send datagram");

    match completion(&backend, token).expect("UDP receive") {
        CompletionKind::UdpRecv {
            bytes_read, buffer, ..
        } => {
            assert_eq!(bytes_read, 3);
            assert_eq!(buffer.len(), 3);
            assert_eq!(&buffer[..], b"udp");
        }
        other => panic!("unexpected completion: {other:?}"),
    }
}
