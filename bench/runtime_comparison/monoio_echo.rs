//! monoio HTTP echo server for runtime comparison.

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};

async fn handle_client(stream: TcpStream) {
    let mut stream = stream;
    let _ = stream.set_nodelay(true);
    let mut buf = vec![0u8; 4096];

    let body = b"Hello from monoio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();

    loop {
        let (result, buf2) = stream.read(buf).await;
        buf = buf2;
        match result {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stream.write_all(response.clone()).await.0.is_err() {
                    break;
                }
            }
        }
    }
}

#[monoio::main(timer = false)]
async fn main() -> std::io::Result<()> {
    let mut addr = String::from("0.0.0.0:8081");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--addr" || arg == "-a" {
            if let Some(v) = args.next() {
                addr = v;
            }
        }
    }

    let listener = TcpListener::bind(&addr)?;
    println!("monoio http_echo listening on {addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        monoio::spawn(async move {
            handle_client(stream).await;
        });
    }
}
