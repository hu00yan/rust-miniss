# Protocol integration verification — 2026-07-13

This is a correctness record for the current commit, not a performance claim.
GitHub Actions is not part of this repository's gate; the commands below were
run manually on the two supported targets.

| Target | Backend | HTTP/2 | QUIC/HTTP-3 |
|---|---|---:|---:|
| macOS | kqueue via mio | pass | pass |
| Orb Ubuntu | io_uring | pass | pass |

Commands:

```text
cargo test --test hyper_http2 --all-features -- --nocapture
cargo test --test quinn_h3 --all-features -- --nocapture
cargo test --test tls_tests --all-features -- --nocapture

orb -m ubuntu sh -lc 'cd /mnt/mac/Users/huyan00/mycode/rust-miniss && \
  cargo test --test hyper_http2 --all-features -- --nocapture && \
  cargo test --test quinn_h3 --all-features -- --nocapture && \
  cargo test --test tls_tests --all-features -- --nocapture'
```

The HTTP/2 test drives four concurrent response streams, including a 32 KiB
multi-frame body under a 16 KiB stream window, a 500 response, and a
client-cancelled request stream. The raw h2 client explicitly releases consumed
capacity, so the test exercises WINDOW_UPDATE rather than relying on the default
65,535-byte window.
The QUIC tests are bounded and perform a real loopback UDP exchange with an H3
request/response/body, endpoint rebind migration, and QUIC datagram validation.
A second QUIC test completes a session-ticket handshake and verifies client
0-RTT acceptance plus a server-side early-data stream. TLS has bounded TLS
1.3 client handshake/read/write/close, untrusted-certificate, and hostname
mismatch tests. The macOS path uses optimistic UDP write readiness because kqueue does
not reliably report the UDP write filter; the actual nonblocking send still
returns errors to Quinn.
The QUIC suite also runs a two-worker test that creates the server and client
endpoints on separate owned shards and completes 32 concurrent stream
exchanges. A sustained multi-connection test for one endpoint created from a
control thread remains an explicit ownership gap in `TODO.md`.
