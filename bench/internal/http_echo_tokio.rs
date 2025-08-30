use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_http_request(stream: &mut TcpStream) -> std::io::Result<(String, String, String)> {
    let mut data = Vec::with_capacity(4096);
    let mut buf = [0u8; 1024];
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

async fn write_http_response(stream: &mut TcpStream) -> std::io::Result<()> {
    let body = b"Hello from rust-miniss";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

async fn handle_client(mut stream: TcpStream) {
    let _ = read_http_request(&mut stream).await;
    let _ = write_http_response(&mut stream).await;
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut addr = String::from("0.0.0.0:0"); // Use port 0 to get a random available port
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--addr" || arg == "-a" {
            if let Some(v) = args.next() {
                addr = v;
            }
        }
    }

    let listener = TcpListener::bind(&addr).await?;
    let actual_addr = listener.local_addr()?;
    println!("tokio http_echo listening on {}", actual_addr);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            handle_client(stream).await;
        });
    }
}
