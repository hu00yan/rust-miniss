//! Compatibility layer between `futures::io` and `tokio::io` traits.
//!
//! Re-exports [`tokio_util::compat`] so users can convert any
//! `futures::io::AsyncRead`/`AsyncWrite` type into a `tokio::io`
//! compatible wrapper for use with the tokio ecosystem
//! (hyper, tower, reqwest, etc.).
//!
//! # Example
//!
//! ```rust,no_run
//! use rust_miniss::net::AsyncTcpStream;
//! use rust_miniss::compat::{FuturesAsyncReadCompatExt, Compat};
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! # async fn example(stream: AsyncTcpStream) {
//! let mut compat = stream.compat();
//! let mut buf = [0u8; 1024];
//! compat.read(&mut buf).await.unwrap();
//! # }
//! ```

pub use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, FuturesAsyncWriteCompatExt};
