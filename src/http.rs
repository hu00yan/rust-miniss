//! HTTP request and response handling for the miniss runtime.
//!
//! This module provides high-level HTTP abstractions built on top of the async TCP functionality.
//! It supports basic HTTP/1.1 request parsing and response generation.

use crate::net::AsyncTcpStream;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::str;

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
            Method::Other(method) => write!(f, "{}", method),
        }
    }
}

impl From<&str> for Method {
    fn from(method: &str) -> Self {
        match method.to_uppercase().as_str() {
            "GET" => Method::GET,
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "DELETE" => Method::DELETE,
            "HEAD" => Method::HEAD,
            "OPTIONS" => Method::OPTIONS,
            "PATCH" => Method::PATCH,
            _ => Method::Other(method.to_string()),
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
}

impl fmt::Display for StatusCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.0 {
            200 => "OK",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Unknown",
        };
        write!(f, "{} {}", self.0, reason)
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
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    /// Parse HTTP request from bytes
    pub fn parse(data: &[u8]) -> io::Result<Self> {
        let request_str = str::from_utf8(data)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid UTF-8"))?;

        let mut lines = request_str.lines();

        // Parse request line
        let request_line = lines
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Empty request"))?;

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() != 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid request line",
            ));
        }

        let method = Method::from(parts[0]);
        let path = parts[1].to_string();
        let version = parts[2].to_string();

        // Parse headers
        let mut headers = HashMap::new();
        let mut body_start = None;

        for (i, line) in lines.enumerate() {
            if line.is_empty() {
                body_start = Some(i + 2); // +2 because we consumed the request line
                break;
            }

            if let Some(colon_pos) = line.find(':') {
                let name = line[..colon_pos].trim().to_lowercase();
                let value = line[colon_pos + 1..].trim().to_string();
                headers.insert(name, value);
            }
        }

        // Extract body if present
        let body = if let Some(start) = body_start {
            let body_lines: Vec<&str> = request_str.lines().skip(start).collect();
            body_lines.join("\n").into_bytes()
        } else {
            Vec::new()
        };

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
}

impl Response {
    pub fn new(status: StatusCode) -> Self {
        let mut response = Self {
            status,
            version: "HTTP/1.1".to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        };

        // Add default headers
        response
            .headers
            .insert("server".to_string(), "miniss/1.0".to_string());
        response
    }

    pub fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
        let body_bytes = body.into();
        self.headers
            .insert("content-length".to_string(), body_bytes.len().to_string());
        self.body = body_bytes;
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.to_lowercase(), value.to_string());
        self
    }

    /// Convert response to bytes for sending over TCP
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut response = format!("{} {}\r\n", self.version, self.status);

        for (name, value) in &self.headers {
            response.push_str(&format!("{}: {}\r\n", name, value));
        }

        response.push_str("\r\n");
        let mut bytes = response.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
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
    pub async fn read_request(&self) -> io::Result<Request> {
        let (_, buffer) = self.stream.read().await?;
        Request::parse(buffer.as_ref())
    }

    /// Send HTTP response to the connection
    pub async fn send_response(&self, response: Response) -> io::Result<()> {
        let response_bytes = response.to_bytes();
        self.stream.write_all(&response_bytes).await
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
    content: String,
    content_type: String,
}

impl StaticHandler {
    pub fn new(content: impl Into<String>, content_type: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            content_type: content_type.into(),
        }
    }
}

impl HttpHandler for StaticHandler {
    fn handle(&self, _request: Request) -> impl std::future::Future<Output = Response> + Send {
        let content = self.content.clone();
        let content_type = self.content_type.clone();

        async move {
            Response::new(StatusCode::OK)
                .with_header("content-type", &content_type)
                .with_body(content.as_bytes())
        }
    }
}
