# Rust-Miniss Startup Prompt

## 🚀 Let's Build a High-Performance Async Runtime!

### Project Context
We're building **rust-miniss**, a Rust port of the miniss project (a toy Seastar implementation). Our goal is to create a minimal but functional async runtime that demonstrates:
- Shared-nothing architecture (one thread per CPU core)
- Lock-free cross-CPU communication
- io-uring for high-performance I/O (Linux)
- Custom Future/Promise implementation

### Key Design Decisions Made
1. **io-uring over AIO**: Better performance, more modern API
2. **Crossbeam for concurrency**: Production-ready lock-free data structures
3. **Custom Future impl**: To understand the internals (not just use std::future)
4. **Minimal scope**: ~2000 lines for MVP, focusing on core concepts

### Starting Point
Let's begin with the foundation - a simple Future/Promise implementation that we'll build upon.

```rust
// Our first task: Implement a basic Future/Promise pair
// This will be the building block for everything else

pub struct Future<T> {
    // What state do we need?
}

pub struct Promise<T> {
    // How do we complete a future?
}

impl<T> Future<T> {
    pub fn new() -> (Future<T>, Promise<T>) {
        // Create a linked pair
    }
}
```

### Development Philosophy
1. **Start Simple**: Get a working single-threaded executor first
2. **Test Everything**: Write tests as we go
3. **Measure Performance**: Use criterion for benchmarks
4. **Document Decisions**: Explain why, not just what

### First Milestone
A working example that can:
```rust
fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("Hello from rust-miniss!");
    });
}
```

### Questions to Keep in Mind
- How does Waker work in our custom implementation?
- How do we handle task scheduling efficiently?
- What's the minimal API surface we need?

### Ready? Let's Code! 🦀
Start by creating the basic module structure:
```
src/
├── lib.rs          // Public API
├── future.rs       // Future/Promise implementation
├── executor.rs     // Single-threaded executor
├── task.rs         // Task abstraction
└── waker.rs        // Waker implementation
```

**Your move!** What would you like to tackle first?
