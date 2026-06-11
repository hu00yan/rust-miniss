//! kqueue backend implementation reusing the mio-based epoll backend.
//!
//! The epoll backend in this crate is implemented entirely in terms of `mio`'s
//! cross-platform polling API. On macOS `mio` automatically maps to kqueue,
//! so we can safely reuse the same implementation without duplicating the code.

pub use crate::io::epoll::EpollBackend as KqueueBackend;
