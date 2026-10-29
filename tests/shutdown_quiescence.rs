//! Shutdown-reclaim regressions promoted from `just stress` into
//! `just test-regular` (defect-forensics 2026-10, Tier-1 items 3+4): each run
//! now passes the live quiescence oracle — all-zero termination snapshot
//! including the per-shard resource-balance ledger, plus a process fd-count
//! guard — instead of a bare post-join snapshot assert.
//!
//! Run serially (`--test-threads=1`, as `just test-regular` does): the fd
//! baseline is process-global, and the helper's settle window only absorbs
//! *transient* concurrent fds, not a sibling test holding a runtime open.
//!
//! The originals remain in `tests/stress_test.rs` behind `just stress` with
//! their full stress parameters and rounds; these are the bounded
//! single-round forms, promoted so the oracle runs on every `just check`.

#[path = "common/quiescent.rs"]
mod quiescent;

use quiescent::assert_quiescent;
use rust_miniss::net::AsyncUdpSocket;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bounded promotion of
/// `stress_test::test_multicore_loaded_shutdown_reclaims_all_state`
/// (single round, 8 timer tasks, 2 workers). This is the loaded-shutdown
/// drain/park-tail and cancel-reap surface (finding #40's class), now
/// exercised under the quiescence oracle on every `just check`.
#[test]
fn loaded_shutdown_with_timer_tasks_reclaims_state_quiescently() {
    assert_quiescent!(Some(2), "loaded shutdown quiescence", |runtime| {
        let stop = Arc::new(AtomicBool::new(false));
        for _ in 0..8 {
            let stop = stop.clone();
            runtime
                .spawn(async move {
                    while !stop.load(Ordering::Acquire) {
                        rust_miniss::timer::sleep(Duration::from_millis(1)).await;
                    }
                })
                .expect("loaded shutdown task admission");
        }
        runtime.block_on(async {
            rust_miniss::timer::sleep(Duration::from_millis(50)).await;
        });
        stop.store(true, Ordering::Release);
    });
}

/// Bounded promotion of
/// `stress_test::test_multicore_pending_udp_shutdown_reclaims_socket_leases`
/// (single round, 8 pending receive sockets, 2 workers). Pending recv
/// operations and their descriptor leases are canceled and reclaimed through
/// the terminal path at shutdown — the abandoned-lease surface (finding #43 /
/// D24) — under the full oracle, without cross-thread TCP client timing.
#[test]
fn pending_udp_shutdown_reclaims_socket_leases_quiescently() {
    assert_quiescent!(Some(2), "udp lease quiescence", |runtime| {
        let ready = Arc::new(AtomicUsize::new(0));
        for _ in 0..8 {
            let ready = ready.clone();
            runtime
                .spawn(async move {
                    let socket = AsyncUdpSocket::bind(
                        "127.0.0.1:0"
                            .parse::<std::net::SocketAddr>()
                            .expect("bind address"),
                    )
                    .expect("bind quiescence UDP socket");
                    ready.fetch_add(1, Ordering::Release);
                    let mut buffer = [0u8; 64];
                    let _ = socket.recv_from(&mut buffer).await;
                })
                .expect("admit quiescence UDP task");
        }
        runtime.block_on(async {
            let deadline = Instant::now() + Duration::from_secs(2);
            while ready.load(Ordering::Acquire) != 8 {
                assert!(Instant::now() < deadline, "UDP tasks did not become ready");
                rust_miniss::timer::sleep(Duration::from_millis(1)).await;
            }
        });
    });
}
