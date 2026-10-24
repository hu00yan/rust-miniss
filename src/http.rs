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
/// Maximum accepted request body size (8 MiB).
const MAX_HTTP_BODY_SIZE: usize = 8 * 1024 * 1024;

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
        if method == "GET" {
            Method::GET
        } else if method == "POST" {
            Method::POST
        } else if method == "PUT" {
            Method::PUT
        } else if method == "DELETE" {
            Method::DELETE
        } else if method == "HEAD" {
            Method::HEAD
        } else if method == "OPTIONS" {
            Method::OPTIONS
        } else if method == "PATCH" {
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

    /// Parse HTTP request from bytes. Bodies are limited to 8 MiB.
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let header_end = find_header_end(data).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete HTTP headers: missing CRLF delimiter",
            )
        })?;
        if header_end > MAX_HTTP_HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP headers exceed the maximum size",
            ));
        }
        let header = &data[..header_end];
        let body_start = if header_end < data.len() {
            header_end + 4
        } else {
            data.len()
        };
        let body = &data[body_start..];
        if body.len() > MAX_HTTP_BODY_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request body exceeds the maximum size",
            ));
        }
        let request_str = str::from_utf8(header)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8 in headers"))?;
        let declared_body_len = content_length(header)?;
        if body.len() != declared_body_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request body length does not match Content-Length",
            ));
        }
        let mut lines = request_str.lines();

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
        if !is_valid_header_name(method_str) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid HTTP method token",
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
            let raw_value = &line[colon_pos + 1..];
            if !is_valid_header_value(raw_value) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid HTTP header value",
                ));
            }
            let value = raw_value.trim().to_string();
            headers.insert(name, value);
        }

        let body = body.to_vec();

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
            // The wire length always reflects the body actually serialized.
            self.content_length = Some(self.body.len());
        } else if is_valid_response_header(name, value) {
            self.headers
                .insert(name.to_ascii_lowercase(), value.to_string());
        }
        self
    }

    fn build_headers(&self) -> SmallVec<[u8; 512]> {
        // This response type only supports HTTP/1.1; never serialize arbitrary public input here.
        let version = if self.version == "HTTP/1.1" {
            self.version.as_str()
        } else {
            "HTTP/1.1"
        };
        let header_bytes: usize = self
            .headers
            .iter()
            .filter(|(name, value)| is_valid_response_header(name, value))
            .map(|(name, value)| name.len() + value.len() + 4)
            .sum();
        let content_length = self.body.len();
        let has_content_length = self.content_length.is_some()
            || self
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("content-length"));
        let mut len_buf = itoa::Buffer::new();
        let mut status_buf = itoa::Buffer::new();
        let content_length_bytes = if has_content_length {
            len_buf.format(content_length).len()
        } else {
            0
        };
        let fixed_bytes = b"server: miniss/1.0\r\n".len() + content_length_bytes;
        let mut response = SmallVec::<[u8; 512]>::with_capacity(
            version.len() + 1 + self.status.wire_len() + fixed_bytes + header_bytes + 2,
        );
        response.extend_from_slice(version.as_bytes());
        response.extend_from_slice(b" ");
        response.extend_from_slice(status_buf.format(self.status.0).as_bytes());
        response.extend_from_slice(b" ");
        response.extend_from_slice(self.status.reason_phrase().as_bytes());
        response.extend_from_slice(b"\r\n");
        response.extend_from_slice(b"server: miniss/1.0\r\n");
        if has_content_length {
            response.extend_from_slice(b"content-length: ");
            response.extend_from_slice(len_buf.format(content_length).as_bytes());
            response.extend_from_slice(b"\r\n");
        }

        for (name, value) in &self.headers {
            if !is_valid_response_header(name, value) {
                continue;
            }
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
    validate_crlf_line_endings(headers)?;
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
            if length > MAX_HTTP_BODY_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP request body exceeds the maximum size",
                ));
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

fn is_valid_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte) || byte >= 0x80)
}

fn is_valid_response_header(name: &str, value: &str) -> bool {
    !name.eq_ignore_ascii_case("content-length")
        // This serializer does not apply chunk framing, so never emit a
        // Transfer-Encoding header that conflicts with its generated length.
        && !name.eq_ignore_ascii_case("transfer-encoding")
        && is_valid_header_name(name)
        && is_valid_header_value(value)
}

fn validate_crlf_line_endings(headers: &[u8]) -> io::Result<()> {
    for (index, byte) in headers.iter().copied().enumerate() {
        let valid_lf = byte != b'\n' || index > 0 && headers[index - 1] == b'\r';
        let valid_cr = byte != b'\r' || headers.get(index + 1) == Some(&b'\n');
        if !valid_lf || !valid_cr {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP headers must use CRLF line endings",
            ));
        }
    }
    Ok(())
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
        let request = Request::parse(
            b"GET /health HTTP/1.1\r\nhOsT: localhost\r\nX-Trace: 42\r\nContent-Length: 4\r\n\r\nbody",
        )
        .unwrap();

        assert_eq!(request.method, Method::GET);
        assert_eq!(request.path, "/health");
        assert_eq!(request.version, "HTTP/1.1");
        assert_eq!(request.headers.get("host"), Some(&"localhost".to_string()));
        assert_eq!(request.headers.get("x-trace"), Some(&"42".to_string()));
        assert_eq!(request.body, b"body");
    }

    #[test]
    fn request_parser_rejects_missing_header_delimiter() {
        let error = Request::parse(b"GET / HTTP/1.1").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_parser_rejects_bare_lf_header_lines() {
        let error = Request::parse(b"GET / HTTP/1.1\nHost: localhost\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_parser_rejects_invalid_utf8_in_headers() {
        let error = Request::parse(b"GET / HTTP/1.1\r\n\xff\r\n\r\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_parser_accepts_non_utf8_body_bytes() {
        let request =
            Request::parse(b"POST /upload HTTP/1.1\r\nContent-Length: 2\r\n\r\n\xff\xfe").unwrap();
        assert_eq!(request.body, b"\xff\xfe");
    }

    #[test]
    fn request_parser_preserves_case_sensitive_methods() {
        let request = Request::parse(b"get / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(request.method, Method::Other("get".to_owned()));
    }

    #[test]
    fn request_parser_rejects_content_length_mismatches_and_unframed_bodies() {
        for request in [
            b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nx".as_slice(),
            b"POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\nxy",
            b"POST / HTTP/1.1\r\n\r\nx",
        ] {
            let error = Request::parse(request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn request_parser_accepts_zero_length_and_unframed_empty_bodies() {
        for request in [
            b"POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\n".as_slice(),
            b"POST / HTTP/1.1\r\n\r\n",
        ] {
            assert!(Request::parse(request).unwrap().body.is_empty());
        }
    }

    #[test]
    fn request_parser_rejects_control_bytes_in_header_values() {
        for value in [b"bad\0value".as_slice(), b"bad\x7fvalue"] {
            let request = [b"GET / HTTP/1.1\r\nX-Test: ".as_slice(), value, b"\r\n\r\n"].concat();
            let error = Request::parse(&request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }

        let request = Request::parse(b"GET / HTTP/1.1\r\nX-Test: left\tright\r\n\r\n").unwrap();
        assert_eq!(
            request.headers.get("x-test"),
            Some(&"left\tright".to_owned())
        );
    }

    #[test]
    fn request_parser_rejects_non_token_method_bytes() {
        for method in [b"G\0ET".as_slice(), b"G\x7fET"] {
            let request = [method, b" / HTTP/1.1\r\n\r\n"].concat();
            let error = Request::parse(&request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
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
    fn content_length_parser_rejects_oversized_bodies() {
        let headers = b"POST / HTTP/1.1\r\nContent-Length: 8388609\r\n";
        let error = content_length(headers).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
    fn response_version_cannot_inject_status_or_header_lines() {
        let mut response = Response::new(StatusCode::OK);
        response.version = "HTTP/1.1\r\nX-Injected: yes".to_owned();

        let bytes = response.to_bytes();
        assert!(bytes.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(!bytes
            .windows(b"X-Injected".len())
            .any(|part| part == b"X-Injected"));
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
    fn response_serializer_rejects_transfer_encoding_without_chunking() {
        let mut direct_header = Response::new(StatusCode::OK).with_body("abc");
        direct_header
            .headers
            .insert("transfer-encoding".to_owned(), "chunked".to_owned());
        let responses = [
            Response::new(StatusCode::OK)
                .with_header("Transfer-Encoding", "chunked")
                .with_body("abc"),
            direct_header,
        ];

        for response in responses {
            let bytes = response.to_bytes();
            assert!(!bytes
                .windows(b"transfer-encoding".len())
                .any(|part| part.eq_ignore_ascii_case(b"transfer-encoding")));
            assert!(bytes
                .windows(b"content-length: 3\r\n".len())
                .any(|line| line == b"content-length: 3\r\n"));
            assert!(bytes.ends_with(b"\r\n\r\nabc"));
        }
    }

    #[test]
    fn response_content_length_cannot_be_overridden_after_body() {
        let bytes = Response::new(StatusCode::OK)
            .with_body("abc")
            .with_header("Content-Length", "99")
            .to_bytes();
        assert!(bytes
            .windows(b"content-length: 3\r\n".len())
            .any(|line| line == b"content-length: 3\r\n"));
        assert!(!bytes
            .windows(b"content-length: 99\r\n".len())
            .any(|line| line == b"content-length: 99\r\n"));
    }

    #[test]
    fn response_headers_reject_crlf_in_names_and_values() {
        let mut response = Response::new(StatusCode::OK)
            .with_header("x-safe", "ok\r\nx-injected: yes")
            .with_header("x-name\r\nx-injected: yes", "ok");
        response
            .headers
            .insert("x-direct".to_owned(), "ok\r\nx-injected: yes".to_owned());
        response
            .headers
            .insert("content-length".to_owned(), "99".to_owned());
        let bytes = response.to_bytes();
        assert!(!bytes
            .windows(b"x-injected".len())
            .any(|part| part == b"x-injected"));
        assert_eq!(
            bytes
                .windows(b"content-length:".len())
                .filter(|part| *part == b"content-length:")
                .count(),
            1
        );
        assert!(bytes
            .windows(b"content-length: 0\r\n".len())
            .any(|line| line == b"content-length: 0\r\n"));
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
