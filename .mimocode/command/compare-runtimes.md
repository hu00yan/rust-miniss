---
description: "Build and benchmark http_echo for miniss, tokio, and monoio runtimes side-by-side in OrbStack, output comparison table."
---

# compare-runtimes — Multi-runtime benchmark comparison

Build http_echo for miniss, tokio_echo, and monoio_echo, run each with wrk under identical conditions, and output a comparison table.

## Usage

```
compare-runtimes [OPTIONS]
```

### Options

| Arg | Default | Description |
|-----|---------|-------------|
| `$1` or `--conns` | `100` | wrk connection count |
| `$2` or `--duration` | `10s` | wrk duration |
| `$3` or `--threads` | `4` | wrk thread count |
| `$4` or `--port-base` | `19000` | Starting port (each runtime gets port+0, +1, +2) |
| `$5` or `--features` | `io_uring` | Cargo feature flags |
| `$6` or `--cores` | `1` | Runtime cores per server |

## Procedure

1. **Build all three binaries** in OrbStack:
   ```
   orb cargo build --release --example http_echo --features $FEATURES
   orb cargo build --release --example tokio_echo --features $FEATURES
   orb cargo build --release --example monoio_echo --features $FEATURES
   ```
   If any build fails, skip that runtime and note it in the output.

2. **For each runtime** (miniss → tokio → monoio):
   a. Kill any existing server on its port
   b. Start server in background:
      - miniss: `orb ./target/release/examples/http_echo --addr 127.0.0.1:$PORT --cores $CORES`
      - tokio: `orb ./target/release/examples/tokio_echo --addr 127.0.0.1:$PORT`
      - monoio: `orb ./target/release/examples/monoio_echo --addr 127.0.0.1:$PORT`
   c. Wait 2s for startup
   d. Run wrk: `wrk -t$THREADS -c$CONNS -d$DURATION http://127.0.0.1:$PORT/echo`
   e. Kill server
   f. Record: runtime name, req/s, latency avg/p50/p99, socket errors

3. **Output comparison table**:
   ```
   | Runtime   | Req/s    | Latency avg | p50    | p99    | Errors |
   |-----------|----------|-------------|--------|--------|--------|
   | miniss    | 567,000  | 175µs       | 108µs  | 1.1ms  | 0      |
   | tokio     | 450,000  | 222µs       | 180µs  | 1.5ms  | 0      |
   | monoio    | 580,000  | 172µs       | 100µs  | 900µs  | 0      |
   ```

## Example invocations

```bash
# Default comparison
compare-runtimes

# High concurrency, longer duration
compare-runtimes --conns 800 --duration 30s

# 4 cores
compare-runtimes --cores 4 --conns 200
```

## Notes

- Requires all three examples to compile. `tokio_echo` and `monoio_echo` must exist in `examples/`.
- Each runtime runs on a separate port to avoid conflicts.
- Servers are always killed after each run.
- All runs happen in OrbStack Linux VM for io_uring support.
- If a binary doesn't exist, skip it and note in output (don't fail the whole comparison).
