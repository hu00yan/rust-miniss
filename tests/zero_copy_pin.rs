use futures::io::AsyncReadExt;
use rust_miniss::net::AsyncTcpStream;
use rust_miniss::Runtime;
use std::io::Write;
use std::net::TcpListener;
use std::os::fd::AsRawFd;

fn accept_ready(listener: &TcpListener) -> std::net::TcpStream {
    let mut pfd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => loop {
                // SAFETY: `pfd` is a valid pointer to a stack-allocated `pollfd`
                // referencing `listener`'s valid, open file descriptor.
                let ret = unsafe { libc::poll(&mut pfd, 1, 5000) };
                if ret < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                assert!(ret > 0, "timed out waiting for accept");
                break;
            },
            Err(e) => panic!("accept failed: {e}"),
        }
    }
}

#[test]
fn test_buffer_read_exact() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"pinned data").unwrap();

        let stream = accept_ready(&listener);
        stream.set_nonblocking(true).unwrap();
        let mut async_stream = AsyncTcpStream::from(stream);

        let mut buf = [0u8; 11];
        async_stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pinned data");
    });
}

#[test]
fn test_buffer_read_multipart() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"AAA").unwrap();
        client.write_all(b"BBB").unwrap();

        let stream = accept_ready(&listener);
        stream.set_nonblocking(true).unwrap();
        let mut async_stream = AsyncTcpStream::from(stream);

        let mut first = [0u8; 3];
        async_stream.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"AAA");

        let mut second = [0u8; 3];
        async_stream.read_exact(&mut second).await.unwrap();
        assert_eq!(&second, b"BBB");
    });
}

#[test]
fn test_buffer_read_zero_length() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new();
    runtime.block_on(async {
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_nonblocking(true).unwrap();
        client.write_all(b"data").unwrap();
        drop(client);

        let stream = accept_ready(&listener);
        stream.set_nonblocking(true).unwrap();
        let mut async_stream = AsyncTcpStream::from(stream);

        let mut buf = [0u8; 16];
        let n = async_stream.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"data");
    });
}
