//! HTTP request and response handling for the miniss runtime.
//!
//! This module provides high-level HTTP abstractions built on top of the async TCP functionality.
//! It supports basic HTTP/1.1 request parsing and response generation.

use crate::net::AsyncTcpStream;
#[cfg(target_os = "linux")]
use crate::Buffer;
use futures::io::AsyncWrite;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::io::IoSlice;
use std::pin::Pin;
use std::str;
use std::sync::Arc;

const MAX_HTTP_HEADER_SIZE: usize = 64 * 1024;

/// HTTP request method
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    GET,
    POST,
    PUT,
    DELETE,
    HEAD,
    OPTIONS,
    PATCH,
    Other(String),
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Method::GET => write!(f, "GET"),
            Method::POST => write!(f, "POST"),
            Method::PUT => write!(f, "PUT"),
            Method::DELETE => write!(f, "DELETE"),
            Method::HEAD => write!(f, "HEAD"),
            Method::OPTIONS => write!(f, "OPTIONS"),
            Method::PATCH => write!(f, "PATCH"),
            Method::Other(method) => write!(f, "{method}"),
        }
    }
}

impl From<&str> for Method {
    fn from(method: &str) -> Self {
        if method.eq_ignore_ascii_case("GET") {
            Method::GET
        } else if method.eq_ignore_ascii_case("POST") {
            Method::POST
        } else if method.eq_ignore_ascii_case("PUT") {
            Method::PUT
        } else if method.eq_ignore_ascii_case("DELETE") {
            Method::DELETE
        } else if method.eq_ignore_ascii_case("HEAD") {
            Method::HEAD
        } else if method.eq_ignore_ascii_case("OPTIONS") {
            Method::OPTIONS
        } else if method.eq_ignore_ascii_case("PATCH") {
            Method::PATCH
        } else {
            Method::Other(method.to_string())
        }
    }
}

/// HTTP status code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCode(pub u16);

impl StatusCode {
    pub const OK: StatusCode = StatusCode(200);
    pub const NOT_FOUND: StatusCode = StatusCode(404);
    pub const INTERNAL_SERVER_ERROR: StatusCode = StatusCode(500);
    pub const BAD_REQUEST: StatusCode = StatusCode(400);

    #[inline]
    fn reason_phrase(self) -> &'static str {
        match self.0 {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Unknown",
        }
    }

    #[inline]
    fn wire_len(self) -> usize {
        let digits = match self.0 {
            0..=9 => 1,
            10..=99 => 2,
            100..=999 => 3,
            1000..=9999 => 4,
            _ => 5,
        };
        digits + 1 + self.reason_phrase().len()
    }
}

impl fmt::Display for StatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.0, self.reason_phrase())
    }
}

/// HTTP request
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub version: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn new(method: Method, path: String) -> Self {
        Self {
            method,
            path,
            version: "HTTP/1.1".to_string(),
            headers: HashMap::with_capacity(8),
            body: Vec::new(),
        }
    }

    /// Parse HTTP request from bytes
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let request_str = str::from_utf8(data)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8"))?;

        let (head, body) = request_str
            .split_once("\r\n\r\n")
            .unwrap_or((request_str, ""));
        let mut lines = head.lines();

        // Parse request line
        let request_line = lines
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Empty request"))?;

        let mut parts = request_line.split_whitespace();
        let method_str = parts
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid request line"))?;
        let path = parts
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid request line"))?;
        let version = parts
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid request line"))?;
        if parts.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid request line",
            ));
        }
        if version != "HTTP/1.1" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unsupported HTTP version",
            ));
        }

        let method = Method::from(method_str);
        let path = path.to_string();
        let version = version.to_string();

        // Parse headers
        let mut headers = HashMap::with_capacity(8);

        for line in lines {
            if line.is_empty() {
                break;
            }

            let Some(colon_pos) = line.find(':') else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid HTTP header",
                ));
            };
            let mut name = line[..colon_pos].to_owned();
            if !is_valid_header_name(&name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid HTTP header name",
                ));
            }
            name.make_ascii_lowercase();
            let value = line[colon_pos + 1..].trim().to_string();
            headers.insert(name, value);
        }

        let body = body.as_bytes().to_vec();

        Ok(Request {
            method,
            path,
            version,
            headers,
            body,
        })
    }
}

/// HTTP response
#[derive(Debug, Clone)]
pub struct Response {
    pub status: StatusCode,
    pub version: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    content_length: Option<usize>,
}

impl Response {
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            version: "HTTP/1.1".to_string(),
            headers: HashMap::with_capacity(8),
            body: Vec::new(),
            content_length: None,
        }
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        let body_bytes = body.into();
        self.content_length = Some(body_bytes.len());
        self.body = body_bytes;
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if name.eq_ignore_ascii_case("content-length") {
            if let Ok(len) = value.parse::<usize>() {
                self.content_length = Some(len);
            } else {
                self.headers
                    .insert(name.to_ascii_lowercase(), value.to_string());
            }
        } else {
            self.headers
                .insert(name.to_ascii_lowercase(), value.to_string());
        }
        self
    }

    fn build_headers(&self) -> SmallVec<[u8; 512]> {
        let header_bytes: usize = self
            .headers
            .iter()
            .map(|(name, value)| name.len() + value.len() + 4)
            .sum();
        let content_length = self.content_length.unwrap_or(self.body.len());
        let mut len_buf = itoa::Buffer::new();
        let mut status_buf = itoa::Buffer::new();
        let content_length_bytes = if self.content_length.is_some() {
            len_buf.format(content_length).len()
        } else {
            0
        };
        let fixed_bytes = b"server: miniss/1.0\r\n".len() + content_length_bytes;
        let mut response = SmallVec::<[u8; 512]>::with_capacity(
            self.version.len() + 1 + self.status.wire_len() + fixed_bytes + header_bytes + 2,
        );
        response.extend_from_slice(self.version.as_bytes());
        response.extend_from_slice(b" ");
        response.extend_from_slice(status_buf.format(self.status.0).as_bytes());
        response.extend_from_slice(b" ");
        response.extend_from_slice(self.status.reason_phrase().as_bytes());
        response.extend_from_slice(b"\r\n");
        response.extend_from_slice(b"server: miniss/1.0\r\n");
        if self.content_length.is_some() {
            response.extend_from_slice(b"content-length: ");
            response.extend_from_slice(len_buf.format(content_length).as_bytes());
            response.extend_from_slice(b"\r\n");
        }

        for (name, value) in &self.headers {
            response.extend_from_slice(name.as_bytes());
            response.extend_from_slice(b": ");
            response.extend_from_slice(value.as_bytes());
            response.extend_from_slice(b"\r\n");
        }

        response.extend_from_slice(b"\r\n");
        response
    }

    fn serialize_headers(&self) -> Vec<u8> {
        self.build_headers().into_vec()
    }

    /// Convert response to bytes for sending over TCP
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut response = self.serialize_headers();
        response.extend_from_slice(&self.body);
        response
    }
}

/// HTTP server connection handler
pub struct HttpConnection {
    stream: AsyncTcpStream,
    read_buf: Vec<u8>,
}

impl HttpConnection {
    pub fn new(stream: AsyncTcpStream) -> Self {
        Self {
            stream,
            read_buf: Vec::with_capacity(4096),
        }
    }

    /// Read and parse HTTP request from the connection
    pub async fn read_request(&mut self) -> io::Result<Request> {
        loop {
            if let Some(header_end) = find_header_end(&self.read_buf) {
                if header_end > MAX_HTTP_HEADER_SIZE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "HTTP headers exceed the maximum size",
                    ));
                }
                let body_len = content_length(&self.read_buf[..header_end])?;
                let request_len = header_end
                    .checked_add(4)
                    .and_then(|length| length.checked_add(body_len))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "HTTP request is too large")
                    })?;
                if self.read_buf.len() >= request_len {
                    let request = Request::parse(&self.read_buf[..request_len]);
                    self.read_buf.drain(..request_len);
                    return request;
                }
            }

            if self.read_buf.len() > MAX_HTTP_HEADER_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP headers exceed the maximum size",
                ));
            }

            #[cfg(target_os = "linux")]
            {
                let buffer = Buffer::with_capacity(4096);
                let (n, buffer) = self.stream.read_rent(buffer).await?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed before a complete HTTP request",
                    ));
                }
                self.read_buf.extend_from_slice(&buffer[..n]);
                buffer.recycle();
            }
            #[cfg(not(target_os = "linux"))]
            {
                let mut buffer = [0u8; 4096];
                let n = futures::io::AsyncReadExt::read(&mut self.stream, &mut buffer).await?;
                if n == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed before a complete HTTP request",
                    ));
                }
                self.read_buf.extend_from_slice(&buffer[..n]);
            }
        }
    }

    /// Send HTTP response to the connection
    pub async fn send_response(&mut self, response: Response) -> io::Result<()> {
        let header = response.build_headers();
        write_all_slices(&mut self.stream, &header, &response.body).await
    }
}

/// Write an HTTP response's head and body fully.
///
/// This is a response detail, not a stream capability: the only caller is
/// [`HttpConnection::send_response`], and "write a head, then a body" is the
/// shape HTTP/1.1 needs. It uses `writev(2)` when both parts are still
/// outstanding, so the common one-syscall response stays one syscall.
async fn write_all_slices(
    stream: &mut AsyncTcpStream,
    first: &[u8],
    second: &[u8],
) -> io::Result<()> {
    let mut first = first;
    let mut second = second;

    while !first.is_empty() || !second.is_empty() {
        let n = if second.is_empty() {
            futures::future::poll_fn(|cx| AsyncWrite::poll_write(Pin::new(&mut *stream), cx, first))
                .await?
        } else if first.is_empty() {
            futures::future::poll_fn(|cx| {
                AsyncWrite::poll_write(Pin::new(&mut *stream), cx, second)
            })
            .await?
        } else {
            let bufs = [IoSlice::new(first), IoSlice::new(second)];
            futures::future::poll_fn(|cx| {
                AsyncWrite::poll_write_vectored(Pin::new(&mut *stream), cx, &bufs)
            })
            .await?
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write_all_slices wrote zero bytes",
            ));
        }

        let first_len = first.len();
        if n < first_len {
            first = &first[n..];
            continue;
        }

        first = &[];
        let remaining = n - first_len;
        if remaining >= second.len() {
            second = &[];
        } else {
            second = &second[remaining..];
        }
    }

    Ok(())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> io::Result<usize> {
    let text = str::from_utf8(headers)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP headers"))?;
    let mut parsed = None;
    for line in text.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid HTTP header",
            ));
        };
        if !is_valid_header_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid HTTP header name",
            ));
        }
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
            })?;
            if let Some(previous) = parsed {
                if previous != length {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "conflicting Content-Length headers",
                    ));
                }
            }
            parsed = Some(length);
        } else if name.eq_ignore_ascii_case("transfer-encoding") && !value.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Transfer-Encoding is unsupported",
            ));
        }
    }
    Ok(parsed.unwrap_or(0))
}

fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
        })
}

/// Simple HTTP server trait for handling requests
pub trait HttpHandler: Send + Sync + 'static {
    fn handle(&self, request: Request) -> impl std::future::Future<Output = Response> + Send;
}

/// Basic echo handler that returns the request details
pub struct EchoHandler;

impl HttpHandler for EchoHandler {
    async fn handle(&self, request: Request) -> Response {
        let body = format!(
            "Method: {}\nPath: {}\nVersion: {}\nHeaders: {:#?}\nBody: {}\n",
            request.method,
            request.path,
            request.version,
            request.headers,
            String::from_utf8_lossy(&request.body)
        );

        Response::new(StatusCode::OK)
            .with_header("content-type", "text/plain")
            .with_body(body)
    }
}

/// Static file/text handler
pub struct StaticHandler {
    content: Arc<str>,
    content_type: Arc<str>,
}

impl StaticHandler {
    pub fn new(content: impl Into<String>, content_type: impl Into<String>) -> Self {
        Self {
            content: Arc::<str>::from(content.into()),
            content_type: Arc::<str>::from(content_type.into()),
        }
    }
}

impl HttpHandler for StaticHandler {
    fn handle(&self, _request: Request) -> impl std::future::Future<Output = Response> + Send {
        let content = self.content.clone();
        let content_type = self.content_type.clone();

        async move {
            Response::new(StatusCode::OK)
                .with_header("content-type", content_type.as_ref())
                .with_body(content.as_bytes())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn request_parser_normalizes_method_and_header_names() {
        let request =
            Request::parse(b"gEt /health HTTP/1.1\r\nhOsT: localhost\r\nX-Trace: 42\r\n\r\nbody")
                .unwrap();

        assert_eq!(request.method, Method::GET);
        assert_eq!(request.path, "/health");
        assert_eq!(request.version, "HTTP/1.1");
        assert_eq!(request.headers.get("host"), Some(&"localhost".to_string()));
        assert_eq!(request.headers.get("x-trace"), Some(&"42".to_string()));
        assert_eq!(request.body, b"body");
    }

    #[test]
    fn request_parser_rejects_invalid_utf8() {
        let error = Request::parse(b"GET / HTTP/1.1\r\n\xff\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_parser_rejects_missing_request_line_parts() {
        for request in [b"GET\r\n\r\n".as_slice(), b"GET /\r\n\r\n"] {
            let error = Request::parse(request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn request_parser_rejects_extra_request_line_parts() {
        let error = Request::parse(b"GET / HTTP/1.1 extra\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_parser_rejects_unsupported_http_versions() {
        for version in [b"HTTP/1.0".as_slice(), b"HTTP/2".as_slice()] {
            let request = [b"GET / ", version, b"\r\n\r\n"].concat();
            let error = Request::parse(&request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn request_parser_rejects_malformed_headers() {
        let error = Request::parse(b"GET / HTTP/1.1\r\nbroken-header\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_parser_rejects_invalid_header_names() {
        for header in [
            b"bad name: value".as_slice(),
            b"bad\tname: value".as_slice(),
            b"bad(name): value".as_slice(),
            b" bad: value".as_slice(),
        ] {
            let request = [
                b"GET / HTTP/1.1\r\n".as_slice(),
                header,
                b"\r\n\r\n".as_slice(),
            ]
            .concat();
            let error = Request::parse(&request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn content_length_parser_accepts_whitespace_and_rejects_invalid_values() {
        assert_eq!(
            content_length(b"POST / HTTP/1.1\r\nContent-Length: 7 \r\n").unwrap(),
            7
        );
        let error = content_length(b"POST / HTTP/1.1\r\nContent-Length: nope\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn content_length_parser_rejects_conflicting_duplicates() {
        let headers = b"POST / HTTP/1.1\r\nContent-Length: 7\r\ncontent-length: 8\r\n";
        let error = content_length(headers).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn content_length_parser_rejects_unsupported_transfer_encoding() {
        let error =
            content_length(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn content_length_parser_rejects_invalid_header_names_before_framing() {
        let error = content_length(b"POST / HTTP/1.1\r\nbad name: 1\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn response_serialization_has_matching_body_length() {
        let response = Response::new(StatusCode::OK)
            .with_header("X-Request-ID", "abc")
            .with_body("hello");
        let bytes = response.to_bytes();
        let content_length = b"content-length: 5\r\n";

        assert!(bytes.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(bytes
            .windows(content_length.len())
            .any(|line| line == content_length));
        assert!(bytes.ends_with(b"\r\n\r\nhello"));
    }

    #[test]
    fn response_content_length_header_is_serialized_only_once() {
        let response = Response::new(StatusCode::OK)
            .with_header("Content-Length", "3")
            .with_body("abc");
        let bytes = response.to_bytes();
        let content_length = b"content-length:";

        assert_eq!(
            bytes
                .windows(content_length.len())
                .filter(|part| *part == content_length)
                .count(),
            1
        );
        assert!(bytes.ends_with(b"\r\n\r\nabc"));
    }

    #[test]
    fn request_parser_preserves_unknown_methods() {
        let request = Request::parse(b"PURGE /cache HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(request.method, Method::Other("PURGE".to_owned()));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn request_parser_never_panics_on_arbitrary_bytes(
            data in prop::collection::vec(any::<u8>(), 0..4096)
        ) {
            let _ = Request::parse(&data);
        }

        #[test]
        fn response_serialization_preserves_arbitrary_body(
            body in prop::collection::vec(any::<u8>(), 0..1024)
        ) {
            let bytes = Response::new(StatusCode::OK)
                .with_body(body.clone())
                .to_bytes();
            prop_assert!(bytes.ends_with(&body));
        }
    }
}
