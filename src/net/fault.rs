use crate::net::{AsyncTcpStream, AsyncUdpSocket};
use crate::{Buffer, RentBuffer};
use futures::io::{AsyncRead, AsyncWrite};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

/// Synthetic socket fault injected by [`FaultInjector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketFault {
    /// Pass through: normal delivery without fault.
    None,
    /// Delay packet delivery by the specified duration.
    Delay(Duration),
    /// Truncate packet payload to at most `usize` bytes.
    Truncate(usize),
    /// Simulate peer reset by returning `ECONNRESET`.
    Reset,
    /// Simulate timeout cancellation race:
    /// Read/write is raced against a timeout.
    /// Packet delivery is delayed by `delay`.
    /// If `delay > timeout`, the timeout fires first and drops the in-flight future.
    /// If `delay < timeout`, delivery completes before timeout.
    /// If `delay == timeout`, completion and timeout race in the same turn.
    CancelRace { delay: Duration, timeout: Duration },
}

/// Configuration for generating random faults from a PRNG seed.
#[derive(Debug, Clone)]
pub struct FaultConfig {
    pub seed: u64,
    pub delay_prob: f64,
    pub delay_range: (Duration, Duration),
    pub truncate_prob: f64,
    pub truncate_range: (usize, usize),
    pub reset_prob: f64,
    pub cancel_race_prob: f64,
    pub race_delay_range: (Duration, Duration),
    pub race_timeout_range: (Duration, Duration),
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            delay_prob: 0.25,
            delay_range: (Duration::from_millis(1), Duration::from_millis(5)),
            truncate_prob: 0.25,
            truncate_range: (1, 16),
            reset_prob: 0.15,
            cancel_race_prob: 0.35,
            race_delay_range: (Duration::from_millis(1), Duration::from_millis(4)),
            race_timeout_range: (Duration::from_millis(1), Duration::from_millis(3)),
        }
    }
}

/// Metrics tracking injected faults and operation outcomes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FaultStats {
    pub total_faults: usize,
    pub delays: usize,
    pub truncations: usize,
    pub resets: usize,
    pub cancel_races: usize,
    pub completed_ops: usize,
    pub cancelled_ops: usize,
}

/// A deterministic sequence or PRNG-driven generator of [`SocketFault`]s.
#[derive(Debug)]
pub struct FaultSchedule {
    rng: StdRng,
    config: FaultConfig,
    queue: VecDeque<SocketFault>,
    stats: FaultStats,
}

impl FaultSchedule {
    pub fn new(config: FaultConfig) -> Self {
        let rng = StdRng::seed_from_u64(config.seed);
        Self {
            rng,
            config,
            queue: VecDeque::new(),
            stats: FaultStats::default(),
        }
    }

    pub fn from_sequence(faults: impl IntoIterator<Item = SocketFault>) -> Self {
        Self {
            rng: StdRng::seed_from_u64(0),
            config: FaultConfig::default(),
            queue: faults.into_iter().collect(),
            stats: FaultStats::default(),
        }
    }

    /// Build a schedule with `count` randomized cancellation races using a seed.
    pub fn random_cancellation_races(seed: u64, count: usize) -> Self {
        let config = FaultConfig {
            seed,
            ..Default::default()
        };
        let mut rng = StdRng::seed_from_u64(seed);
        let mut queue = VecDeque::with_capacity(count);
        for _ in 0..count {
            let r: f64 = rng.gen();
            let fault = if r < 0.15 {
                let d_ms = rng.gen_range(1..=3);
                SocketFault::Delay(Duration::from_millis(d_ms))
            } else if r < 0.30 {
                let max_b = rng.gen_range(1..=8);
                SocketFault::Truncate(max_b)
            } else if r < 0.40 {
                SocketFault::Reset
            } else {
                let t_ms = rng.gen_range(1..=2);
                let d_ms = rng.gen_range(0..=3);
                SocketFault::CancelRace {
                    delay: Duration::from_millis(d_ms),
                    timeout: Duration::from_millis(t_ms),
                }
            };
            queue.push_back(fault);
        }
        Self {
            rng,
            config,
            queue,
            stats: FaultStats::default(),
        }
    }

    pub fn next_fault(&mut self) -> SocketFault {
        let fault = if let Some(f) = self.queue.pop_front() {
            f
        } else {
            self.generate_fault()
        };
        self.stats.total_faults += 1;
        match &fault {
            SocketFault::None => {}
            SocketFault::Delay(_) => self.stats.delays += 1,
            SocketFault::Truncate(_) => self.stats.truncations += 1,
            SocketFault::Reset => self.stats.resets += 1,
            SocketFault::CancelRace { .. } => self.stats.cancel_races += 1,
        }
        fault
    }

    fn generate_fault(&mut self) -> SocketFault {
        let r: f64 = self.rng.gen();
        let mut cumulative = self.config.delay_prob;
        if r < cumulative {
            let low = self.config.delay_range.0.as_micros() as u64;
            let high = self.config.delay_range.1.as_micros() as u64;
            let micros = if high > low {
                self.rng.gen_range(low..=high)
            } else {
                low
            };
            return SocketFault::Delay(Duration::from_micros(micros));
        }
        cumulative += self.config.truncate_prob;
        if r < cumulative {
            let max_b = self
                .rng
                .gen_range(self.config.truncate_range.0..=self.config.truncate_range.1);
            return SocketFault::Truncate(max_b);
        }
        cumulative += self.config.reset_prob;
        if r < cumulative {
            return SocketFault::Reset;
        }
        cumulative += self.config.cancel_race_prob;
        if r < cumulative {
            let d_low = self.config.race_delay_range.0.as_micros() as u64;
            let d_high = self.config.race_delay_range.1.as_micros() as u64;
            let d_micros = if d_high > d_low {
                self.rng.gen_range(d_low..=d_high)
            } else {
                d_low
            };
            let t_low = self.config.race_timeout_range.0.as_micros() as u64;
            let t_high = self.config.race_timeout_range.1.as_micros() as u64;
            let t_micros = if t_high > t_low {
                self.rng.gen_range(t_low..=t_high)
            } else {
                t_low
            };
            return SocketFault::CancelRace {
                delay: Duration::from_micros(d_micros),
                timeout: Duration::from_micros(t_micros),
            };
        }
        SocketFault::None
    }

    pub fn stats(&self) -> &FaultStats {
        &self.stats
    }

    pub fn record_completed(&mut self) {
        self.stats.completed_ops += 1;
    }

    pub fn record_cancelled(&mut self) {
        self.stats.cancelled_ops += 1;
    }
}

/// Shard-local handle to a shared [`FaultSchedule`].
#[derive(Clone, Debug)]
pub struct FaultInjector(Rc<RefCell<FaultSchedule>>);

impl FaultInjector {
    pub fn new(schedule: FaultSchedule) -> Self {
        Self(Rc::new(RefCell::new(schedule)))
    }

    pub fn from_config(config: FaultConfig) -> Self {
        Self::new(FaultSchedule::new(config))
    }

    /// An injector that performs no faults (pass-through).
    pub fn pass_through() -> Self {
        Self::from_config(FaultConfig {
            delay_prob: 0.0,
            truncate_prob: 0.0,
            reset_prob: 0.0,
            cancel_race_prob: 0.0,
            ..Default::default()
        })
    }

    pub fn from_sequence(faults: impl IntoIterator<Item = SocketFault>) -> Self {
        Self::new(FaultSchedule::from_sequence(faults))
    }

    pub fn random_cancellation_races(seed: u64, count: usize) -> Self {
        Self::new(FaultSchedule::random_cancellation_races(seed, count))
    }

    pub fn next_fault(&self) -> SocketFault {
        self.0.borrow_mut().next_fault()
    }

    pub fn stats(&self) -> FaultStats {
        *self.0.borrow().stats()
    }

    pub fn record_completed(&self) {
        self.0.borrow_mut().record_completed();
    }

    pub fn record_cancelled(&self) {
        self.0.borrow_mut().record_cancelled();
    }
}

/// A wrapper around [`AsyncTcpStream`] that injects synthetic socket faults.
#[derive(Debug)]
pub struct FaultTcpStream {
    inner: AsyncTcpStream,
    injector: FaultInjector,
    delay_timer: Option<crate::timer::SleepFuture>,
    inner_reading: bool,
}

impl FaultTcpStream {
    pub fn new(inner: AsyncTcpStream, injector: FaultInjector) -> Self {
        Self {
            inner,
            injector,
            delay_timer: None,
            inner_reading: false,
        }
    }

    pub fn inner(&self) -> &AsyncTcpStream {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut AsyncTcpStream {
        &mut self.inner
    }

    pub fn into_inner(self) -> AsyncTcpStream {
        self.inner
    }

    pub fn injector(&self) -> &FaultInjector {
        &self.injector
    }

    /// Read into a user-provided owned buffer with fault injection.
    pub async fn read_rent<T: RentBuffer>(&self, buffer: T) -> io::Result<(usize, T)> {
        let fault = self.injector.next_fault();
        match fault {
            SocketFault::None => {
                let res = self.inner.read_rent(buffer).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::Reset => Err(io::Error::from_raw_os_error(libc::ECONNRESET)),
            SocketFault::Truncate(max_len) => {
                let (n, buf) = self.inner.read_rent(buffer).await?;
                let mut vec = Vec::from(buf.into());
                let actual = std::cmp::min(n, max_len);
                vec.truncate(actual);
                self.injector.record_completed();
                Ok((actual, T::from(Buffer::from(vec))))
            }
            SocketFault::Delay(duration) => {
                crate::timer::sleep(duration).await;
                let res = self.inner.read_rent(buffer).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::CancelRace { delay, timeout } => {
                let read_fut = async {
                    if !delay.is_zero() {
                        crate::timer::sleep(delay).await;
                    }
                    self.inner.read_rent(buffer).await
                };
                let timeout_fut = crate::timer::sleep(timeout);
                futures::pin_mut!(read_fut);
                futures::pin_mut!(timeout_fut);
                match futures::future::select(read_fut, timeout_fut).await {
                    futures::future::Either::Left((res, _)) => {
                        if res.is_ok() {
                            self.injector.record_completed();
                        }
                        res
                    }
                    futures::future::Either::Right(((), _read_fut)) => {
                        self.injector.record_cancelled();
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "cancellation race: timeout dropped read_rent future",
                        ))
                    }
                }
            }
        }
    }

    /// Write from a user-provided owned buffer with fault injection.
    pub async fn write_rent<T: RentBuffer>(&self, data: T) -> io::Result<(usize, T)> {
        let fault = self.injector.next_fault();
        match fault {
            SocketFault::None => {
                let res = self.inner.write_rent(data).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::Reset => Err(io::Error::from_raw_os_error(libc::ECONNRESET)),
            SocketFault::Truncate(max_len) => {
                let raw = data.into();
                let to_write = std::cmp::min(raw.len(), max_len);
                let part = Buffer::from_slice(&raw[..to_write]);
                let (n, _) = self.inner.write_rent(part).await?;
                self.injector.record_completed();
                Ok((n, T::from(raw)))
            }
            SocketFault::Delay(duration) => {
                crate::timer::sleep(duration).await;
                let res = self.inner.write_rent(data).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::CancelRace { timeout, .. } => {
                let write_fut = self.inner.write_rent(data);
                let timeout_fut = crate::timer::sleep(timeout);
                futures::pin_mut!(write_fut);
                futures::pin_mut!(timeout_fut);
                match futures::future::select(write_fut, timeout_fut).await {
                    futures::future::Either::Left((res, _)) => {
                        if res.is_ok() {
                            self.injector.record_completed();
                        }
                        res
                    }
                    futures::future::Either::Right(((), _write_fut)) => {
                        self.injector.record_cancelled();
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "cancellation race: timeout dropped write_rent future",
                        ))
                    }
                }
            }
        }
    }

    /// Write a static byte slice through the inner stream.
    pub async fn write_all_static(&self, data: &'static [u8]) -> io::Result<()> {
        self.inner.write_all_static(data).await
    }
}

impl AsRawFd for FaultTcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsyncRead for FaultTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(mut timer) = this.delay_timer.take() {
            match Pin::new(&mut timer).poll(cx) {
                Poll::Pending => {
                    this.delay_timer = Some(timer);
                    return Poll::Pending;
                }
                Poll::Ready(()) => {
                    let res = Pin::new(&mut this.inner).poll_read(cx, buf);
                    this.inner_reading = res.is_pending();
                    if res.is_ready() {
                        this.injector.record_completed();
                    }
                    return res;
                }
            }
        }
        if !this.inner_reading {
            let fault = this.injector.next_fault();
            match fault {
                SocketFault::None => {}
                SocketFault::Reset => {
                    return Poll::Ready(Err(io::Error::from_raw_os_error(libc::ECONNRESET)));
                }
                SocketFault::Truncate(max_len) => {
                    let target_len = std::cmp::min(buf.len(), max_len);
                    let res = Pin::new(&mut this.inner).poll_read(cx, &mut buf[..target_len]);
                    this.inner_reading = res.is_pending();
                    if res.is_ready() {
                        this.injector.record_completed();
                    }
                    return res;
                }
                SocketFault::Delay(duration) => {
                    let mut timer = crate::timer::SleepFuture::new(duration);
                    match Pin::new(&mut timer).poll(cx) {
                        Poll::Pending => {
                            this.delay_timer = Some(timer);
                            return Poll::Pending;
                        }
                        Poll::Ready(()) => {
                            let res = Pin::new(&mut this.inner).poll_read(cx, buf);
                            this.inner_reading = res.is_pending();
                            if res.is_ready() {
                                this.injector.record_completed();
                            }
                            return res;
                        }
                    }
                }
                SocketFault::CancelRace { delay, .. } => {
                    let mut timer = crate::timer::SleepFuture::new(delay);
                    match Pin::new(&mut timer).poll(cx) {
                        Poll::Pending => {
                            this.delay_timer = Some(timer);
                            return Poll::Pending;
                        }
                        Poll::Ready(()) => {
                            let res = Pin::new(&mut this.inner).poll_read(cx, buf);
                            this.inner_reading = res.is_pending();
                            if res.is_ready() {
                                this.injector.record_completed();
                            }
                            return res;
                        }
                    }
                }
            }
        }

        let res = Pin::new(&mut this.inner).poll_read(cx, buf);
        this.inner_reading = res.is_pending();
        if res.is_ready() {
            this.injector.record_completed();
        }
        res
    }

    fn poll_read_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(mut timer) = this.delay_timer.take() {
            match Pin::new(&mut timer).poll(cx) {
                Poll::Pending => {
                    this.delay_timer = Some(timer);
                    return Poll::Pending;
                }
                Poll::Ready(()) => {
                    let res = Pin::new(&mut this.inner).poll_read_vectored(cx, bufs);
                    this.inner_reading = res.is_pending();
                    if res.is_ready() {
                        this.injector.record_completed();
                    }
                    return res;
                }
            }
        }
        let res = Pin::new(&mut this.inner).poll_read_vectored(cx, bufs);
        this.inner_reading = res.is_pending();
        if res.is_ready() {
            this.injector.record_completed();
        }
        res
    }
}

impl AsyncWrite for FaultTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_close(cx)
    }
}

impl hyper::rt::Read for FaultTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        // SAFETY: `ReadBufCursor` exposes the unwritten tail of hyper's
        // buffer as a writable chunk for the duration of this call.
        let raw = unsafe { buf.as_mut() };
        let slice =
            // SAFETY: same region, `raw.len()` is its exact capacity.
            unsafe { std::slice::from_raw_parts_mut(raw.as_mut_ptr().cast::<u8>(), raw.len()) };

        match AsyncRead::poll_read(self.as_mut(), cx, slice) {
            Poll::Ready(Ok(bytes_read)) => {
                // SAFETY: the underlying AsyncRead initialized exactly `bytes_read` bytes.
                unsafe {
                    buf.advance(bytes_read);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl hyper::rt::Write for FaultTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        hyper::rt::Write::poll_write(Pin::new(&mut this.inner), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        hyper::rt::Write::poll_flush(Pin::new(&mut this.inner), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        hyper::rt::Write::poll_shutdown(Pin::new(&mut this.inner), cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        hyper::rt::Write::poll_write_vectored(Pin::new(&mut this.inner), cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// A wrapper around [`AsyncUdpSocket`] that injects synthetic socket faults.
pub struct FaultUdpSocket {
    inner: AsyncUdpSocket,
    injector: FaultInjector,
}

impl FaultUdpSocket {
    pub fn new(inner: AsyncUdpSocket, injector: FaultInjector) -> Self {
        Self { inner, injector }
    }

    pub fn inner(&self) -> &AsyncUdpSocket {
        &self.inner
    }

    pub fn into_inner(self) -> AsyncUdpSocket {
        self.inner
    }

    pub fn injector(&self) -> &FaultInjector {
        &self.injector
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let fault = self.injector.next_fault();
        match fault {
            SocketFault::None => {
                let res = self.inner.recv_from(buf).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::Reset => Err(io::Error::from_raw_os_error(libc::ECONNRESET)),
            SocketFault::Truncate(max_len) => {
                let target = std::cmp::min(buf.len(), max_len);
                let (n, addr) = self.inner.recv_from(&mut buf[..target]).await?;
                self.injector.record_completed();
                Ok((n, addr))
            }
            SocketFault::Delay(duration) => {
                crate::timer::sleep(duration).await;
                let res = self.inner.recv_from(buf).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::CancelRace { delay, timeout } => {
                let recv_fut = async {
                    if !delay.is_zero() {
                        crate::timer::sleep(delay).await;
                    }
                    self.inner.recv_from(buf).await
                };
                let timeout_fut = crate::timer::sleep(timeout);
                futures::pin_mut!(recv_fut);
                futures::pin_mut!(timeout_fut);
                match futures::future::select(recv_fut, timeout_fut).await {
                    futures::future::Either::Left((res, _)) => {
                        if res.is_ok() {
                            self.injector.record_completed();
                        }
                        res
                    }
                    futures::future::Either::Right(((), _recv_fut)) => {
                        self.injector.record_cancelled();
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "cancellation race: timeout dropped recv_from future",
                        ))
                    }
                }
            }
        }
    }

    pub async fn recv_from_rent<T: RentBuffer>(
        &self,
        buffer: T,
    ) -> io::Result<(usize, SocketAddr, T)> {
        let fault = self.injector.next_fault();
        match fault {
            SocketFault::None => {
                let res = self.inner.recv_from_rent(buffer).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::Reset => Err(io::Error::from_raw_os_error(libc::ECONNRESET)),
            SocketFault::Truncate(max_len) => {
                let (n, addr, buf) = self.inner.recv_from_rent(buffer).await?;
                let mut vec = Vec::from(buf.into());
                let actual = std::cmp::min(n, max_len);
                vec.truncate(actual);
                self.injector.record_completed();
                Ok((actual, addr, T::from(Buffer::from(vec))))
            }
            SocketFault::Delay(duration) => {
                crate::timer::sleep(duration).await;
                let res = self.inner.recv_from_rent(buffer).await;
                if res.is_ok() {
                    self.injector.record_completed();
                }
                res
            }
            SocketFault::CancelRace { delay, timeout } => {
                let recv_fut = async {
                    if !delay.is_zero() {
                        crate::timer::sleep(delay).await;
                    }
                    self.inner.recv_from_rent(buffer).await
                };
                let timeout_fut = crate::timer::sleep(timeout);
                futures::pin_mut!(recv_fut);
                futures::pin_mut!(timeout_fut);
                match futures::future::select(recv_fut, timeout_fut).await {
                    futures::future::Either::Left((res, _)) => {
                        if res.is_ok() {
                            self.injector.record_completed();
                        }
                        res
                    }
                    futures::future::Either::Right(((), _recv_fut)) => {
                        self.injector.record_cancelled();
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "cancellation race: timeout dropped recv_from_rent future",
                        ))
                    }
                }
            }
        }
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.inner.send_to(buf, target).await
    }

    pub async fn send_to_rent<T: RentBuffer>(
        &self,
        data: T,
        target: SocketAddr,
    ) -> io::Result<(usize, T)> {
        self.inner.send_to_rent(data, target).await
    }
}

impl AsRawFd for FaultUdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

/// Create a connected pair of [`FaultTcpStream`]s on loopback.
pub fn fault_tcp_pair(
    client_injector: FaultInjector,
    server_injector: FaultInjector,
) -> io::Result<(FaultTcpStream, FaultTcpStream)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let client = std::net::TcpStream::connect(addr)?;
    let (server, _) = listener.accept()?;
    client.set_nonblocking(true)?;
    server.set_nonblocking(true)?;
    let client_stream = AsyncTcpStream::new(client)?;
    let server_stream = AsyncTcpStream::new(server)?;
    Ok((
        FaultTcpStream::new(client_stream, client_injector),
        FaultTcpStream::new(server_stream, server_injector),
    ))
}

/// Create a pair of bound [`FaultUdpSocket`]s on loopback.
pub fn fault_udp_pair(
    client_injector: FaultInjector,
    server_injector: FaultInjector,
) -> io::Result<(FaultUdpSocket, FaultUdpSocket)> {
    let client = AsyncUdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    let server = AsyncUdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
    Ok((
        FaultUdpSocket::new(client, client_injector),
        FaultUdpSocket::new(server, server_injector),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::rt::ReadBuf;
    use std::mem::MaybeUninit;

    #[test]
    fn fault_tcp_stream_hyper_read_intercepts_reset_fault() {
        let runtime = crate::Runtime::new();
        runtime.block_on(async {
            let client_injector = FaultInjector::from_sequence([SocketFault::Reset]);
            let server_injector = FaultInjector::pass_through();
            let (mut client, _server) = fault_tcp_pair(client_injector, server_injector).unwrap();

            let mut storage = [MaybeUninit::uninit(); 128];
            let mut buf = ReadBuf::uninit(&mut storage);
            let cursor = buf.unfilled();
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);

            // Must intercept reset fault and return ECONNRESET
            let poll_res = hyper::rt::Read::poll_read(Pin::new(&mut client), &mut cx, cursor);
            match poll_res {
                Poll::Ready(Err(err)) => {
                    assert_eq!(err.raw_os_error(), Some(libc::ECONNRESET));
                }
                other => panic!("expected ECONNRESET, got {:?}", other),
            }
        });
    }
}
