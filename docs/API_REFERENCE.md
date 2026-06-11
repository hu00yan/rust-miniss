# Rust-Miniss API Reference

## Overview

Rust-Miniss is a high-performance async runtime implementing a thread-per-core architecture inspired by Seastar. Each CPU core runs its own event loop with lock-free cross-CPU communication.

## Core Components

### Runtime

The runtime provides both single-threaded and multi-core execution models.

#### Single-threaded Runtime

```rust
use rust_miniss::Runtime;

let runtime = Runtime::new();
runtime.block_on(async {
    println!("Hello from single-threaded runtime!");
});
```

#### Multi-core Runtime

```rust
use rust_miniss::multicore::MultiCoreRuntime;

let runtime = MultiCoreRuntime::new(Some(4))?; // 4 CPU cores
runtime.spawn(async {
    println!("Hello from multi-core runtime!");
})?;
runtime.shutdown()?;
runtime.join()?;
```

### Task Management

#### Basic Task Spawning

```rust
use rust_miniss::{Runtime, task};

let runtime = Runtime::new();
runtime.block_on(async {
    let handle = task::spawn(async {
        println!("Task running!");
        42
    });
    
    let result = handle.await;
    assert_eq!(result, 42);
});
```

### Timer System

The timer system provides sleep, timeouts, and interval functionality.

#### Sleep

```rust
use rust_miniss::timer;
use std::time::Duration;

async fn example() {
    timer::sleep(Duration::from_millis(100)).await;
    println!("Woke up after 100ms");
}
```

#### Timeouts

```rust
use rust_miniss::timer;
use std::time::Duration;

async fn example() {
    let result = timer::timeout(Duration::from_secs(1), async {
        timer::sleep(Duration::from_millis(500)).await;
        "completed"
    }).await;
    
    match result {
        Ok(value) => println!("Completed with: {}", value),
        Err(_) => println!("Timed out!"),
    }
}
```

#### Intervals

```rust
use rust_miniss::timer::Interval;
use std::time::Duration;

async fn example() {
    let mut interval = Interval::new(Duration::from_millis(100));
    
    for _ in 0..5 {
        interval.tick().await;
        println!("Tick!");
    }
}
```

#### Periodic Tasks

```rust
use rust_miniss::task::spawn_periodic;
use std::time::Duration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

async fn example() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();
    
    let handle = spawn_periodic(Duration::from_millis(100), move || {
        let counter = counter_clone.clone();
        async move {
            let current = counter.fetch_add(1, Ordering::SeqCst);
            println!("Periodic task executed: {}", current + 1);
            
            if current >= 4 {
                return false; // Stop after 5 executions
            }
            true // Continue
        }
    });
    
    // The periodic task will run 5 times, then stop
    handle.await;
}
```

### Signal Handling

The runtime supports graceful shutdown via OS signals.

```rust
use rust_miniss::signal::SignalHandler;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

let shutdown_flag = Arc::new(AtomicBool::new(false));
let handler = SignalHandler::new(shutdown_flag.clone());
handler.start();
```

### Cross-CPU Task Distribution

Tasks can be distributed across CPU cores for parallel execution.

```rust
use rust_miniss::multicore::MultiCoreRuntime;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = MultiCoreRuntime::new(Some(4))?;
    let counter = Arc::new(AtomicU32::new(0));
    
    // Spawn tasks across all CPUs
    for i in 0..8 {
        let counter = counter.clone();
        runtime.spawn(async move {
            let current = counter.fetch_add(1, Ordering::SeqCst);
            println!("Task {} executed, total: {}", i, current + 1);
        })?;
    }
    
    runtime.shutdown()?;
    runtime.join()?;
    
    assert_eq!(counter.load(Ordering::SeqCst), 8);
    Ok(())
}
```

## Performance Characteristics

### Benchmarking Goals

- Future creation/completion: < 50ns
- Cross-CPU message: < 200ns  
- Task scheduling: < 100ns
- Timer wheel operations: < 100ns
- File I/O setup: < 1μs

### Memory Pool Optimizations

The runtime uses pre-allocated memory pools to minimize allocations:

- Task queues are pre-allocated with configurable capacity
- Timer wheel slots use pre-allocated VecDeques
- Cross-CPU channels use bounded queues for back-pressure

### Configuration

Performance can be tuned via configuration constants:

```rust
// In src/config.rs
pub const CROSS_CPU_CHANNEL_CAPACITY: usize = 1000;
pub const CPU_THREAD_TIMEOUT_MS: u64 = 10;
pub const INITIAL_TASK_QUEUE_CAPACITY: usize = 128;
pub const EXPECTED_WAKEUP_COUNT: usize = 16;
```

## Error Handling

The runtime uses `Result<T, E>` types throughout:

### Runtime Errors

```rust
use rust_miniss::error::RuntimeError;

match runtime.spawn(async { "hello" }) {
    Ok(task_id) => {
        println!("Task spawned with ID: {:?}", task_id);
    },
    Err(RuntimeError::TaskFailed(msg)) => {
        println!("Failed to spawn task: {}", msg);
    },
    Err(e) => {
        println!("Error: {:?}", e);
    }
}
```

### Timer Errors

```rust
use rust_miniss::timer::{timeout, TimeoutError};
use std::time::Duration;

let result = timeout(Duration::from_millis(100), async {
    timer::sleep(Duration::from_secs(1)).await;
    "too slow"
}).await;

match result {
    Ok(value) => println!("Completed: {}", value),
    Err(TimeoutError) => println!("Operation timed out"),
}
```

## Feature Flags

Enable specific functionality with feature flags:

```toml
[dependencies]
rust-miniss = { version = "0.1", features = ["io_uring", "signal"] }
```

### Available Features

- `io_uring`: Enable io-uring backend (Linux only)
- `epoll`: Enable epoll backend (Linux/Unix) - enabled by default
- `signal`: Enable signal handling for graceful shutdown
- `arena`: Enable arena allocator for HTTP parsing
- `rent-traits`: Enable AsyncReadRent/AsyncWriteRent traits
- `advanced`: Enable zero-copy Rent + arena
- `debug`: Enable tracing spans and backtraces

## Examples

See the `examples/` directory for complete working examples:

- `hello_world.rs`: Basic single-threaded usage
- `http_echo.rs`: HTTP/1.1 echo server
- `udp_buffered.rs`: UDP socket with buffered I/O
- `metrics_server.rs`: Prometheus metrics exporter

## Safety

The runtime is designed to be memory-safe:

- Minimal unsafe code, well-documented with safety rationales
- All unsafe blocks are audited and bounded
- Lock-free data structures from `crossbeam` crate
- Proper pinning for futures and async state

## Platform Support

- **Linux**: Full support with io-uring or epoll backend
- **macOS**: Development support with kqueue backend  
- **Windows**: Not supported (contributions welcome)

For optimal performance on Linux, use the io-uring backend in a container environment.
