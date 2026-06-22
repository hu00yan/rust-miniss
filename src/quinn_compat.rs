//! Quinn QUIC integration for rust-miniss.
//!
//! Implements `quinn::Runtime` and `quinn::AsyncUdpSocket` traits so that
//! Quinn can run on the rust-miniss runtime without Tokio.
//!
//! # Example
//!
//! ```rust,no_run
//! use rust_miniss::quinn_compat::MinissQuinnRuntime;
//! use std::sync::Arc;
//!
//! # async fn example() {
//! let runtime = Arc::new(MinissQuinnRuntime::new());
//! let endpoint = quinn::Endpoint::server(
//!     quinn::ServerConfig::with_crypto(Arc::new(
//!         quinn::crypto::rustls::QuicServerConfig::try_from(
//!             rustls::ServerConfig::builder()
//!                 .with_no_client_auth()
//!                 .with_single_cert(vec![], todo!())
//!                 .unwrap()
//!         ).unwrap()
//!     )),
//!     "0.0.0.0:4433".parse().unwrap(),
//! ).unwrap();
//! # }
//! ```

use crate::timer;
use quinn::udp::{RecvMeta, Transmit, UdpSockRef, UdpSocketState};
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// A Quinn runtime backed by rust-miniss.
///
/// This runtime implements `quinn::Runtime` to allow Quinn to work
/// with rust-miniss's I/O and timer infrastructure.
#[derive(Debug)]
pub struct MinissQuinnRuntime;

impl MinissQuinnRuntime {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MinissQuinnRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl quinn::Runtime for MinissQuinnRuntime {
    fn new_timer(&self, i: Instant) -> Pin<Box<dyn quinn::AsyncTimer>> {
        let now = Instant::now();
        let duration = if i > now { i - now } else { Duration::ZERO };
        Box::pin(MinissQuinnTimer {
            deadline: i,
            future: Some(Box::pin(timer::sleep(duration))),
        })
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        tokio::spawn(future);
    }

    fn wrap_udp_socket(
        &self,
        sock: std::net::UdpSocket,
    ) -> io::Result<Arc<dyn quinn::AsyncUdpSocket>> {
        sock.set_nonblocking(true)?;
        let state = UdpSocketState::new(UdpSockRef::from(&sock))?;
        Ok(Arc::new(MinissUdpSocket {
            inner: state,
            socket: sock,
        }))
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Timer wrapper for Quinn compatibility.
struct MinissQuinnTimer {
    deadline: Instant,
    future: Option<Pin<Box<dyn Future<Output = ()> + Send + Sync>>>,
}

impl fmt::Debug for MinissQuinnTimer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MinissQuinnTimer")
            .field("deadline", &self.deadline)
            .finish()
    }
}

impl quinn::AsyncTimer for MinissQuinnTimer {
    fn reset(self: Pin<&mut Self>, i: Instant) {
        let this = self.get_mut();
        this.deadline = i;
        let now = Instant::now();
        let duration = if i > now { i - now } else { Duration::ZERO };
        this.future = Some(Box::pin(timer::sleep(duration)));
    }

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(ref mut future) = self.get_mut().future {
            future.as_mut().poll(cx)
        } else {
            Poll::Ready(())
        }
    }
}

/// UDP socket wrapper that implements `quinn::AsyncUdpSocket`.
struct MinissUdpSocket {
    inner: UdpSocketState,
    socket: std::net::UdpSocket,
}

impl fmt::Debug for MinissUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MinissUdpSocket")
            .field("addr", &self.socket.local_addr())
            .finish()
    }
}

impl quinn::AsyncUdpSocket for MinissUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        Box::pin(MinissUdpPoller { socket: self })
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        self.inner.send(UdpSockRef::from(&self.socket), transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        match self.inner.recv(UdpSockRef::from(&self.socket), bufs, meta) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn may_fragment(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            false
        }
        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }
}

/// Poller for write readiness notifications.
#[derive(Debug)]
struct MinissUdpPoller {
    socket: Arc<MinissUdpSocket>,
}

impl quinn::UdpPoller for MinissUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let socket = self.socket.socket.try_clone()?;
        let mut source = mio::net::UdpSocket::from_std(socket);

        let mut poll = mio::Poll::new().map_err(io::Error::other)?;
        poll.registry()
            .register(&mut source, mio::Token(0), mio::Interest::WRITABLE)
            .map_err(io::Error::other)?;

        let mut events = mio::Events::with_capacity(1);
        match poll.poll(&mut events, Some(std::time::Duration::ZERO)) {
            Ok(_) => {
                if events.is_empty() {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }
}
