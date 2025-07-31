# Phase-3 Architecture & API Specification

**Date:** December 2024  
**Project:** rust-miniss  
**Scope:** Phase 3 - I/O Integration Architecture  
**Performance Target:** < 1 µs I/O setup budget

## Executive Summary

Phase 3 introduces high-performance I/O capabilities to the rust-miniss runtime while maintaining the shared-nothing architecture established in Phases 1-2. The design centers around an abstract `IoBackend` trait with platform-specific implementations, zero-copy buffer management, and integration with the existing CPU event loop.

## 1. IoBackend Trait Architecture

### Core Interface

```rust
/// Trait for platform-specific I/O backends
pub trait IoBackend: Send + Sync {
    type IoHandle: IoHandleOps;
    type Error: std::error::Error + Send + Sync + 'static;
    
    /// Submit an I/O operation for execution
    /// Returns immediately with a handle for tracking/cancellation
    fn submit(&mut self, op: IoOperation) -> Result<Self::IoHandle, Self::Error>;
    
    /// Cancel a previously submitted operation
    /// Best-effort cancellation - may still complete
    fn cancel(&mut self, handle: &Self::IoHandle) -> Result<bool, Self::Error>;
    
    /// Poll for completion events (non-blocking)
    /// Returns completed operations with their results
    fn poll(&mut self, timeout: Option<Duration>) -> Result<Vec<IoCompletion>, Self::Error>;
    
    /// Get the maximum batch size for efficient polling
    fn max_batch_size(&self) -> usize { 128 }
}

/// Operations supported by the I/O backend
#[derive(Debug)]
pub enum IoOperation {
    Read {
        fd: RawFd,
        buffer: BufferHandle,
        offset: u64,
        len: usize,
    },
    Write {
        fd: RawFd,
        buffer: BufferHandle,
        offset: u64,
        len: usize,
    },
    Accept {
        fd: RawFd,
    },
    Connect {
        fd: RawFd,
        addr: SocketAddr,
    },
}

/// Completion event from I/O backend
#[derive(Debug)]
pub struct IoCompletion {
    pub handle: IoHandleId,
    pub result: Result<usize, std::io::Error>,
    pub buffer: Option<BufferHandle>,
}
```

### Performance Characteristics

- **submit():** Target < 100ns per operation
- **poll():** Target < 500ns for empty poll, < 2µs for batch processing
- **cancel():** Target < 200ns (best-effort)

## 2. IoHandle / IoFuture State Machine

### State Transitions

```rust
/// Handle for tracking I/O operations
pub struct IoHandle {
    id: IoHandleId,
    state: Arc<Mutex<IoState>>,
    waker: Option<Waker>,
}

/// I/O operation states
#[derive(Debug, Clone)]
enum IoState {
    /// Operation submitted to backend
    Submitted { submitted_at: Instant },
    /// Operation completed with result
    Completed { result: Result<usize, std::io::Error> },
    /// Operation was cancelled
    Cancelled,
    /// Operation failed during submission
    Failed { error: std::io::Error },
}

/// Future that resolves when I/O completes
pub struct IoFuture {
    handle: IoHandle,
    buffer: Option<BufferHandle>,
}

impl Future for IoFuture {
    type Output = Result<(usize, BufferHandle), std::io::Error>;
    
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = self.handle.state.lock().unwrap();
        match &*state {
            IoState::Completed { result } => {
                match result {
                    Ok(bytes) => Poll::Ready(Ok((*bytes, self.buffer.take().unwrap()))),
                    Err(e) => Poll::Ready(Err(e.clone())),
                }
            }
            IoState::Cancelled => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Operation cancelled"
            ))),
            IoState::Failed { error } => Poll::Ready(Err(error.clone())),
            IoState::Submitted { .. } => {
                // Register waker for completion notification
                self.handle.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
```

### Waker Flow

1. **Submission:** `IoFuture` stores waker in `IoHandle`
2. **Completion:** I/O backend updates state and calls `waker.wake()`
3. **Polling:** Future checks state and returns result or re-registers waker

## 3. BufferPool API and Memory-Layout Invariants

### API Design

```rust
/// High-performance memory pool for I/O buffers
pub struct BufferPool {
    /// Per-size class allocators
    size_classes: Vec<SizeClassAllocator>,
    /// Alignment requirement (typically 4KB for O_DIRECT)
    alignment: usize,
    /// Maximum buffer size supported
    max_size: usize,
}

impl BufferPool {
    /// Create a new buffer pool with specified configuration
    pub fn new(config: BufferPoolConfig) -> Self;
    
    /// Allocate a buffer of at least `size` bytes
    /// Returns immediately - never blocks
    pub fn allocate(&self, size: usize) -> Option<BufferHandle>;
    
    /// Return a buffer to the pool
    /// Must be called exactly once per allocated buffer
    pub fn deallocate(&self, handle: BufferHandle);
    
    /// Pre-warm the pool with buffers
    pub fn prealloc(&mut self, size_class: usize, count: usize) -> Result<(), AllocError>;
    
    /// Get pool statistics
    pub fn stats(&self) -> BufferPoolStats;
}

/// Handle to an allocated buffer
#[derive(Debug)]
pub struct BufferHandle {
    ptr: NonNull<u8>,
    size: usize,
    capacity: usize,
    pool_id: u32,
}

/// Buffer pool configuration
#[derive(Debug, Clone)]
pub struct BufferPoolConfig {
    /// Size classes: [4K, 8K, 16K, 32K, 64K, 128K, 256K, 512K, 1M]
    pub size_classes: Vec<usize>,
    /// Buffers to pre-allocate per size class
    pub prealloc_count: usize,
    /// Memory alignment (4096 for O_DIRECT compatibility)
    pub alignment: usize,
    /// Maximum total memory to use (in bytes)
    pub max_memory: usize,
}
```

### Memory-Layout Invariants

1. **Alignment:** All buffers aligned to 4KB boundaries for O_DIRECT support
2. **Contiguity:** Each buffer is a single contiguous memory region
3. **Thread Safety:** Pool supports concurrent allocation/deallocation
4. **Zero-Copy:** Buffers can be passed directly to kernel via io_uring/epoll
5. **Bounded Memory:** Total pool memory never exceeds configured limit

### Size Class Strategy

```rust
/// Standard size classes optimized for common I/O patterns
const DEFAULT_SIZE_CLASSES: &[usize] = &[
    4_096,    // 4KB - small reads
    8_192,    // 8KB
    16_384,   // 16KB
    32_768,   // 32KB
    65_536,   // 64KB - typical socket buffer
    131_072,  // 128KB
    262_144,  // 256KB
    524_288,  // 512KB
    1_048_576, // 1MB - large I/O operations
];
```

## 4. CPU Event-Loop Changes

### Enhanced Event Loop Structure

```rust
/// Updated CPU struct with I/O backend integration
pub struct Cpu {
    /// Existing fields...
    pub id: usize,
    task_queue: HashMap<TaskId, Task>,
    ready_queue: Arc<SegQueue<TaskId>>,
    message_receiver: Receiver<CrossCpuMessage>,
    
    /// New I/O integration fields
    io_backend: Box<dyn IoBackend>,
    io_handles: HashMap<IoHandleId, IoHandle>,
    buffer_pool: Arc<BufferPool>,
    
    /// I/O polling configuration
    io_poll_timeout: Duration,
    max_io_batch: usize,
}

impl Cpu {
    /// Enhanced tick method with I/O polling phase
    pub fn tick(&mut self) -> bool {
        let mut made_progress = false;
        
        // Phase 1: Process cross-CPU messages
        made_progress |= self.process_messages();
        
        // Phase 2: Poll I/O completions (NEW)
        made_progress |= self.poll_io_completions();
        
        // Phase 3: Execute ready tasks
        made_progress |= self.execute_ready_tasks();
        
        made_progress
    }
    
    /// New I/O polling phase
    fn poll_io_completions(&mut self) -> bool {
        let completions = match self.io_backend.poll(Some(Duration::from_nanos(0))) {
            Ok(completions) => completions,
            Err(e) => {
                tracing::error!("I/O polling error: {}", e);
                return false;
            }
        };
        
        let mut made_progress = false;
        for completion in completions {
            if let Some(handle) = self.io_handles.get_mut(&completion.handle) {
                // Update handle state
                {
                    let mut state = handle.state.lock().unwrap();
                    *state = IoState::Completed { result: completion.result };
                }
                
                // Wake associated future
                if let Some(waker) = handle.waker.take() {
                    waker.wake();
                }
                
                made_progress = true;
            }
        }
        
        made_progress
    }
}
```

### Performance Optimizations

- **Adaptive Polling:** Adjust I/O poll timeout based on load
- **Batch Processing:** Process multiple completions per poll cycle
- **Zero-Timeout Polling:** Non-blocking I/O polls during high activity

## 5. Conditional-Compilation Matrix

### Platform-Specific Implementations

```rust
#[cfg(target_os = "linux")]
mod io_uring_backend {
    use io_uring::{IoUring, opcode, types};
    
    pub struct IoUringBackend {
        ring: IoUring,
        submissions: Slab<PendingOp>,
        next_handle_id: AtomicU64,
    }
    
    impl IoBackend for IoUringBackend {
        // High-performance io_uring implementation
        // Target: 50-100ns submit latency
    }
}

#[cfg(target_os = "macos")]
mod kqueue_backend {
    use std::os::unix::io::RawFd;
    
    pub struct KqueueBackend {
        kq_fd: RawFd,
        events: Vec<libc::kevent>,
        pending_ops: HashMap<u64, PendingOp>,
    }
    
    impl IoBackend for KqueueBackend {
        // kqueue-based implementation
        // Target: 100-200ns submit latency
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod epoll_backend {
    pub struct EpollBackend {
        epoll_fd: RawFd,
        events: Vec<libc::epoll_event>,
        pending_ops: HashMap<RawFd, PendingOp>,
    }
    
    impl IoBackend for EpollBackend {
        // Generic epoll fallback
        // Target: 200-500ns submit latency
    }
}
```

### Feature Matrix

| Platform | Backend | Direct I/O | Batch Submit | Vectored I/O | SQPOLL |
|----------|---------|------------|--------------|--------------|--------|
| Linux    | io_uring| ✅         | ✅           | ✅           | ✅     |
| macOS    | kqueue  | ⚠️         | ❌           | ❌           | ❌     |
| Generic  | epoll   | ❌         | ❌           | ❌           | ❌     |

## 6. Performance Budget Analysis

### Setup Cost Breakdown (Target: < 1µs)

```
I/O Operation Setup:
├── Buffer allocation: 50-100ns (from pool)
├── Backend submission: 50-200ns (platform dependent)
├── Handle creation: 20-50ns
├── Waker registration: 10-30ns
└── Future creation: 10-20ns
─────────────────────────────────
Total: 140-400ns ✅ (well under 1µs budget)
```

### Memory Overhead per I/O Operation

```
Per-operation overhead:
├── IoHandle: 64 bytes
├── IoFuture: 32 bytes
├── Backend tracking: 32-128 bytes (platform dependent)
└── Waker storage: 48 bytes
─────────────────────────────
Total: 176-272 bytes per operation
```

## 7. Shared-Nothing Principle Compliance

### Architecture Validation

✅ **CPU Isolation:** Each CPU has its own I/O backend instance  
✅ **Buffer Pool Isolation:** Per-CPU buffer pools prevent contention  
✅ **No Shared Mutable State:** All I/O state is CPU-local  
✅ **Lock-Free Fast Path:** No locks in common I/O submission path  
✅ **Zero-Copy Buffers:** Direct memory access without copying  

### Cross-CPU I/O Considerations

When tasks need to perform I/O on a different CPU:

```rust
/// Cross-CPU I/O message
pub enum CrossCpuMessage {
    /// Existing variants...
    
    /// Submit I/O operation to target CPU
    SubmitIo {
        operation: IoOperation,
        completion_cpu: usize,
        completion_task: TaskId,
    },
}
```

This maintains shared-nothing by:
1. I/O always executed on the target CPU
2. Results sent back via message passing
3. No shared I/O state between CPUs

## 8. Implementation Roadmap

### Phase 3.1: Core Infrastructure (Week 1)
- [ ] `IoBackend` trait definition
- [ ] `BufferPool` implementation
- [ ] Basic `IoHandle`/`IoFuture` types
- [ ] CPU event loop integration

### Phase 3.2: Platform Backends (Week 2)
- [ ] Linux io_uring backend
- [ ] macOS kqueue backend
- [ ] Generic epoll fallback
- [ ] Conditional compilation setup

### Phase 3.3: Integration & Testing (Week 3)
- [ ] File I/O operations
- [ ] Socket I/O operations
- [ ] Cross-CPU I/O message passing
- [ ] Performance benchmarks
- [ ] Error handling and edge cases

### Phase 3.4: Optimization (Week 4)
- [ ] Buffer pool tuning
- [ ] I/O batching optimizations
- [ ] Memory usage optimization
- [ ] Performance validation against < 1µs budget

## 9. Testing Strategy

### Unit Tests
- Buffer pool allocation/deallocation
- I/O backend submission/polling
- State machine transitions
- Error handling scenarios

### Integration Tests
- File read/write operations
- Socket accept/connect operations
- Cross-CPU I/O coordination
- Graceful shutdown with pending I/O

### Performance Tests
- I/O setup latency measurement
- Memory usage validation
- Throughput benchmarks vs. tokio/async-std
- Platform-specific feature validation

## 10. Risk Mitigation

### High-Risk Areas
1. **Buffer Pool Contention:** Mitigated by per-CPU pools
2. **I/O Backend Reliability:** Comprehensive error handling
3. **Memory Leaks:** RAII patterns and Drop implementations
4. **Performance Regression:** Continuous benchmarking

### Fallback Strategies
- Platform detection and graceful degradation
- Configurable buffer pool parameters
- Optional direct I/O support
- Conservative default timeouts
