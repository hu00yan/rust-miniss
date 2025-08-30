use glommio::io::{TcpListener, TcpStream};
use glommio::net::TcpStream as GlommioTcpStream;

async fn read_http_request(stream: &TcpStream) -> std::io::Result<(String, String, String)> {
    let mut data = Vec::with_capacity(4096);
    let mut buf = vec![0u8; 1024];
    
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if data.len() > 16 * 1024 {
            break;
        }
    }
    
    let text = String::from_utf8_lossy(&data);
    let mut parts = text.split("\r\n").next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let version = parts.next().unwrap_or("").to_string();
    Ok((method, path, version))
}

async fn write_http_response(stream: &TcpStream) -> std::io::Result<()> {
    let body = b"Hello from glommio";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

async fn handle_client(stream: TcpStream) {
    let _ = read_http_request(&stream).await;
    let _ = write_http_response(&stream).await;
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
        .spawn(|| async move {
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