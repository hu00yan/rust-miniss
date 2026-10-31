//! A2: per-shard QUIC stats bridge (wt-quic-audit A2; methodology §T4
//! INV-4).
//!
//! Counters are shard-local thread-locals (AGENTS.md MUST #3 — the
//! prescribed `Cell`/`RefCell` pattern), written only from the endpoint's
//! owning shard through delta-encoded samples of quinn's own
//! `Endpoint::stats()` / `Connection::stats()`, and read by the shard's
//! `CollectMetrics` handler to fold into `CoreMetricsSnapshot`. Outside the
//! hot path, no locks, no atomics, no cross-core traffic: each shard folds
//! its own counters, aggregation stays on the control plane (MUST #6).

use quinn::Endpoint;
use std::cell::RefCell;

/// Cumulative QUIC counters for one shard. `connections_active` is the
/// high-water mark (a gauge — summing gauges is meaningless).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuicShardStats {
    pub udp_tx_packets: u64,
    pub udp_rx_packets: u64,
    pub handshakes_accepted: u64,
    pub handshakes_refused: u64,
    pub connections_active: usize,
}

thread_local! {
    static SHARD_QUIC_STATS: RefCell<QuicShardStats> = const {
        RefCell::new(QuicShardStats {
            udp_tx_packets: 0,
            udp_rx_packets: 0,
            handshakes_accepted: 0,
            handshakes_refused: 0,
            connections_active: 0,
        })
    };
}

/// Fold one acceptor's delta sample into the shard's counters. Must be
/// called from the endpoint's owning shard.
pub(crate) fn record(delta: &QuicShardStats) {
    SHARD_QUIC_STATS.with(|stats| {
        let mut stats = stats.borrow_mut();
        stats.udp_tx_packets = stats.udp_tx_packets.saturating_add(delta.udp_tx_packets);
        stats.udp_rx_packets = stats.udp_rx_packets.saturating_add(delta.udp_rx_packets);
        stats.handshakes_accepted = stats
            .handshakes_accepted
            .saturating_add(delta.handshakes_accepted);
        stats.handshakes_refused = stats
            .handshakes_refused
            .saturating_add(delta.handshakes_refused);
        stats.connections_active = stats.connections_active.max(delta.connections_active);
    });
}

/// Control-plane read for the shard's `CollectMetrics` handler. Worker
/// threads only; each runtime's workers are fresh, so counters never leak
/// across runtimes.
pub(crate) fn snapshot() -> QuicShardStats {
    SHARD_QUIC_STATS.with(|stats| *stats.borrow())
}

/// One endpoint's last-sampled `Endpoint::stats()` values.
#[derive(Debug, Default)]
pub(crate) struct EndpointSample {
    last_accepted: u64,
    last_refused: u64,
}

/// Compute the delta between two quinn endpoint samples.
fn endpoint_delta(
    sample: &mut EndpointSample,
    endpoint: &Endpoint,
    active: usize,
) -> QuicShardStats {
    let stats = endpoint.stats();
    let delta = QuicShardStats {
        udp_tx_packets: 0,
        udp_rx_packets: 0,
        handshakes_accepted: stats.accepted_handshakes - sample.last_accepted,
        handshakes_refused: stats.refused_handshakes - sample.last_refused,
        connections_active: active,
    };
    sample.last_accepted = stats.accepted_handshakes;
    sample.last_refused = stats.refused_handshakes;
    delta
}

/// Sample one endpoint's handshake counters. Used by the acceptor's
/// `stats_tick`; kept here so the delta-encoding lives in one place.
pub(crate) fn sample_endpoint(sample: &mut EndpointSample, endpoint: &Endpoint, active: usize) {
    record(&endpoint_delta(sample, endpoint, active));
}
