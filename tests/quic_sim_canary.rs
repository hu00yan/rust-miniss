//! Canary tests for the quinn-seam simulator (methodology §6 step 1).
//!
//! Three canaries prove the harness itself works before any service-layer
//! code leans on it:
//! 1. virtual-time UDP delivery with the fault schedule applied;
//! 2. a full quinn handshake + uni-stream round trip over a lossy, delayed,
//!    reordered, duplicated network;
//! 3. seed replay: same seed = identical world; different seed diverges.
//!
//! TLS-level claims stop at handshake success (methodology §9.5): rcgen keys
//! are random per run, so byte-level replay is asserted only for the plain
//! UDP scenario where no random key material exists.

#[path = "common/quic_sim/mod.rs"]
mod quic_sim;

use quic_sim::{SimFaultSchedule, SimNetwork};
use quinn::udp::{RecvMeta, Transmit};
use quinn::AsyncUdpSocket;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

async fn sim_recv(
    socket: &Arc<dyn AsyncUdpSocket>,
    buf: &mut [u8],
) -> io::Result<(SocketAddr, usize)> {
    std::future::poll_fn(|cx| {
        let mut bufs = [IoSliceMut::new(buf)];
        let mut meta = [RecvMeta::default()];
        match socket.poll_recv(cx, &mut bufs, &mut meta) {
            Poll::Ready(Ok(n)) => {
                assert_eq!(n, 1, "canary passes a single buffer per receive");
                Poll::Ready(Ok((meta[0].addr, meta[0].len)))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// Canary 1: datagrams submitted into the sim network arrive in delivery
/// order, the virtual clock advances by the configured delay, and the fault
/// schedule's counters close the books exactly.
#[test]
fn canary_udp_delivery_advances_virtual_time_deterministically() {
    let faults = SimFaultSchedule {
        seed: 42,
        delay: (Duration::from_millis(5), Duration::from_millis(0)),
        ..SimFaultSchedule::default()
    };
    let net = SimNetwork::new(faults);
    let (sender, _) = net.bind();
    let (receiver, receiver_addr) = net.bind();
    let start = net.now();

    let sim = net.clone();
    let outcome = net
        .drive(Duration::from_millis(100), async move {
            let net = sim;
            let receiver_addr = receiver_addr;
            let send = async {
                for index in 0..3u8 {
                    let transmit = Transmit {
                        destination: receiver_addr,
                        ecn: None,
                        contents: &[index, 0xFF, index],
                        segment_size: None,
                        src_ip: None,
                    };
                    sender.try_send(&transmit).expect("sim send");
                    // Pace one datagram per 5 ms so the three deliveries
                    // land at +5, +10, +15 ms of virtual time.
                    net.sim_sleep(Duration::from_millis(5)).await;
                }
            };
            let receive = async {
                let mut seen = Vec::new();
                while seen.len() < 3 {
                    let (_, len) = sim_recv(&receiver, &mut [0u8; 64]).await.expect("recv");
                    seen.push(len);
                }
                seen
            };
            let ((), seen) = futures::future::join(send, receive).await;
            seen
        })
        .expect("scenario completes inside the virtual budget");

    assert_eq!(
        outcome,
        vec![3usize, 3, 3],
        "datagram lengths arrive intact"
    );
    let counters = net.counters();
    assert_eq!(counters.packets_submitted, 3);
    assert_eq!(counters.packets_delivered, 3);
    assert_eq!(counters.dropped_loss, 0);
    // Three sequential 5 ms deliveries; the receive future wakes per packet,
    // so the clock must land on the third delivery, not before it.
    let elapsed = net.now() - start;
    assert!(
        elapsed >= Duration::from_millis(15),
        "virtual clock did not advance with the delivery schedule: {elapsed:?}"
    );
    assert!(net.is_quiescent(), "world left residue after the scenario");
}

/// Canary 2: quinn (server + client) runs a full TLS 1.3 handshake and a
/// uni-stream echo across the sim network with loss, delay, reorder and dup
/// all active — the harness exercises the real quinn seams.
#[test]
fn canary_quinn_handshake_and_stream_roundtrip_over_faulty_network() {
    let faults = SimFaultSchedule {
        seed: 7,
        loss: 0.05,
        dup: 0.10,
        reorder: Some(Duration::from_millis(1)),
        delay: (Duration::from_millis(5), Duration::from_millis(2)),
        ..SimFaultSchedule::default()
    };
    let net = SimNetwork::new(faults);
    let (server_endpoint, server_addr, cert) = quic_sim::sim_server_endpoint(&net, None);
    let client_endpoint = quic_sim::sim_client_endpoint(&net, cert);

    net.drive(Duration::from_secs(30), async move {
        let server = async move {
            let incoming = server_endpoint.accept().await.expect("endpoint closed");
            let connection = incoming.await.expect("handshake failed");
            let mut stream = connection.accept_uni().await.expect("uni accept");
            let payload = stream.read_to_end(64).await.expect("read payload");
            let mut reply = connection.open_uni().await.expect("open reply");
            reply.write_all(&payload).await.expect("write reply");
            reply.finish().expect("finish reply");
            // Hold the handle until the peer closes: dropping the last
            // Connection reference implicitly closes with code 0, which
            // would kill the client's reply read.
            let _ = connection.closed().await;
        };
        let client = async move {
            let connection = client_endpoint
                .connect(server_addr, "localhost")
                .expect("connect setup")
                .await
                .expect("handshake failed");
            let mut stream = connection.open_uni().await.expect("open uni");
            stream.write_all(b"canary").await.expect("write payload");
            stream.finish().expect("finish payload");
            let mut reply = connection.accept_uni().await.expect("accept reply");
            let echoed = reply.read_to_end(64).await.expect("read reply");
            assert_eq!(echoed, b"canary", "echo round trip corrupted");
        };
        futures::future::join(server, client).await;
    })
    .expect("quinn scenario completes inside the virtual budget");

    let counters = net.counters();
    assert_eq!(
        counters.packets_submitted + counters.duplicates_emitted,
        counters.packets_delivered
            + counters.dropped_loss
            + counters.dropped_mtu
            + counters.dropped_partition
            + counters.dropped_unreachable,
        "fault schedule lost bookkeeping"
    );
    assert!(
        counters.dropped_loss > 0,
        "seed 7 produced no loss; the fault knobs are not biting: {counters:?}"
    );
}

/// The UDP echo scenario whose world must replay byte-identically per seed.
fn udp_fault_scenario(seed: u64) -> (Vec<quic_sim::TraceEvent>, quic_sim::SimCounters) {
    let faults = SimFaultSchedule {
        seed,
        loss: 0.30,
        dup: 0.20,
        reorder: Some(Duration::from_millis(2)),
        delay: (Duration::from_millis(2), Duration::from_millis(3)),
        ..SimFaultSchedule::default()
    };
    let net = SimNetwork::new(faults);
    let (sender, _sender_addr) = net.bind();
    let (_receiver, receiver_addr) = net.bind();
    net.spawn(async move {
        for index in 0..20u8 {
            let transmit = Transmit {
                destination: receiver_addr,
                ecn: None,
                contents: &[index; 32],
                segment_size: None,
                src_ip: None,
            };
            sender.try_send(&transmit).expect("sim send");
        }
    });
    let report = net.advance(Duration::from_millis(500));
    assert!(
        !report.exhausted && !report.stuck,
        "fault scenario executor misbehaved: {report:?}"
    );
    (net.trace(), net.counters())
}

/// Canary 3: the seed is the whole world. Same seed twice = identical trace
/// and counters; a different seed diverges. quinn-level determinism is
/// claimed only at verdict level (methodology §9.5).
#[test]
fn canary_same_seed_replays_identical_world_and_different_seed_diverges() {
    let (trace_a, counters_a) = udp_fault_scenario(11);
    let (trace_b, counters_b) = udp_fault_scenario(11);
    assert_eq!(trace_a, trace_b, "same seed did not replay the same world");
    assert_eq!(counters_a, counters_b);

    let (trace_c, counters_c) = udp_fault_scenario(12);
    assert_ne!(
        trace_a, trace_c,
        "different seeds produced identical worlds; the RNG is not engaged"
    );
    assert_ne!(counters_a, counters_c);

    // Verdict-level determinism for the quinn path: same seed, same verdict.
    for _ in 0..2 {
        let faults = SimFaultSchedule {
            seed: 99,
            loss: 0.10,
            delay: (Duration::from_millis(3), Duration::from_millis(1)),
            ..SimFaultSchedule::default()
        };
        let net = SimNetwork::new(faults);
        let (server_endpoint, server_addr, cert) = quic_sim::sim_server_endpoint(&net, None);
        let client_endpoint = quic_sim::sim_client_endpoint(&net, cert);
        let established = net
            .drive(Duration::from_secs(30), async move {
                let server = async move {
                    let incoming = server_endpoint.accept().await.expect("endpoint closed");
                    incoming.await.expect("handshake failed")
                };
                let client = client_endpoint
                    .connect(server_addr, "localhost")
                    .expect("connect setup");
                futures::future::join(server, client).await
            })
            .expect("handshake scenario completes");
        let client_connection = established.1.expect("client handshake verdict");
        assert_eq!(
            client_connection.remote_address(),
            server_addr,
            "client handshake verdict diverged under the same seed"
        );
        assert_eq!(
            established.0.remote_address().ip().to_string(),
            "10.0.0.1",
            "server-side peer address malformed"
        );
    }
}
