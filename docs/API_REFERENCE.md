# Rust-Miniss API Reference

## Overview

Rust-Miniss is a high-performance async runtime implementing a shared-nothing architecture inspired by Seastar. Each CPU core runs its own event loop with lock-free cross-CPU communication.

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
use rust_miniss::MultiCoreRuntime;

let runtime = MultiCoreRuntime::new(4); // 4 CPU cores
runtime.block_on(async {
    println!("Hello from multi-core runtime!");
}).unwrap();
```

### Task Management

#### Basic Task Spawning

```rust
use rust_miniss::{spawn, block_on};

block_on(async {
    let handle = spawn(async {
        println!("Task running!");
        42
    });
    
    let result = handle.await;
    assert_eq!(result, 42);
});
```

#### Task Cancellation

```rust
use rust_miniss::{spawn, block_on};
use std::time::Duration;

block_on(async {
    let handle = spawn(async {
        timer::sleep(Duration::from_secs(1)).await;
        "completed"
    });
    
    // Cancel the task before it completes
    handle.cancel();
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
use rust_miniss::{timer, spawn};
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

#### Future Extension for Timeouts

```rust
use rust_miniss::timer::FutureExt;
use std::time::Duration;

async fn example() {
    let result = async {
        timer::sleep(Duration::from_millis(500)).await;
        "completed"
    }.with_timeout(Duration::from_secs(1)).await;
    
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

#### Basic Signal Handling

```rust
use rust_miniss::{MultiCoreRuntime, timer};
use std::time::Duration;

#[cfg(feature = "signal")]
async fn example() {
    let runtime = MultiCoreRuntime::new(2);
    
    // Spawn some long-running work
    let handle = runtime.spawn(async {
        loop {
            timer::sleep(Duration::from_millis(100)).await;
            println!("Working...");
        }
    }).unwrap();
    
    // The runtime will gracefully shutdown on SIGINT/SIGTERM
    runtime.block_on(async {
        handle.await
    }).unwrap();
}
```

### Cross-CPU Task Distribution

Tasks can be distributed across CPU cores for parallel execution.

```rust
use rust_miniss::MultiCoreRuntime;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

async fn example() {
    let runtime = MultiCoreRuntime::new(4);
    let counter = Arc::new(AtomicU32::new(0));
    
    let mut handles = Vec::new();
    
    // Spawn tasks across all CPUs
    for i in 0..8 {
        let counter = counter.clone();
        let handle = runtime.spawn(async move {
            let current = counter.fetch_add(1, Ordering::SeqCst);
            println!("Task {} executed, total: {}", i, current + 1);
        }).unwrap();
        handles.push(handle);
    }
    
    // Wait for all tasks to complete
    for handle in handles {
        handle.await;
    }
    
    assert_eq!(counter.load(Ordering::SeqCst), 8);
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
use rust_miniss::{MultiCoreRuntime, RuntimeError};

let runtime = MultiCoreRuntime::new(4);
match runtime.spawn(async { "hello" }) {
    Ok(handle) => {
        let result = handle.await;
        println!("Task completed: {}", result);
    },
    Err(RuntimeError::RuntimeShutdown) => {
        println!("Runtime is shutting down");
    },
    Err(e) => {
        println!("Error spawning task: {:?}", e);
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
rust-miniss = { version = "0.1", features = ["multicore", "signal", "timer"] }
```

### Available Features

- `multicore`: Enable multi-core runtime support
- `signal`: Enable signal handling for graceful shutdown
- `timer`: Enable timer wheel and timeout functionality
- `io-uring`: Enable io-uring backend (Linux only)
- `epoll`: Enable epoll backend (Linux)
- `kqueue`: Enable kqueue backend (macOS/BSD)

## Examples

See the `examples/` directory for complete working examples:

- `hello_world.rs`: Basic single-threaded usage
- `multicore_demo.rs`: Multi-core task distribution
- `timer_sleep.rs`: Timer and sleep functionality
- `graceful_shutdown.rs`: Signal handling
- `periodic_task_demo.rs`: Periodic task execution
- `timer_api_verification.rs`: Comprehensive timer API usage

## Safety

The runtime is designed to be memory-safe:

- Minimal unsafe code, well-documented with safety rationales
- All unsafe blocks are audited and bounded
- Lock-free data structures from `crossbeam` crate
- Proper pinning for futures and async state

## Platform Support

- **Linux**: Full support with io-uring backend
- **macOS**: Development support with kqueue backend  
- **Windows**: Limited support (contributions welcome)

For optimal performance on Linux, use the io-uring backend in a container environment.
