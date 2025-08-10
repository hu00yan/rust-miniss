# Project Purpose, Positioning, and Usage

Why this project exists (the "why"):
- Explore shared-nothing, per-CPU runtime design in Rust, inspired by Seastar/miniss.
- Learn-by-building: demystify async internals (schedulers, timers, cross-CPU messaging).
- Provide a small, understandable codebase to experiment with high-performance runtime ideas without the weight of a full framework.

What it is (today):
- A minimal async runtime prototype with:
  - Per-CPU executors (feature-gated) and cross-CPU message passing.
  - Timer utilities (sleep, timeout, interval, periodic tasks).
  - Example networking path (HTTP echo) demonstrating end-to-end async flow.
- Not a drop-in replacement for Tokio/Seastar; rather a focused learning and experimentation tool.

What it is not (yet):
- A production-grade runtime or a full port of miniss into Rust.
- A comprehensive I/O stack with all platform features (e.g., advanced io_uring usage in production settings).

How to use (practical today):
- As a lightweight runtime for small experiments:
  - Single-threaded: drive async code with `Runtime::new().block_on(...)`.
  - Timers: compose `sleep`, `timeout`, and `Interval` for scheduling.
  - Periodic work: `task::spawn_periodic` for recurring jobs.
- As a reference for architecture:
  - Study per-CPU executors, message passing, and timer wheel structure.
  - Compare call stacks with Tokio to understand design trade-offs.
- As a networking playground:
  - Use `examples/http_echo.rs` as a template for simple TCP protocols and measure baseline behavior.

Benchmarks & environment:
- We record quick local numbers for development iteration.
- Authoritative benchmarks should be run on Linux with io_uring enabled; record CPU, memory, OS, and Rust version alongside results for reproducibility.

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

