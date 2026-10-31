//! A1: shard-local QUIC connection accounting with cap enforcement
//! (wt-quic-audit A1; methodology §T4 INV-2).
//!
//! Every mutable piece of state here is `Cell`/`RefCell` inside a struct
//! owned by the accept loop on the endpoint's owning shard — thread-local
//! by construction (AGENTS.md MUST #3), with no locks, no atomics and no
//! cross-core traffic. The accept policy is decided per arrival at the
//! accept point; the limits are read-only `Arc`-shared config (MUST #5),
//! replaced wholesale by the control plane, never mutated in place.
//!
//! Slot accounting is conserved by construction: a slot is reserved at the
//! accept point (so concurrent handshakes are bounded by the cap too),
//! returned when the handshake fails, and freed when the tracked
//! connection's closure is observed by the next sweep. INV-2 pins the
//! observable halves: high water never exceeds the cap, and
//! `accepted + refused + retried + handshake_failures` accounts for every
//! arrival.

use quinn::{Connection, Endpoint, Incoming};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

/// Read-only accept policy. `max_connections` caps tracked connections
/// (reservations included); `use_retry` sends the first contact from each
/// remote through a stateless retry (anti-flood) before the cap decision.
#[derive(Debug, Clone)]
pub struct QuicLimits {
    pub max_connections: usize,
    pub use_retry: bool,
}

impl Default for QuicLimits {
    fn default() -> Self {
        Self {
            max_connections: 128,
            use_retry: false,
        }
    }
}

/// The accept-point decision for one arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptDecision {
    /// Take a slot and run the handshake.
    Accept,
    /// Reject the handshake (overflow past the cap).
    Refuse,
    /// Stateless retry for an unseen remote (anti-flood; no slot).
    Retry,
}

/// Observable acceptor accounting; the A2 bridge folds these deltas into
/// the shard snapshot, and INV-2 asserts them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcceptorCounters {
    pub accepted: u64,
    pub refused: u64,
    pub retried: u64,
    pub handshake_failures: u64,
    /// Maximum tracked slots ever live (reservations included).
    pub high_water: usize,
    /// Slots currently live.
    pub active: usize,
}

#[derive(Debug, Default)]
struct FlowCounters {
    accepted: u64,
    refused: u64,
    retried: u64,
    handshake_failures: u64,
}

/// Tracked connections: each held handle reserves one slot; the sweep
/// frees the slot once the connection is observed closed.
#[derive(Debug, Default)]
struct ConnTable {
    active: usize,
    high_water: usize,
    tracked: Vec<Connection>,
}

/// Shard-local accept-point policy wrapper over one `quinn::Endpoint`.
///
/// Owned by the accept loop; `next_connection` is the only driving path.
/// Because every decision reads and writes shard-local `RefCell` state,
/// the cap is enforced without any synchronization whatsoever — the same
/// quinn connections that arrive here are driven on this shard, so the
/// accounting cannot drift from the endpoint's reality.
pub struct QuicAcceptor {
    endpoint: Endpoint,
    limits: Arc<QuicLimits>,
    table: RefCell<ConnTable>,
    flow: RefCell<FlowCounters>,
    /// Remotes that already went through a stateless retry round.
    retried_remotes: RefCell<BTreeSet<u16>>,
}

impl std::fmt::Debug for QuicAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicAcceptor")
            .field("limits", &self.limits)
            .field("table", &self.table)
            .field("flow", &self.flow)
            .finish_non_exhaustive()
    }
}

impl QuicAcceptor {
    /// The pure cap model: one decision per arrival, given the shard-local
    /// accounting. Exposed for exhaustive table tests (INV-2).
    pub fn decision(
        active: usize,
        cap: usize,
        use_retry: bool,
        already_seen: bool,
    ) -> AcceptDecision {
        if use_retry && !already_seen {
            AcceptDecision::Retry
        } else if active >= cap {
            AcceptDecision::Refuse
        } else {
            AcceptDecision::Accept
        }
    }

    /// Wrap an endpoint with the accept-point policy.
    pub fn new(endpoint: Endpoint, limits: Arc<QuicLimits>) -> Self {
        Self {
            endpoint,
            limits,
            table: RefCell::new(ConnTable::default()),
            flow: RefCell::new(FlowCounters::default()),
            retried_remotes: RefCell::new(BTreeSet::new()),
        }
    }

    /// The read-only limits this acceptor enforces.
    pub fn limits(&self) -> &Arc<QuicLimits> {
        &self.limits
    }

    /// Live slots right now (reservations included).
    pub fn active(&self) -> usize {
        self.table.borrow().active
    }

    /// Snapshot of the observable accounting.
    pub fn counters(&self) -> AcceptorCounters {
        let table = self.table.borrow();
        let flow = self.flow.borrow();
        AcceptorCounters {
            accepted: flow.accepted,
            refused: flow.refused,
            retried: flow.retried,
            handshake_failures: flow.handshake_failures,
            high_water: table.high_water,
            active: table.active,
        }
    }

    /// One accept-point decision for `incoming`.
    fn decide_for(&self, remote_port: u16) -> AcceptDecision {
        let table = self.table.borrow();
        let already_seen = self.retried_remotes.borrow().contains(&remote_port);
        Self::decision(
            table.active,
            self.limits.max_connections,
            self.limits.use_retry,
            already_seen,
        )
    }

    /// Sweep tracked connections: a closed connection frees its slot.
    ///
    /// Closedness is state, not an event: polling a fresh `closed()` future
    /// returns `Ready` once the connection is closed, even with a no-op
    /// waker, so a synchronous sweep is exact (no missed reclamation) and
    /// needs no background task. An idle accept loop simply defers the
    /// sweep to its next pass.
    pub fn reclaim_closed(&self) {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut table = self.table.borrow_mut();
        let before = table.tracked.len();
        table.tracked.retain(|connection| {
            let mut closed = Box::pin(connection.closed());
            matches!(
                std::future::Future::poll(closed.as_mut(), &mut cx),
                Poll::Pending
            )
        });
        table.active -= before - table.tracked.len();
    }

    /// Next accepted connection, applying the accept-point policy to every
    /// arrival. Returns `None` once the endpoint is closed.
    pub async fn next_connection(&mut self) -> Option<Connection> {
        loop {
            self.reclaim_closed();
            let incoming: Incoming = self.endpoint.accept().await?;
            let remote_port = incoming.remote_address().port();
            match self.decide_for(remote_port) {
                AcceptDecision::Retry => {
                    self.retried_remotes.borrow_mut().insert(remote_port);
                    // `retry` consumes the arrival; only its result tells
                    // whether the retry round was actually sent.
                    match incoming.retry() {
                        Ok(()) => self.flow.borrow_mut().retried += 1,
                        Err(_) => {
                            // Retry unsupported for this arrival: refuse is
                            // impossible (consumed); count as a handshake
                            // failure so arrivals stay accounted.
                            self.flow.borrow_mut().handshake_failures += 1;
                        }
                    }
                }
                AcceptDecision::Refuse => {
                    incoming.refuse();
                    self.flow.borrow_mut().refused += 1;
                }
                AcceptDecision::Accept => {
                    // Reserve at the accept point: concurrent handshakes
                    // are bounded by the cap exactly like live slots.
                    {
                        let mut table = self.table.borrow_mut();
                        table.active += 1;
                        table.high_water = table.high_water.max(table.active);
                    }
                    match incoming.accept() {
                        Ok(connecting) => match connecting.await {
                            Ok(connection) => {
                                self.table.borrow_mut().tracked.push(connection.clone());
                                self.flow.borrow_mut().accepted += 1;
                                return Some(connection);
                            }
                            Err(_handshake) => {
                                // Peer mid-handshake close: return the slot.
                                self.table.borrow_mut().active -= 1;
                                self.flow.borrow_mut().handshake_failures += 1;
                            }
                        },
                        Err(_) => {
                            self.table.borrow_mut().active -= 1;
                            self.flow.borrow_mut().handshake_failures += 1;
                        }
                    }
                }
            }
        }
    }
}
