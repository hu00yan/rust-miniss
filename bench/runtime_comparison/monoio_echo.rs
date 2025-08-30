use monoio::io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};

async fn read_http_request(stream: &mut TcpStream) -> std::io::Result<(String, String, String)> {
    let mut data = Vec::with_capacity(4096);
    let mut buf = vec![0u8; 1024];
    
    loop {
        let (result, buf_read) = stream.read(buf).await;
        buf = buf_read;
        let n = result?;
        
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
    let body = b"Hello from monoio";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    
    let (result, _) = stream.write_all(resp.as_bytes()).await;
    result?;
    let (result, _) = stream.write_all(body).await;
    result?;
    
    Ok(())
}

async fn handle_client(mut stream: TcpStream) {
    let _ = read_http_request(&mut stream).await;
    let _ = write_http_response(&mut stream).await;
}

#[monoio::main]
async fn main() -> std::io::Result<()> {
    let mut addr = String::from("0.0.0.0:8080");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--addr" || arg == "-a" {
            if let Some(v) = args.next() {
                addr = v;
            }
        }
    }

    let listener = TcpListener::bind(&addr).await?;
    println!("monoio http_echo listening on {addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        monoio::spawn(async move {
            handle_client(stream).await;
        });
    }
}