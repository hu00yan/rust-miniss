//! Deterministic in-memory QUIC network for the service-lifecycle property
//! core (methodology §T4 sim-flavor, §6). Contract: one thread; one world
//! lock held only across leaf state access, never across a future poll or a
//! waker call; the only time source is the virtual clock advanced inside
//! `run_until`; packet outcomes come from one `StdRng` in per-packet
//! submission order — same seed + schedule = identical world. quinn plugs in
//! only through `AsyncUdpSocket` + `Runtime` (`Endpoint::new_with_abstract_
//! socket`), the same traits the production `MinissQuicRuntime` implements.
//! Fault knobs are exactly the generator-consumed set (§6): loss / reorder /
//! dup / delay / partition / mtu.

use parking_lot::Mutex;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncTimer, AsyncUdpSocket, Runtime as QuinnRuntime, UdpPoller};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// Step bound per `run_until`: a runaway schedule stops here, not hangs.
const MAX_STEPS: u32 = 200_000;

/// Generator-consumed fault knobs (methodology §6). `partition` entries are
/// `(at_step, from_port, to_port)` and match in either direction — the typed
/// form of the methodology sketch's `(usize, usize)` pair.
#[derive(Clone, Debug)]
pub struct SimFaultSchedule {
    pub seed: u64,
    pub loss: f64,
    pub reorder: Option<Duration>,
    pub dup: f64,
    pub delay: (Duration, Duration),
    pub partition: Vec<(u64, u16, u16)>,
    pub mtu: Option<usize>,
}

impl Default for SimFaultSchedule {
    fn default() -> Self {
        Self {
            seed: 0,
            loss: 0.0,
            reorder: None,
            dup: 0.0,
            delay: (Duration::from_millis(1), Duration::from_millis(0)),
            partition: Vec::new(),
            mtu: None,
        }
    }
}

/// Virtual-world counters; the oracle-facing projection of a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimCounters {
    pub packets_submitted: u64,
    pub packets_delivered: u64,
    pub duplicates_emitted: u64,
    pub dropped_loss: u64,
    pub dropped_mtu: u64,
    pub dropped_partition: u64,
    pub dropped_unreachable: u64,
    pub tasks_spawned: u64,
    pub tasks_completed: u64,
    /// Peak bytes unread across all sim receive queues.
    pub peak_unread_socket_bytes: usize,
}

/// Replayable trace; equality of two traces proves identical worlds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    Delivered {
        seq: u64,
        at_us: u64,
        from: u16,
        to: u16,
        len: usize,
    },
    Dropped {
        seq: u64,
        reason: &'static str,
    },
}

/// Executor outcome for one `run_until` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SimRunReport {
    pub steps: u32,
    pub quiescent: bool,
    /// Tasks remain but no timer or delivery can ever wake them.
    pub stuck: bool,
    /// `MAX_STEPS` reached (runaway retransmission loop).
    pub exhausted: bool,
    pub timed_out: bool,
}

type Packet = (u16, u16, Vec<u8>);
/// A task's future after its poll round: `None` when the task completed.
type ResumedTask = (usize, Option<Pin<Box<dyn Future<Output = ()> + Send>>>);

struct SocketState {
    queue: VecDeque<(SocketAddr, Vec<u8>)>,
    recv_waker: Option<Waker>,
}

struct TimerEntry {
    deadline: std::time::Instant,
    waker: Option<Waker>,
}

struct World {
    now: std::time::Instant,
    anchor: std::time::Instant,
    rng: StdRng,
    faults: SimFaultSchedule,
    step: u64,
    seq: u64,
    next_port: u16,
    sockets: BTreeMap<u16, SocketState>,
    timers: BTreeMap<u64, TimerEntry>,
    next_timer_id: u64,
    deliveries: BTreeMap<(std::time::Instant, u64), Packet>,
    /// `tasks[id]`; `None` once completed or mid-poll. Never compacted, so
    /// ascending-id poll order is part of the determinism contract.
    tasks: Vec<Option<Pin<Box<dyn Future<Output = ()> + Send>>>>,
    /// `wake_board[id]` schedules task `id` for its next poll — the single
    /// wake channel, so a wake landing mid-poll still sticks.
    wake_board: Vec<bool>,
    spawn_queue: Vec<Pin<Box<dyn Future<Output = ()> + Send>>>,
    trace: Vec<TraceEvent>,
    counters: SimCounters,
}

impl World {
    fn partitioned(&self, from: u16, to: u16) -> bool {
        self.faults.partition.iter().any(|&(at, a, b)| {
            self.step >= at && ((from == a && to == b) || (from == b && to == a))
        })
    }

    /// Apply the fault schedule to one datagram and schedule delivery. RNG
    /// draw order per packet: loss, dup, delay jitter, reorder jitter.
    fn submit(&mut self, from: u16, to: u16, contents: &[u8]) {
        self.seq += 1;
        let seq = self.seq;
        self.counters.packets_submitted += 1;
        let drop = |world: &mut Self, reason: &'static str| {
            world.trace.push(TraceEvent::Dropped { seq, reason });
        };
        if let Some(mtu) = self.faults.mtu {
            if contents.len() > mtu {
                self.counters.dropped_mtu += 1;
                drop(self, "mtu");
                return;
            }
        }
        if self.partitioned(from, to) {
            self.counters.dropped_partition += 1;
            drop(self, "partition");
            return;
        }
        if !self.rng.gen_bool(1.0 - self.faults.loss.clamp(0.0, 1.0)) {
            self.counters.dropped_loss += 1;
            drop(self, "loss");
            return;
        }
        let duplicate = self.rng.gen_bool(self.faults.dup.clamp(0.0, 1.0));
        let jitter = self
            .rng
            .gen_range(0..=self.faults.delay.1.as_micros() as u64);
        let reorder = match self.faults.reorder {
            Some(r) => self.rng.gen_range(0..=r.as_micros() as u64),
            None => 0,
        };
        let at = self.now + self.faults.delay.0 + Duration::from_micros(jitter + reorder);
        let packet: Packet = (from, to, contents.to_vec());
        self.deliveries.insert((at, seq), packet.clone());
        if duplicate {
            self.counters.duplicates_emitted += 1;
            self.seq += 1;
            self.deliveries.insert((at, self.seq), packet);
        }
    }

    fn total_queued_bytes(&self) -> usize {
        self.sockets
            .values()
            .map(|s| s.queue.iter().map(|(_, d)| d.len()).sum::<usize>())
            .sum()
    }

    /// Deliver due datagrams and fire due timers, collecting wakers to wake
    /// outside the lock.
    fn take_due_events(&mut self) -> Vec<Waker> {
        let mut wakers = Vec::new();
        let now = self.now;
        let mut due = Vec::new();
        for (at, seq) in self.deliveries.keys() {
            if *at > now {
                break;
            }
            due.push((*at, *seq));
        }
        for (at, seq) in due {
            let Some((from, to, data)) = self.deliveries.remove(&(at, seq)) else {
                continue;
            };
            self.counters.packets_delivered += 1;
            self.trace.push(TraceEvent::Delivered {
                seq,
                at_us: now.duration_since(self.anchor).as_micros() as u64,
                from,
                to,
                len: data.len(),
            });
            if let Some(socket) = self.sockets.get_mut(&to) {
                socket.queue.push_back((
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), from),
                    data,
                ));
                if let Some(waker) = socket.recv_waker.take() {
                    wakers.push(waker);
                }
            } else {
                self.counters.dropped_unreachable += 1;
            }
            let queued = self.total_queued_bytes();
            self.counters.peak_unread_socket_bytes =
                self.counters.peak_unread_socket_bytes.max(queued);
        }
        let expired: Vec<u64> = self
            .timers
            .iter()
            .filter(|(_, e)| e.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(entry) = self.timers.remove(&id) {
                if let Some(waker) = entry.waker {
                    wakers.push(waker);
                }
            }
        }
        wakers
    }

    fn next_event(&self) -> Option<std::time::Instant> {
        let timer = self.timers.values().map(|e| e.deadline).min();
        let delivery = self.deliveries.keys().next().map(|(at, _)| *at);
        match (timer, delivery) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn spawn_task(&mut self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.tasks.push(Some(future));
        self.wake_board.push(true);
        self.counters.tasks_spawned += 1;
    }
}

/// Shared virtual world. `parking_lot::Mutex` (never behind `Arc`, never
/// held across a poll or a waker call) provides the `Send + Sync` bounds
/// quinn's traits demand; nothing here runs in any production path.
pub struct SimWorld {
    inner: Mutex<World>,
}

impl fmt::Debug for SimWorld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimWorld").finish_non_exhaustive()
    }
}

struct TaskWaker {
    id: usize,
    world: Arc<SimWorld>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let mut world = self.world.inner.lock();
        if self.id < world.wake_board.len() {
            world.wake_board[self.id] = true;
        }
    }
}

/// Handle to the virtual world: bind sockets, spawn tasks, run time.
#[derive(Clone)]
pub struct SimNetwork {
    world: Arc<SimWorld>,
}

impl fmt::Debug for SimNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimNetwork").finish_non_exhaustive()
    }
}

impl SimNetwork {
    pub fn new(faults: SimFaultSchedule) -> Self {
        let anchor = std::time::Instant::now();
        let seed = faults.seed;
        Self {
            world: Arc::new(SimWorld {
                inner: Mutex::new(World {
                    now: anchor,
                    anchor,
                    rng: StdRng::seed_from_u64(seed),
                    faults,
                    step: 0,
                    seq: 0,
                    next_port: 1001,
                    sockets: BTreeMap::new(),
                    timers: BTreeMap::new(),
                    next_timer_id: 1,
                    deliveries: BTreeMap::new(),
                    tasks: Vec::new(),
                    wake_board: Vec::new(),
                    spawn_queue: Vec::new(),
                    trace: Vec::new(),
                    counters: SimCounters::default(),
                }),
            }),
        }
    }

    /// The only clock the sim world exposes.
    pub fn now(&self) -> std::time::Instant {
        self.world.inner.lock().now
    }

    /// The quinn `Runtime` over this network.
    pub fn runtime(&self) -> Arc<dyn QuinnRuntime> {
        Arc::new(SimRuntime {
            world: Arc::clone(&self.world),
        })
    }

    /// Bind a fresh sim socket (one node of the virtual network).
    pub fn bind(&self) -> (Arc<dyn AsyncUdpSocket>, SocketAddr) {
        let mut world = self.world.inner.lock();
        while world.sockets.contains_key(&world.next_port) {
            world.next_port += 1;
        }
        let port = world.next_port;
        world.next_port += 1;
        world.sockets.insert(
            port,
            SocketState {
                queue: VecDeque::new(),
                recv_waker: None,
            },
        );
        let socket: Arc<dyn AsyncUdpSocket> = Arc::new(SimUdpSocket {
            world: Arc::clone(&self.world),
            port,
        });
        (
            socket,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), port),
        )
    }

    /// Spawn a test-side task (quinn drivers land in the same queue).
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.world.inner.lock().spawn_task(Box::pin(future));
    }

    /// Run the world until `deadline` of virtual time is consumed or the
    /// world runs out of events. Virtual time advances only here.
    pub fn run_until(&self, deadline: std::time::Instant) -> SimRunReport {
        let mut report = SimRunReport::default();
        loop {
            report.steps += 1;
            if report.steps > MAX_STEPS {
                report.exhausted = true;
                return report;
            }
            // Phase A (locked): take spawns, deliver due packets, fire due
            // timers, extract runnable futures.
            let (wakers, runnable): (Vec<Waker>, Vec<ResumedTask>) = {
                let mut world = self.world.inner.lock();
                for future in std::mem::take(&mut world.spawn_queue) {
                    world.spawn_task(future);
                }
                let wakers = world.take_due_events();
                let mut runnable = Vec::new();
                for id in 0..world.tasks.len() {
                    if world.wake_board[id] && world.tasks[id].is_some() {
                        world.wake_board[id] = false;
                        runnable.push((id, world.tasks[id].take()));
                    }
                }
                (wakers, runnable)
            };
            let ran_anything = !wakers.is_empty() || !runnable.is_empty();
            for waker in wakers {
                waker.wake();
            }
            // Phase B: poll outside the lock so wakers can take it.
            let mut resumed: Vec<ResumedTask> = Vec::new();
            for (id, future) in runnable {
                let Some(mut future) = future else { continue };
                let waker = Waker::from(Arc::new(TaskWaker {
                    id,
                    world: Arc::clone(&self.world),
                }));
                let mut cx = Context::from_waker(&waker);
                if future.as_mut().poll(&mut cx).is_ready() {
                    self.world.inner.lock().counters.tasks_completed += 1;
                    resumed.push((id, None));
                } else {
                    resumed.push((id, Some(future)));
                }
            }
            {
                let mut world = self.world.inner.lock();
                for (id, slot) in resumed {
                    world.tasks[id] = slot;
                }
            }
            if ran_anything {
                continue;
            }
            // Nothing runnable at `now`: advance the clock to the next event.
            let next = self.world.inner.lock().next_event();
            match next {
                None => {
                    let world = self.world.inner.lock();
                    report.quiescent = true;
                    report.stuck = world.tasks.iter().any(|slot| slot.is_some());
                    return report;
                }
                Some(event) if event > deadline => {
                    // Fire whatever is due exactly at the deadline, then stop.
                    self.world.inner.lock().now = deadline;
                    let wakers = self.world.inner.lock().take_due_events();
                    for waker in wakers {
                        waker.wake();
                    }
                    report.timed_out = true;
                    return report;
                }
                Some(event) => {
                    self.world.inner.lock().now = event;
                }
            }
        }
    }

    /// `run_until(now + duration)`: the generator's `idle-advance` op.
    pub fn advance(&self, duration: Duration) -> SimRunReport {
        self.run_until(self.now() + duration)
    }

    /// Sleep until the virtual clock reaches `now + duration`: the only
    /// time-based primitive test scenarios may use.
    pub async fn sim_sleep(&self, duration: Duration) {
        let deadline = self.now() + duration;
        let mut timer = self.runtime().new_timer(deadline);
        std::future::poll_fn(|cx| timer.as_mut().poll(cx)).await;
    }

    /// Race `future` against a virtual sleep; `None` means the limit hit.
    #[allow(dead_code)] // consumed by the property-test binary, not canaries
    pub async fn sim_timeout<T>(
        &self,
        limit: Duration,
        future: impl Future<Output = T>,
    ) -> Option<T> {
        match futures::future::select(Box::pin(future), Box::pin(self.sim_sleep(limit))).await {
            futures::future::Either::Left((value, _)) => Some(value),
            futures::future::Either::Right(_) => None,
        }
    }

    /// Spawn a future, drive the world up to `limit`, return its output.
    #[allow(dead_code)] // consumed by the canary binary, not properties
    pub fn drive<T, F>(&self, limit: Duration, future: F) -> Result<T, SimRunReport>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let slot: Arc<DriveSlot<T>> = Arc::new(DriveSlot {
            value: Mutex::new(None),
        });
        self.spawn({
            let slot = Arc::clone(&slot);
            async move {
                let value = future.await;
                *slot.value.lock() = Some(value);
            }
        });
        let report = self.advance(limit);
        let taken = slot.value.lock().take();
        match taken {
            Some(value) => Ok(value),
            None => Err(report),
        }
    }

    pub fn counters(&self) -> SimCounters {
        self.world.inner.lock().counters
    }

    #[allow(dead_code)] // consumed by the canary binary, not properties
    pub fn trace(&self) -> Vec<TraceEvent> {
        self.world.inner.lock().trace.clone()
    }

    /// True when no task, timer, delivery or queued datagram remains.
    pub fn is_quiescent(&self) -> bool {
        let world = self.world.inner.lock();
        world.tasks.iter().all(|slot| slot.is_none())
            && world.deliveries.is_empty()
            && world.timers.is_empty()
            && world.sockets.values().all(|s| s.queue.is_empty())
    }
}

#[allow(dead_code)] // paired with drive, canary-binary only
struct DriveSlot<T> {
    value: Mutex<Option<T>>,
}

/// Always-writable poller: the sim never returns WouldBlock from `try_send`.
#[derive(Debug)]
struct SimPoller;

impl UdpPoller for SimPoller {
    fn poll_writable(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// quinn `AsyncUdpSocket` over the virtual world.
struct SimUdpSocket {
    world: Arc<SimWorld>,
    port: u16,
}

impl fmt::Debug for SimUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimUdpSocket")
            .field("port", &self.port)
            .finish()
    }
}

impl Drop for SimUdpSocket {
    fn drop(&mut self) {
        self.world.inner.lock().sockets.remove(&self.port);
    }
}

impl AsyncUdpSocket for SimUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // In-memory sends never block; quinn consults the poller only after
        // a WouldBlock, which `try_send` never returns.
        Box::pin(SimPoller)
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        let mut world = self.world.inner.lock();
        let destination = match transmit.destination {
            SocketAddr::V4(v4) => v4.port(),
            SocketAddr::V6(_) => {
                world.seq += 1;
                let seq = world.seq;
                world.trace.push(TraceEvent::Dropped {
                    seq,
                    reason: "non-ipv4-destination",
                });
                return Ok(());
            }
        };
        match transmit.segment_size {
            Some(size) if size > 0 => {
                for chunk in transmit.contents.chunks(size) {
                    world.submit(self.port, destination, chunk);
                }
            }
            _ => world.submit(self.port, destination, transmit.contents),
        }
        Ok(())
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut world = self.world.inner.lock();
        let Some(socket) = world.sockets.get_mut(&self.port) else {
            return Poll::Ready(Err(io::Error::other("sim socket was closed")));
        };
        let mut count = 0;
        while count < bufs.len() {
            let Some((source, data)) = socket.queue.pop_front() else {
                break;
            };
            let len = data.len().min(bufs[count].len());
            bufs[count][..len].copy_from_slice(&data[..len]);
            meta[count] = RecvMeta {
                addr: source,
                len,
                stride: len,
                ecn: None,
                dst_ip: None,
            };
            count += 1;
        }
        if count == 0 {
            socket.recv_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        Poll::Ready(Ok(count))
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            self.port,
        ))
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// quinn `Runtime` over the virtual world: virtual-clock timers, task-queue
/// spawn, virtual `now`. Same trait contract as `MinissQuicRuntime`, so
/// drift between the two shows up here.
struct SimRuntime {
    world: Arc<SimWorld>,
}

impl fmt::Debug for SimRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimRuntime").finish_non_exhaustive()
    }
}

impl QuinnRuntime for SimRuntime {
    fn new_timer(&self, deadline: std::time::Instant) -> Pin<Box<dyn AsyncTimer>> {
        let mut world = self.world.inner.lock();
        let id = world.next_timer_id;
        world.next_timer_id += 1;
        world.timers.insert(
            id,
            TimerEntry {
                deadline,
                waker: None,
            },
        );
        Box::pin(SimTimer {
            world: Arc::clone(&self.world),
            id,
        })
    }

    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + Send>>) {
        self.world.inner.lock().spawn_task(future);
    }

    fn wrap_udp_socket(&self, _socket: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>> {
        Err(io::Error::other("sim runtime does not wrap std sockets"))
    }

    fn now(&self) -> std::time::Instant {
        self.world.inner.lock().now
    }
}

struct SimTimer {
    world: Arc<SimWorld>,
    id: u64,
}

impl fmt::Debug for SimTimer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimTimer").field("id", &self.id).finish()
    }
}

impl Drop for SimTimer {
    fn drop(&mut self) {
        self.world.inner.lock().timers.remove(&self.id);
    }
}

impl AsyncTimer for SimTimer {
    fn reset(self: Pin<&mut Self>, deadline: std::time::Instant) {
        let this = self.get_mut();
        let mut world = this.world.inner.lock();
        // Re-insert unconditionally: the entry is absent after the timer
        // fired (firing removes it), and quinn reuses one timer object by
        // resetting it to each new deadline.
        world.timers.insert(
            this.id,
            TimerEntry {
                deadline,
                waker: None,
            },
        );
    }

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut world = this.world.inner.lock();
        let now = world.now;
        // A missing entry means the timer already fired.
        let fired = world
            .timers
            .get(&this.id)
            .is_none_or(|entry| entry.deadline <= now);
        if fired {
            world.timers.remove(&this.id);
            return Poll::Ready(());
        }
        if let Some(entry) = world.timers.get_mut(&this.id) {
            entry.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

/// One rcgen self-signed "localhost" certificate pair (methodology §9.5:
/// handshake-success claims only, no byte-level TLS replay).
pub fn sim_certs() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let cert = generate_simple_self_signed(vec!["localhost".into()]).expect("rcgen self-signed");
    (
        cert.cert.into(),
        PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into()),
    )
}

fn sim_crypto_server(
    cert: &CertificateDer<'static>,
    key: &PrivateKeyDer<'static>,
) -> quinn::crypto::rustls::QuicServerConfig {
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 versions")
    .with_no_client_auth()
    .with_single_cert(vec![cert.clone()], key.clone_key())
    .expect("server cert");
    crypto.alpn_protocols = vec![b"sim/1".to_vec()];
    QuicServerConfig::try_from(crypto).expect("quic server crypto")
}

fn sim_crypto_client(cert: &CertificateDer<'static>) -> quinn::crypto::rustls::QuicClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.clone()).expect("valid certificate");
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    crypto.alpn_protocols = vec![b"sim/1".to_vec()];
    QuicClientConfig::try_from(crypto).expect("quic client crypto")
}

/// Bind a server endpoint over the sim network; the returned certificate is
/// what clients must trust.
pub fn sim_server_endpoint(
    net: &SimNetwork,
    transport: Option<Arc<quinn::TransportConfig>>,
) -> (quinn::Endpoint, SocketAddr, CertificateDer<'static>) {
    let (cert, key) = sim_certs();
    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(sim_crypto_server(&cert, &key)));
    server_config.transport = transport.unwrap_or_default();
    let (socket, addr) = net.bind();
    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        net.runtime(),
    )
    .expect("sim server endpoint");
    (endpoint, addr, cert)
}

/// Bind a client endpoint over the sim network trusting `cert`.
pub fn sim_client_endpoint(net: &SimNetwork, cert: CertificateDer<'static>) -> quinn::Endpoint {
    let mut client_config = quinn::ClientConfig::new(Arc::new(sim_crypto_client(&cert)));
    client_config.transport_config(Arc::new(quinn::TransportConfig::default()));
    let (socket, _) = net.bind();
    let mut endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        None,
        socket,
        net.runtime(),
    )
    .expect("sim client endpoint");
    endpoint.set_default_client_config(client_config);
    endpoint
}
