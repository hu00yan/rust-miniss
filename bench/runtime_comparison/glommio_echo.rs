//! glommio HTTP echo server for runtime comparison.

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::net::{TcpListener, TcpStream};

async fn handle_client(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let mut buf = vec![0u8; 4096];

    let body = b"Hello from glommio";
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    );
    let response = [header.as_bytes(), body].concat();

    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stream.write_all(&response).await.is_err() {
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

    glommio::LocalExecutorBuilder::default()
        .spawn(move || async move {
            let listener = TcpListener::bind(&addr)?;
            println!("glommio http_echo listening on {addr}");

            loop {
                let stream = listener.accept().await?;
                glommio::spawn_local(async move {
                    handle_client(stream).await;
                })
                .detach();
            }
        })
        .unwrap()
        .join()
        .unwrap()
}
