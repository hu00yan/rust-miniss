# Project Purpose, Positioning, and Usage

Why this project exists (the "why"):
- Explore shared-nothing, per-CPU runtime design in Rust, inspired by Seastar/miniss.
- Learn-by-building: demystify async internals (schedulers, timers, cross-CPU messaging).
- Provide a small, understandable codebase to experiment with high-performance runtime ideas without the weight of a full framework.

What it is (today):
- A minimal async runtime prototype with:
  - Multi-core runtime (thread-per-core architecture) with shared-nothing design
  - Timer utilities (sleep, timeout, interval, periodic tasks)
  - Async networking (TCP/UDP with non-blocking I/O using epoll/io-uring)
  - HTTP echo server example with performance optimizations
- Educational tool for understanding async runtime internals and high-performance I/O

What it is not:
- A production-grade runtime (missing advanced features, optimizations)
- Drop-in replacement for Tokio/Async-std
- Comprehensive I/O stack (limited platform support, features)

How to use (practical today):
- As a lightweight runtime for small experiments:
  - Multi-core: use `Runtime::new_multi_core()` for thread-per-core execution.
  - Timers: compose `sleep`, `timeout`, and `Interval` for scheduling.
  - Periodic work: `task::spawn_periodic` for recurring jobs.
- As a reference for architecture:
  - Study per-CPU executors, cross-CPU messaging, timer wheels, and async I/O backends.
  - Compare performance with Tokio/Monoio/Glommio using included benchmarks.
- As a networking playground:
  - Use `examples/http_echo.rs` as a template for TCP servers and measure performance.
  - Experiment with UDP operations and buffer management.

Benchmarks & environment:
- Includes HTTP benchmark comparing rust-miniss vs Tokio/Monoio/Glommio.
- Run `./bench/scripts/bench_http.sh` for automated performance testing.
- Authoritative benchmarks should be run on Linux with io_uring enabled; record CPU, memory, OS, and Rust version alongside results for reproducibility.
- Recent optimizations reduced CPU usage in event loops and improved I/O efficiency.

Roadmap alignment (why this direction):
- Validate design assumptions (shared-nothing, lock-free fast paths) with small, measurable slices.
- Keep scope tight to learn faster: correctness first, then targeted performance measurements.
- Use examples (like HTTP echo) to ground performance conversations in runnable code.

Where it can go (if we continue):
- Expand I/O backends and buffer management for real workloads.
- Improve multicore scheduling and load balancing heuristics.
- Add profiling/metrics hooks to validate performance budgets.

If you are evaluating this project:
- Treat it as a learning artifact and a starting point for experiments.
- Expect APIs to evolve; prefer reading `docs/API_REFERENCE.md` and examples for up-to-date usage.

