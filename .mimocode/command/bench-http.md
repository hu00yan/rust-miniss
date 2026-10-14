---
description: "Build http_echo, run server in OrbStack Linux VM, benchmark with wrk, output results, and clean up. Parameterized by connections, duration, threads, port, and feature flags."
---

# bench-http — Build, run, and benchmark http_echo

Build the http_echo example (optionally in OrbStack for io_uring), start the server, run wrk, collect results, and kill the server.

## Usage

```
bench-http [OPTIONS]
```

### Options (positional or flag-style)

| Arg | Default | Description |
|-----|---------|-------------|
| `$1` or `--conns` | `100` | wrk connection count |
| `$2` or `--duration` | `10s` | wrk duration |
| `$3` or `--threads` | `4` | wrk thread count |
| `$4` or `--port` | `18080` | server listen port |
| `$5` or `--features` | `io_uring` | Cargo feature flags (empty = default features) |
| `$6` or `--linux` | `true` | Run in OrbStack Linux VM (`orb`) |
| `$7` or `--cores` | `1` | Number of runtime cores (--cores N flag to http_echo) |

## Procedure

1. **Kill any existing server** on the target port:
   ```
   pkill -9 -f "http_echo.*--addr.*:$PORT" 2>/dev/null; sleep 1
   ```

2. **Build** (inside OrbStack if `--linux` is true):
   - If `--features` is non-empty:
     ```
     orb cargo build --release --example http_echo --features $FEATURES
     ```
   - Else:
     ```
     orb cargo build --release --example http_echo
     ```

3. **Start server** in background:
   ```
   orb ./target/release/examples/http_echo --addr 127.0.0.1:$PORT --cores $CORES 2>/tmp/http_echo_$PORT.log &
   SERVER_PID=$!
   sleep 2
   ```

4. **Verify server is alive**:
   ```
   echo "test" | nc -w 2 127.0.0.1 $PORT || { echo "FAIL: server not responding"; kill $SERVER_PID 2>/dev/null; exit 1; }
   ```

5. **Run wrk**:
   ```
   wrk -t$THREADS -c$CONNS -d$DURATION http://127.0.0.1:$PORT/echo
   ```

6. **Kill server**:
   ```
   kill $SERVER_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null
   ```

## Example invocations

```bash
# Default: 100 conns, 10s, 4 threads, io_uring in OrbStack
bench-http

# 800 connections, 30 seconds
bench-http --conns 800 --duration 30s

# macOS (no OrbStack), custom port
bench-http --linux false --port 9090 --features ""

# High concurrency, 8 cores
bench-http --conns 1000 --cores 8 --duration 20s
```

## Output

wrk stdout is the output. The command captures and returns it. On failure (build error, server crash), it prints the error and exits non-zero.

## Notes

- All benchmarks MUST run in OrbStack Linux VM for io_uring support. macOS uses kqueue/mio.
- Server is always killed in cleanup, even on error.
- `pkill -9` is used to ensure no leftover processes.
