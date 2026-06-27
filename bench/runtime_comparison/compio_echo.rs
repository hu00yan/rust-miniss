//! compio HTTP echo server for runtime comparison.

use compio::io::AsyncWriteExt;
use compio::net::{TcpListener, TcpStream};
use compio_io::AsyncRead;

async fn handle_client(mut stream: TcpStream) {
    let mut buf = vec![0u8; 4096];
    let body = b"Hello from compio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();

    loop {
        let read_result = stream.read(buf).await;
        buf = read_result.1;
        match read_result.0 {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stream.write_all(response.clone()).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    let mut addr = String::from("0.0.0.0:8080");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--addr" || arg == "-a" {
            if let Some(v) = args.next() {
                addr = v;
            }
        }
    }

    let runtime = compio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let listener = TcpListener::bind(&addr).await?;
        println!("compio http_echo listening on {addr}");

        loop {
            let (stream, _) = listener.accept().await?;
            compio::runtime::spawn(async move {
                handle_client(stream).await;
            })
            .detach();
        }
    })
}
