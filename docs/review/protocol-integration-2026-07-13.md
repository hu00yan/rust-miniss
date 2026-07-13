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

orb -m ubuntu sh -lc 'cd /mnt/mac/Users/huyan00/mycode/rust-miniss && \
  cargo test --test hyper_http2 --all-features -- --nocapture && \
  cargo test --test quinn_h3 --all-features -- --nocapture'
```

The QUIC test is bounded and performs a real loopback UDP exchange with an H3
request, response, and body validation. The macOS path uses optimistic UDP
write readiness because kqueue does not reliably report the UDP write filter;
the actual nonblocking send still returns errors to Quinn.
