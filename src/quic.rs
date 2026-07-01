use crate::cpu::{current_task_id, io_state};
use crate::io::IoToken;
use crate::multicore::MultiCoreRuntime;
use crate::timer::SleepFuture;
use quinn::udp::{RecvMeta, Transmit, UdpSockRef};
use quinn::{AsyncTimer, AsyncUdpSocket, Runtime as QuinnRuntimeTrait, UdpPoller};
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::UdpSocket;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::os::unix::io::AsRawFd;
use std::task::{Context, Poll};
use std::time::Instant;
use libc;

#[derive(Clone)]
pub struct MinissQuicRuntime {
    runtime: Arc<MultiCoreRuntime>,
}

impl MinissQuicRuntime {
    pub fn new(runtime: Arc<MultiCoreRuntime>) -> Self {
        Self { runtime }
    }

    pub fn runtime(&self) -> &Arc<MultiCoreRuntime> {
        &self.runtime
    }
}

impl fmt::Debug for MinissQuicRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MinissQuicRuntime").finish_non_exhaustive()
    }
}

impl QuinnRuntimeTrait for MinissQuicRuntime {
    fn new_timer(&self, i: Instant) -> Pin<Box<dyn AsyncTimer>> {
        Box::pin(MinissSleep::new(i))
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.runtime
            .spawn(future)
            .expect("quinn background task spawn failed");
    }

    fn wrap_udp_socket(&self, socket: UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Ok(Arc::new(MinissUdpSocket::new(socket)?))
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug)]
struct MinissSleep {
    inner: SleepFuture,
}

impl MinissSleep {
    fn new(deadline: Instant) -> Self {
        Self {
            inner: SleepFuture::new(deadline.saturating_duration_since(Instant::now())),
        }
    }
}

impl Future for MinissSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `MinissSleep` only contains an inner future; replacing the
        // field is handled in `reset` and does not rely on self-references.
        let this = unsafe { self.get_unchecked_mut() };
        Pin::new(&mut this.inner).poll(cx)
    }
}

impl AsyncTimer for MinissSleep {
    fn reset(self: Pin<&mut Self>, deadline: Instant) {
        // SAFETY: `MinissSleep` does not self-reference; replacing the inner
        // sleep future is safe because callers hold `Pin<&mut Self>`.
        let this = unsafe { self.get_unchecked_mut() };
        this.inner = SleepFuture::new(deadline.saturating_duration_since(Instant::now()));
    }

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Future::poll(self, cx)
    }
}

#[derive(Debug)]
pub struct MinissUdpSocket {
    inner: UdpSocket,
    state: quinn::udp::UdpSocketState,
    recv_token: AtomicU64,
}

impl MinissUdpSocket {
    fn new(socket: UdpSocket) -> io::Result<Self> {
        let state = quinn::udp::UdpSocketState::new((&socket).into())?;
        Ok(Self {
            inner: socket,
            state,
            recv_token: AtomicU64::new(0),
        })
    }

    fn load_recv_token(&self) -> Option<IoToken> {
        let id = self.recv_token.load(Ordering::Acquire);
        if id == 0 {
            None
        } else {
            Some(IoToken::from_id(id))
        }
    }

    fn store_recv_token(&self, token: IoToken) {
        self.recv_token.store(token.id(), Ordering::Release);
    }

    fn clear_recv_token(&self, token: IoToken) {
        let _ = self
            .recv_token
            .compare_exchange(token.id(), 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

impl Drop for MinissUdpSocket {
    fn drop(&mut self) {
        if let Some(token) = self.load_recv_token() {
            let state = io_state();
            unsafe {
                state.clear_slot(token);
            }
        }
    }
}

impl AsyncUdpSocket for MinissUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(MinissUdpPoller::new(self))
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.state.try_send(UdpSockRef::from(&self.inner), transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let state = io_state();
        if let Some(token) = self.load_recv_token() {
            if let Some(result) = unsafe { state.take_completed(token) } {
                self.clear_recv_token(token);
                match result {
                    Ok(_) => {}
                    Err(err) => return Poll::Ready(Err(err.into())),
                }
            } else {
                unsafe {
                    state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
                }
                return Poll::Pending;
            }
        }

        match self.state.recv(UdpSockRef::from(&self.inner), bufs, meta) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let op = crate::io::Op::PollAdd {
                    fd: self.inner.as_raw_fd(),
                    events: libc::POLLIN as u32,
                    multishot: false,
                };
                let token = state.io_backend.submit(op);
                self.store_recv_token(token);
                unsafe {
                    state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
                }
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.state.max_gso_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.state.gro_segments()
    }

    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
}

#[derive(Debug)]
struct MinissUdpPoller {
    socket: Arc<MinissUdpSocket>,
    token: AtomicU64,
}

impl MinissUdpPoller {
    fn new(socket: Arc<MinissUdpSocket>) -> Self {
        Self {
            socket,
            token: AtomicU64::new(0),
        }
    }

    fn load_token(&self) -> Option<IoToken> {
        let id = self.token.load(Ordering::Acquire);
        if id == 0 {
            None
        } else {
            Some(IoToken::from_id(id))
        }
    }

    fn store_token(&self, token: IoToken) {
        self.token.store(token.id(), Ordering::Release);
    }

    fn clear_token(&self, token: IoToken) {
        let _ = self
            .token
            .compare_exchange(token.id(), 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

impl Drop for MinissUdpPoller {
    fn drop(&mut self) {
        if let Some(token) = self.load_token() {
            let state = io_state();
            unsafe {
                state.clear_slot(token);
            }
        }
    }
}

impl UdpPoller for MinissUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let state = io_state();
        if let Some(token) = self.load_token() {
            if let Some(result) = unsafe { state.take_completed(token) } {
                self.clear_token(token);
                return match result {
                    Ok(_) => Poll::Ready(Ok(())),
                    Err(err) => Poll::Ready(Err(err.into())),
                };
            }
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            return Poll::Pending;
        }

        let op = crate::io::Op::PollAdd {
            fd: self.socket.inner.as_raw_fd(),
            events: libc::POLLOUT as u32,
            multishot: false,
        };
        let token = state.io_backend.submit(op);
        self.store_token(token);
        unsafe {
            state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
        }
        Poll::Pending
    }
}
