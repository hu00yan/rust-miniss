use crate::cpu::{current_core_id, current_task_id, with_io_state};
use crate::io::IoToken;
use crate::multicore::MultiCoreRuntime;
use crate::timer::SleepFuture;
use libc;
use quinn::udp::{RecvMeta, Transmit, UdpSockRef};
use quinn::{AsyncTimer, AsyncUdpSocket, Runtime as QuinnRuntimeTrait, UdpPoller};
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::UdpSocket;
use std::os::unix::io::AsRawFd;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::Instant;

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
        if current_core_id().is_some() {
            self.runtime
                .spawn_local(future)
                .expect("quinn local background task spawn failed");
        } else {
            self.runtime
                .spawn(future)
                .expect("quinn background task spawn failed");
        }
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
    owner_core: AtomicUsize,
}

const UNOWNED_CORE: usize = usize::MAX;

impl MinissUdpSocket {
    fn new(socket: UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        let state = quinn::udp::UdpSocketState::new((&socket).into())?;
        Ok(Self {
            inner: socket,
            state,
            recv_token: AtomicU64::new(0),
            owner_core: AtomicUsize::new(UNOWNED_CORE),
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
        let _ =
            self.recv_token
                .compare_exchange(token.id(), 0, Ordering::AcqRel, Ordering::Acquire);
    }

    fn ensure_owner(&self) -> io::Result<()> {
        let core = current_core_id()
            .ok_or_else(|| io::Error::other("QUIC I/O requires a miniss worker shard"))?;
        match self.owner_core.compare_exchange(
            UNOWNED_CORE,
            core,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(owner) if owner == core => Ok(()),
            Err(owner) => Err(io::Error::other(format!(
                "QUIC socket owned by shard {owner}, polled from shard {core}"
            ))),
        }
    }
}

impl Drop for MinissUdpSocket {
    fn drop(&mut self) {
        if current_core_id() == Some(self.owner_core.load(Ordering::Acquire)) {
            if let Some(token) = self.load_recv_token() {
                with_io_state(|state| unsafe {
                    state.cancel_and_discard(token);
                });
            }
        }
    }
}

impl AsyncUdpSocket for MinissUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(MinissUdpPoller::new(self))
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.ensure_owner()?;
        self.state.try_send(UdpSockRef::from(&self.inner), transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        if let Err(err) = self.ensure_owner() {
            return Poll::Ready(Err(err));
        }
        with_io_state(|state| {
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
                    let token = match state.io_backend.submit(op) {
                        Ok(token) => token,
                        Err(error) => return Poll::Ready(Err(error.into())),
                    };
                    self.store_recv_token(token);
                    unsafe {
                        state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
                    }
                    state.io_backend.flush();
                    Poll::Pending
                }
                Err(err) => Poll::Ready(Err(err)),
            }
        })
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
    owner_core: AtomicUsize,
}

impl MinissUdpPoller {
    fn new(socket: Arc<MinissUdpSocket>) -> Self {
        Self {
            socket,
            token: AtomicU64::new(0),
            owner_core: AtomicUsize::new(UNOWNED_CORE),
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

    fn ensure_owner(&self) -> io::Result<()> {
        let core = current_core_id()
            .ok_or_else(|| io::Error::other("QUIC I/O requires a miniss worker shard"))?;
        match self.owner_core.compare_exchange(
            UNOWNED_CORE,
            core,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(owner) if owner == core => Ok(()),
            Err(owner) => Err(io::Error::other(format!(
                "QUIC poller owned by shard {owner}, polled from shard {core}"
            ))),
        }
    }
}

impl Drop for MinissUdpPoller {
    fn drop(&mut self) {
        if current_core_id() == Some(self.owner_core.load(Ordering::Acquire)) {
            if let Some(token) = self.load_token() {
                with_io_state(|state| unsafe {
                    state.cancel_and_discard(token);
                });
            }
        }
    }
}

impl UdpPoller for MinissUdpPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(err) = self.socket.ensure_owner() {
            return Poll::Ready(Err(err));
        }
        if let Err(err) = self.ensure_owner() {
            return Poll::Ready(Err(err));
        }
        with_io_state(|state| {
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
            let token = match state.io_backend.submit(op) {
                Ok(token) => token,
                Err(error) => return Poll::Ready(Err(error.into())),
            };
            self.store_token(token);
            unsafe {
                state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
            }
            state.io_backend.flush();
            Poll::Pending
        })
    }
}
