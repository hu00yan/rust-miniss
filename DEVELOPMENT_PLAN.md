# Rust-Miniss Development Plan

## Project Overview
A Rust implementation of miniss (toy Seastar) - a high-performance async runtime with shared-nothing architecture.

## Architecture Principles
1. **Shared-Nothing**: Each CPU core runs its own event loop
2. **Lock-Free Communication**: Cross-CPU communication via SPSC queues
3. **Zero-Copy I/O**: Direct memory access with io-uring
4. **Minimal Allocations**: Pre-allocated buffers where possible

## Development Phases

### Phase 1: Core Foundation (Week 1)
- [ ] Basic project structure
- [ ] Custom Future implementation
- [ ] Simple single-threaded executor
- [ ] Task queue and scheduling

### Phase 2: Multi-Core Support (Week 2)
- [ ] Per-CPU executors
- [ ] Cross-CPU message passing (crossbeam SPSC)
- [ ] CPU affinity and thread pinning
- [ ] Work submission across CPUs

### Phase 3: I/O Integration (Week 3)
- [ ] io-uring wrapper for Linux
- [ ] Fallback to epoll/kqueue for testing
- [ ] Async file operations
- [ ] Buffer management

### Phase 4: Timer 6 Signals (Week 4)
- [ ] (P4-1) Timer wheel implementation
- [ ] (P4-2) Signal handling integration
- [ ] (P4-3) Timeout futures
- [ ] (P4-4) Periodic tasks

### Phase 5: Polish & Performance (Week 5)
- [ ] Benchmarks vs tokio/async-std
- [ ] Memory pool optimization
- [ ] Documentation
- [ ] Examples

## Key Components

### 1. Future/Promise
```rust
// Simplified custom future implementation
pub struct Future<T> {
    state: Arc<Mutex<FutureState<T>>>,
}

pub struct Promise<T> {
    state: Arc<Mutex<FutureState<T>>>,
}
```

### 2. CPU Executor
```rust
pub struct Cpu {
    id: usize,
    task_queue: VecDeque<Task>,
    io_ring: Option<IoUring>,
    timer_wheel: TimerWheel,
}
```

### 3. Cross-CPU Queue
```rust
pub struct CrossCpuQueue {
    tx: Producer<Message>,
    rx: Consumer<Message>,
}
```

### 4. I/O Subsystem
```rust
pub struct IoUringBackend {
    ring: IoUring,
    submissions: Slab<IoCompletion>,
}
```

## Coding Guidelines
1. **Safety First**: Use unsafe only when necessary and well-documented
2. **Zero-Cost Abstractions**: Measure performance impact of abstractions
3. **Clear Error Handling**: Use Result<T, E> everywhere
4. **Comprehensive Tests**: Unit tests for each module
## Benchmarking Goals

- Future creation/completion: < 50ns
- Cross-CPU message: < 200ns
- Task scheduling: < 100ns
- File I/O setup: < 1μs

## Current Benchmark Snapshot

- Minimal HTTP/1.1 echo server (localhost, macOS):
  - Requests/sec: 21978.09
  - p99 latency: N/A (wrk run didn’t expose p99)
- Reproduction steps and details in docs/benchmarks/http.md

## Development Environment

### Containerized Development (Recommended)
Since io-uring is Linux-only, use containers for development on any platform:

```bash
# Quick start with the development script
./dev.sh build    # Build the container
./dev.sh shell    # Start interactive development
./dev.sh check    # Quick compilation check
./dev.sh test     # Run tests
```

### Manual Docker Setup
If you prefer manual control:

```bash
# Build and run the development container
docker-compose build
docker-compose run --rm rust-miniss bash

# Inside container, you have full Rust toolchain + io-uring support
cargo check
cargo test --features=multicore
```

### Platform-Specific Testing
- **Linux (container)**: Full io-uring backend testing
- **macOS (native)**: Development best with containerized Linux environment
  ```bash
  # Focus on io-uring backend
  cargo test --features=multicore
  ```

### Development Focus
We are prioritizing `io-uring` due to its high performance.
Leverage development environments like OrbStack or Docker for Linux testing.
```bash
# Build with the default high-performance io-uring backend
cargo build 

# Enable multicore support for testing
cargo build --features=multicore
```
