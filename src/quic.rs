use crate::cpu::{current_core_id, current_task_id, with_io_state};
use crate::io::IoToken;
use crate::multicore::{current_core_handle, CoreHandle, MultiCoreRuntime};
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
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Arc, OnceLock,
};
use std::task::{Context, Poll};
#[cfg(target_os = "macos")]
use std::time::Duration;
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
            if let Err(error) = self.runtime.spawn_local(future) {
                // Quinn's Runtime::spawn cannot return an admission result.
                // Runtime shutdown is allowed to race with Quinn dropping a
                // driver, so a rejected background task must not panic here.
                tracing::error!("quinn local background task admission failed: {error}");
            }
        } else if let Err(error) = self.runtime.spawn_detached(future) {
            // Quinn's Runtime::spawn cannot return an admission result.
            // Make bounded backpressure visible and let Quinn observe the
            // resulting driver loss instead of panicking the process.
            tracing::error!("quinn background task admission failed: {error}");
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
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll(cx)
    }
}

impl AsyncTimer for MinissSleep {
    fn reset(self: Pin<&mut Self>, deadline: Instant) {
        // SAFETY: `MinissSleep` does not self-reference; replacing the inner
        // sleep future is safe because callers hold `Pin<&mut Self>`.
        let this = self.get_mut();
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
    retirement_reserved: AtomicBool,
    owner_core: AtomicUsize,
    /// The owning shard's inbox, captured when ownership is claimed - which
    /// happens on that shard. `Drop` needs it when the last reference is
    /// released somewhere else.
    owner_shard: OnceLock<CoreHandle>,
}

const UNOWNED_CORE: usize = usize::MAX;
const QUIC_UDP_BUFFER_SIZE: libc::c_int = 4 * 1024 * 1024;

/// Record the shard that owns `slot` when it is first claimed, so that a later
/// drop from another thread still knows where a pending operation must be
/// retired.
fn capture_shard(slot: &OnceLock<CoreHandle>) {
    if slot.get().is_none() {
        if let Some(handle) = current_core_handle() {
            let _ = slot.set(handle);
        }
    }
}

/// Reserve bounded capacity before submitting each QUIC I/O token. The owner
/// shard's retirement lane can then accept exactly one cross-thread cancel for
/// every admitted token, even when the ordinary inbox is full.
fn reserve_retirement(owner_shard: &OnceLock<CoreHandle>) -> io::Result<CoreHandle> {
    let handle = owner_shard
        .get()
        .cloned()
        .ok_or_else(|| io::Error::other("QUIC owner shard was not captured"))?;
    if !handle.reserve_io_retirement() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "QUIC pending-operation retirement capacity is full",
        ));
    }
    Ok(handle)
}

fn release_retirement(owner_shard: &OnceLock<CoreHandle>, reserved: &AtomicBool) {
    if reserved.swap(false, Ordering::AcqRel) {
        if let Some(handle) = owner_shard.get() {
            handle.release_io_retirement();
        }
    }
}

/// Retire `token` on the shard that owns it. Each successful QUIC operation
/// already owns a capacity reservation, so cross-shard drop never blocks and
/// never competes with ordinary bounded mailbox traffic.
fn retire_token(owner_shard: &OnceLock<CoreHandle>, reserved: &AtomicBool, token: IoToken) {
    if let Some(handle) = owner_shard.get() {
        if reserved.swap(false, Ordering::AcqRel) {
            // A full retirement queue would violate the admission reservation
            // invariant: queued tokens cannot outnumber all outstanding slots.
            assert!(
                handle.retire_io_token(token),
                "reserved QUIC retirement slot was unavailable"
            );
        }
    }
}

impl MinissUdpSocket {
    fn new(socket: UdpSocket) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            // SAFETY: the descriptor belongs to `socket`, and `size` remains
            // valid for the duration of the synchronous setsockopt call.
            let result = unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    option,
                    &QUIC_UDP_BUFFER_SIZE as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&QUIC_UDP_BUFFER_SIZE) as libc::socklen_t,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let state = quinn::udp::UdpSocketState::new((&socket).into())?;
        Ok(Self {
            inner: socket,
            state,
            recv_token: AtomicU64::new(0),
            retirement_reserved: AtomicBool::new(false),
            owner_core: AtomicUsize::new(UNOWNED_CORE),
            owner_shard: OnceLock::new(),
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

    fn clear_recv_token(&self, token: IoToken) -> bool {
        self.recv_token
            .compare_exchange(token.id(), 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
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
            Ok(_) => {}
            Err(owner) if owner == core => {}
            Err(owner) => {
                return Err(io::Error::other(format!(
                    "QUIC socket owned by shard {owner}, polled from shard {core}"
                )))
            }
        }
        capture_shard(&self.owner_shard);
        Ok(())
    }
}

impl Drop for MinissUdpSocket {
    fn drop(&mut self) {
        let owner = self.owner_core.load(Ordering::Acquire);
        let Some(token) = self.load_recv_token() else {
            return;
        };
        if current_core_id() == Some(owner) {
            // SAFETY: the `owner` check puts us on the owning shard, and
            // `token` is this socket's own live recv token.
            with_io_state(|state| unsafe {
                state.cancel_and_discard(token);
            });
            release_retirement(&self.owner_shard, &self.retirement_reserved);
            return;
        }
        retire_token(&self.owner_shard, &self.retirement_reserved, token);
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
                // Fused completions are delivered through the owner completion
                // slot before the task is polled. Consume that slot first;
                // falling back to the shared completion table covers backends
                // which publish completion records there.
                // SAFETY: `state` is this thread's own shard; `token` is this socket's
                // live in-flight token, so the owner-completion slot is ours to take.
                if let Some(result) = unsafe { state.take_owner_completion(token) }
                    // SAFETY: same live shard-local token; this is the shared-table fallback.
                    .or_else(|| unsafe { state.take_completed(token) })
                {
                    if self.clear_recv_token(token) {
                        release_retirement(&self.owner_shard, &self.retirement_reserved);
                    }
                    match result {
                        Ok(_) => {}
                        Err(err) => return Poll::Ready(Err(err.into())),
                    }
                } else {
                    // SAFETY: `store_waker_and_pending_task` needs a live token owned by this
                    // `CpuIoState`; `token` was just read above, on the owning thread.
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
                    let reservation = match reserve_retirement(&self.owner_shard) {
                        Ok(handle) => handle,
                        Err(error) => return Poll::Ready(Err(error)),
                    };
                    let token = match state.io_backend.submit(op) {
                        Ok(token) => token,
                        Err(error) => {
                            reservation.release_io_retirement();
                            return Poll::Ready(Err(error.into()));
                        }
                    };
                    self.retirement_reserved.store(true, Ordering::Release);
                    self.store_recv_token(token);
                    // SAFETY: `token` was just returned by `submit` on this same shard and
                    // has no completion yet, so no slot reuse can race the store.
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
    #[cfg(not(target_os = "macos"))]
    token: AtomicU64,
    #[cfg(not(target_os = "macos"))]
    retirement_reserved: AtomicBool,
    #[cfg(target_os = "macos")]
    retry_timer: Option<MinissSleep>,
    owner_core: AtomicUsize,
    /// The owning shard's inbox; see `MinissUdpSocket::owner_shard`.
    owner_shard: OnceLock<CoreHandle>,
}

impl MinissUdpPoller {
    fn new(socket: Arc<MinissUdpSocket>) -> Self {
        Self {
            socket,
            #[cfg(not(target_os = "macos"))]
            token: AtomicU64::new(0),
            #[cfg(not(target_os = "macos"))]
            retirement_reserved: AtomicBool::new(false),
            #[cfg(target_os = "macos")]
            retry_timer: None,
            owner_core: AtomicUsize::new(UNOWNED_CORE),
            owner_shard: OnceLock::new(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn load_token(&self) -> Option<IoToken> {
        let id = self.token.load(Ordering::Acquire);
        if id == 0 {
            None
        } else {
            Some(IoToken::from_id(id))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn store_token(&self, token: IoToken) {
        self.token.store(token.id(), Ordering::Release);
    }

    #[cfg(not(target_os = "macos"))]
    fn clear_token(&self, token: IoToken) -> bool {
        self.token
            .compare_exchange(token.id(), 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
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
            Ok(_) => {}
            Err(owner) if owner == core => {}
            Err(owner) => {
                return Err(io::Error::other(format!(
                    "QUIC poller owned by shard {owner}, polled from shard {core}"
                )))
            }
        }
        capture_shard(&self.owner_shard);
        Ok(())
    }
}

impl Drop for MinissUdpPoller {
    fn drop(&mut self) {
        #[cfg(not(target_os = "macos"))]
        {
            let owner = self.owner_core.load(Ordering::Acquire);
            let Some(token) = self.load_token() else {
                return;
            };
            if current_core_id() == Some(owner) {
                // SAFETY: the `owner` check puts us on the owning shard, and
                // `token` is this poller's own live token.
                with_io_state(|state| unsafe {
                    state.cancel_and_discard(token);
                });
                release_retirement(&self.owner_shard, &self.retirement_reserved);
                return;
            }
            retire_token(&self.owner_shard, &self.retirement_reserved, token);
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
        // macOS/kqueue does not reliably deliver EVFILT_WRITE readiness for
        // UDP sockets. Datagram sends are normally immediately writable; let
        // Quinn attempt the send and handle an actual WouldBlock itself.
        #[cfg(target_os = "macos")]
        {
            let this = self.get_mut();
            let mut poll_fd = libc::pollfd {
                fd: this.socket.inner.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // UDP write filters are not reliable through mio/kqueue on macOS,
            // but a zero-timeout poll accurately samples send-buffer pressure.
            // SAFETY: `poll_fd` is a live local `libc::pollfd` and the socket owns the descriptor.
            let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
            if ready < 0 {
                return Poll::Ready(Err(io::Error::last_os_error()));
            }
            if (poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL)) != 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "macOS UDP socket reported a write readiness error",
                )));
            }
            if ready > 0 && (poll_fd.revents & libc::POLLOUT) != 0 {
                this.retry_timer = None;
                Poll::Ready(Ok(()))
            } else {
                let timer = this.retry_timer.get_or_insert_with(|| {
                    MinissSleep::new(Instant::now() + Duration::from_millis(1))
                });
                match Future::poll(Pin::new(timer), cx) {
                    Poll::Ready(()) => {
                        this.retry_timer = None;
                        Poll::Ready(Ok(()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            with_io_state(|state| {
                if let Some(token) = self.load_token() {
                    // See poll_recv: io_uring fused completions wake the owner
                    // task with a result that is not stored in the shared table.
                    // SAFETY: `state` is this thread's own shard; `token` is this poller's
                    // live in-flight token, so the owner-completion slot is ours to take.
                    if let Some(result) = unsafe { state.take_owner_completion(token) }
                        // SAFETY: same live shard-local token; this is the shared-table fallback.
                        .or_else(|| unsafe { state.take_completed(token) })
                    {
                        if self.clear_token(token) {
                            release_retirement(&self.owner_shard, &self.retirement_reserved);
                        }
                        return match result {
                            Ok(_) => Poll::Ready(Ok(())),
                            Err(err) => Poll::Ready(Err(err.into())),
                        };
                    }
                    // SAFETY: `token` is this poller's live in-flight token on this shard;
                    // `store_waker_and_pending_task` needs exactly that.
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
                let reservation = match reserve_retirement(&self.owner_shard) {
                    Ok(handle) => handle,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                let token = match state.io_backend.submit(op) {
                    Ok(token) => token,
                    Err(error) => {
                        reservation.release_io_retirement();
                        return Poll::Ready(Err(error.into()));
                    }
                };
                self.retirement_reserved.store(true, Ordering::Release);
                self.store_token(token);
                // SAFETY: `token` was just returned by `submit` on this same shard and
                // has no completion yet, so no slot reuse can race the store.
                unsafe {
                    state.store_waker_and_pending_task(token, cx.waker(), current_task_id());
                }
                state.io_backend.flush();
                Poll::Pending
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multicore::CoreMessage;
    use futures::channel::oneshot;
    use std::time::Duration;

    #[test]
    fn cross_shard_socket_drop_retires_under_full_inbox_pressure() {
        let runtime = MultiCoreRuntime::new(Some(2)).expect("two worker shards");
        let socket = Arc::new(
            MinissUdpSocket::new(UdpSocket::bind("127.0.0.1:0").expect("bind UDP socket"))
                .expect("create miniss UDP socket"),
        );
        let (setup_tx, setup_rx) = std::sync::mpsc::channel();
        runtime
            .spawn_on(0, {
                let socket = Arc::clone(&socket);
                async move {
                    let handle = current_core_handle().expect("owner shard handle");
                    let mut bytes = [0_u8; 128];
                    let mut bufs = [IoSliceMut::new(&mut bytes)];
                    let mut meta = [RecvMeta::default()];
                    let waker = std::task::Waker::noop().clone();
                    let mut cx = Context::from_waker(&waker);
                    let pending = socket.poll_recv(&mut cx, &mut bufs, &mut meta).is_pending();
                    setup_tx.send((handle, pending)).expect("test remains live");
                }
            })
            .expect("submit pending receive setup");
        let (handle, pending) = setup_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("receive setup result");
        assert!(pending, "empty UDP socket completed receive");

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        runtime
            .spawn_on(0, async move {
                std::future::poll_fn(move |_| {
                    let _ = started_tx.send(());
                    release_rx
                        .recv_timeout(Duration::from_secs(1))
                        .expect("release blocked shard task");
                    Poll::Ready(())
                })
                .await;
            })
            .expect("submit shard blocker");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("owner shard did not enter blocker");

        let mut ordinary_inbox_full = false;
        for from_core in 0..5000 {
            if !handle.post(CoreMessage::Ping { from_core }) {
                ordinary_inbox_full = true;
                break;
            }
        }
        drop(socket);
        release_tx.send(()).expect("release owner shard");
        assert!(ordinary_inbox_full, "ordinary inbox did not reach capacity");

        let (retired_tx, retired_rx) = std::sync::mpsc::channel();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut submitted = false;
        while std::time::Instant::now() < deadline {
            let retired_tx = retired_tx.clone();
            if runtime
                .spawn_on(0, async move {
                    let mut pending = usize::MAX;
                    for _ in 0..10 {
                        with_io_state(|state| {
                            state.io_backend.park(Duration::from_millis(10));
                            let _ = state.io_backend.drain_completions();
                            pending = state.io_backend.pending_operation_count();
                        });
                        if pending == 0 {
                            break;
                        }
                    }
                    let _ = retired_tx.send(pending);
                })
                .is_ok()
            {
                submitted = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(submitted, "could not submit bounded retirement check");
        let pending_after = retired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("retirement check timed out");
        let (queued_retirements, reserved_retirements) = handle.io_retirement_counts();
        assert_eq!(
            pending_after, 0,
            "cross-shard drop left the pending QUIC receive registered; queued={queued_retirements}, reserved={reserved_retirements}"
        );

        runtime
            .shutdown_with_timeout(Duration::from_secs(1))
            .expect("shutdown workers");
        runtime.join().expect("join workers");
    }

    #[test]
    fn udp_socket_and_poller_reject_foreign_shards() {
        let runtime = MultiCoreRuntime::new(Some(2)).expect("two worker shards");
        let socket = Arc::new(
            MinissUdpSocket::new(UdpSocket::bind("127.0.0.1:0").expect("bind UDP socket"))
                .expect("create miniss UDP socket"),
        );
        let poller = Arc::new(MinissUdpPoller::new(socket.clone()));

        let (socket_first_tx, socket_first_rx) = oneshot::channel();
        runtime
            .spawn_on(0, {
                let socket = socket.clone();
                async move {
                    let _ = socket_first_tx.send(socket.ensure_owner());
                }
            })
            .expect("admit socket owner task");
        assert!(runtime
            .block_on(socket_first_rx)
            .expect("socket owner task")
            .is_ok());

        let (socket_foreign_tx, socket_foreign_rx) = oneshot::channel();
        runtime
            .spawn_on(1, {
                let socket = socket.clone();
                async move {
                    let _ = socket_foreign_tx.send(socket.ensure_owner());
                }
            })
            .expect("admit foreign socket task");
        let socket_error = runtime
            .block_on(socket_foreign_rx)
            .expect("foreign socket task")
            .expect_err("foreign shard acquired socket ownership");
        assert!(socket_error.to_string().contains("owned by shard 0"));

        let (poller_first_tx, poller_first_rx) = oneshot::channel();
        runtime
            .spawn_on(0, {
                let poller = poller.clone();
                async move {
                    let _ = poller_first_tx.send(poller.ensure_owner());
                }
            })
            .expect("admit poller owner task");
        assert!(runtime
            .block_on(poller_first_rx)
            .expect("poller owner task")
            .is_ok());

        let (poller_foreign_tx, poller_foreign_rx) = oneshot::channel();
        runtime
            .spawn_on(1, async move {
                let _ = poller_foreign_tx.send(poller.ensure_owner());
            })
            .expect("admit foreign poller task");
        let poller_error = runtime
            .block_on(poller_foreign_rx)
            .expect("foreign poller task")
            .expect_err("foreign shard acquired poller ownership");
        assert!(poller_error.to_string().contains("owned by shard 0"));

        runtime
            .shutdown_with_timeout(Duration::from_secs(1))
            .expect("shutdown workers");
    }

    #[test]
    fn dropping_owner_socket_drains_pending_receive() {
        let runtime = MultiCoreRuntime::new(Some(1)).expect("worker shard");
        let socket = Arc::new(
            MinissUdpSocket::new(UdpSocket::bind("127.0.0.1:0").expect("bind UDP socket"))
                .expect("create miniss UDP socket"),
        );
        let (tx, rx) = oneshot::channel();
        runtime
            .spawn_on(0, {
                let socket = socket.clone();
                async move {
                    let mut bytes = [0u8; 128];
                    let mut bufs = [IoSliceMut::new(&mut bytes)];
                    let mut meta = [RecvMeta::default()];
                    let waker = std::task::Waker::noop().clone();
                    let mut cx = Context::from_waker(&waker);
                    let result = socket.poll_recv(&mut cx, &mut bufs, &mut meta);
                    assert!(result.is_pending(), "empty UDP socket completed receive");
                    let pending_before =
                        with_io_state(|state| state.io_backend.pending_operation_count());
                    assert!(
                        pending_before > 0,
                        "receive did not retain a backend operation"
                    );

                    drop(socket);
                    let mut pending_after = pending_before;
                    for _ in 0..10 {
                        with_io_state(|state| {
                            state.io_backend.park(Duration::from_millis(10));
                            let _ = state.io_backend.drain_completions();
                            pending_after = state.io_backend.pending_operation_count();
                        });
                        if pending_after == 0 {
                            break;
                        }
                    }
                    let _ = tx.send((pending_before, pending_after));
                }
            })
            .expect("admit pending receive task");
        drop(socket);

        let (pending_before, pending_after) = runtime.block_on(rx).expect("pending receive task");
        assert!(pending_before > 0);
        assert_eq!(pending_after, 0, "owner drop left a receive operation live");
        runtime
            .shutdown_with_timeout(Duration::from_secs(1))
            .expect("shutdown workers");
    }
}
