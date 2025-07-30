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

### Phase 4: Timer & Signals (Week 4)
- [ ] Timer wheel implementation
- [ ] Signal handling integration
- [ ] Timeout futures
- [ ] Periodic tasks

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

## Development Environment
- Linux VM/Container for io-uring testing
- macOS fallback using kqueue
- Continuous benchmarking with criterion
