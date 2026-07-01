//! HTTP request and response handling for the miniss runtime.
//!
//! This module provides high-level HTTP abstractions built on top of the async TCP functionality.
//! It supports basic HTTP/1.1 request parsing and response generation.

use crate::net::AsyncTcpStream;
use futures::io::AsyncReadExt;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::str;
use std::sync::Arc;
use smallvec::SmallVec;

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

        let method = Method::from(method_str);
        let path = path.to_string();
        let version = version.to_string();

        // Parse headers
        let mut headers = HashMap::with_capacity(8);

        for line in lines {
            if line.is_empty() {
                break;
            }

            if let Some(colon_pos) = line.find(':') {
                let mut name = line[..colon_pos].trim().to_owned();
                name.make_ascii_lowercase();
                let value = line[colon_pos + 1..].trim().to_string();
                headers.insert(name, value);
            }
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
}

impl HttpConnection {
    pub fn new(stream: AsyncTcpStream) -> Self {
        Self { stream }
    }

    /// Read and parse HTTP request from the connection
    pub async fn read_request(&mut self) -> io::Result<Request> {
        let mut buffer = [0u8; 4096];
        let n = self.stream.read(&mut buffer).await?;
        let buffer = &buffer[..n];
        Request::parse(buffer)
    }

    /// Send HTTP response to the connection
    pub async fn send_response(&mut self, response: Response) -> io::Result<()> {
        let header = response.build_headers();
        self.stream.write_all_slices(&header, &response.body).await
    }
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
