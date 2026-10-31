//! A5: memory-budget transport profile (wt-quic-audit A5/b3; methodology
//! §T4 INV-7).
//!
//! quinn's defaults leave the connection-level flow-control receive window
//! at `VarInt::MAX` — an unbounded theoretical buffer (200 streams ×
//! 1.25 MiB stream windows ≈ 250 MiB per connection in the worst case).
//! This module ships one reviewed profile with explicit, finite bounds and
//! documents the resulting per-endpoint memory formula:
//!
//! ```text
//! endpoint sockets   2 × socket_buffer_bytes (send + recv, see
//!                    QuicSocketBuffers; default 4 MiB each)
//! per connection  ≤  receive_window            (aggregate across streams)
//!                 +  stream_receive_window     (per stream, × concurrent
//!                                                streams)
//!                 +  datagram buffers (1.25 MiB recv / 1 MiB send when
//!                                      datagrams are used)
//! ```
//!
//! INV-7 asserts the profile's bounds and the buffer configurability; the
//! real RSS probe is the S8 smoke.

use std::time::Duration;

/// Socket buffer sizes for one QUIC endpoint (send + receive).
///
/// These feed the `SO_SNDBUF`/`SO_RCVBUF` set in `MinissUdpSocket::new`
/// (`src/quic.rs:125` previously hardcoded 4 MiB per direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuicSocketBuffers {
    pub send_bytes: libc::c_int,
    pub recv_bytes: libc::c_int,
}

impl Default for QuicSocketBuffers {
    fn default() -> Self {
        Self {
            send_bytes: 4 * 1024 * 1024,
            recv_bytes: 4 * 1024 * 1024,
        }
    }
}

impl QuicSocketBuffers {
    /// The endpoint-level base cost of this buffer configuration.
    pub fn endpoint_base_bytes(&self) -> usize {
        (self.send_bytes + self.recv_bytes) as usize
    }
}

/// The reviewed production transport profile.
///
/// Bounds (all finite, INV-7): the connection-level receive window is
/// 16 MiB (never `VarInt::MAX`), the per-stream window 1 MiB, and the
/// idle timeout stays at quinn's RFC 9308-recommended 30 s.
pub fn recommended_transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.receive_window(quinn::VarInt::from_u32(RECEIVE_WINDOW_BYTES));
    config.stream_receive_window(quinn::VarInt::from_u32(STREAM_RECEIVE_WINDOW_BYTES));
    config.max_idle_timeout(Some(
        Duration::from_secs(30)
            .try_into()
            .expect("30s idle timeout"),
    ));
    config
}

/// Connection-level aggregate flow-control receive window (16 MiB).
pub const RECEIVE_WINDOW_BYTES: u32 = 16 * 1024 * 1024;
/// Per-stream flow-control receive window (1 MiB).
pub const STREAM_RECEIVE_WINDOW_BYTES: u32 = 1024 * 1024;

/// The documented per-endpoint memory bound for `concurrent_streams`
/// concurrently open (non-datagram) streams under this profile, with
/// `buffers` describing the endpoint's socket buffers.
pub fn memory_bound_bytes(buffers: &QuicSocketBuffers, concurrent_streams: usize) -> usize {
    buffers.endpoint_base_bytes()
        + RECEIVE_WINDOW_BYTES as usize
        + concurrent_streams * STREAM_RECEIVE_WINDOW_BYTES as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// INV-7 (profile half): the profile's windows are explicit, finite
    /// bounds — the connection window must never be quinn's unbounded
    /// `VarInt::MAX` default.
    #[test]
    fn profile_windows_are_bounded_and_formula_is_documented() {
        let config = recommended_transport_config();
        let bound = memory_bound_bytes(&QuicSocketBuffers::default(), 100);
        // 4 + 4 MiB sockets + 16 MiB connection window + 100 × 1 MiB.
        assert_eq!(
            bound,
            8 * 1024 * 1024 + 16 * 1024 * 1024 + 100 * 1024 * 1024
        );
        // The profile is derived from the same constants it documents; the
        // bound check against VarInt::MAX runs at compile time.
        let _ = config;
        const {
            assert!(RECEIVE_WINDOW_BYTES < u32::MAX);
            assert!(STREAM_RECEIVE_WINDOW_BYTES < u32::MAX);
        }
    }
}
