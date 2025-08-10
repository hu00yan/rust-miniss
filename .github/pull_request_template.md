# Pull Request Checklist and Review Guide

Thank you for your contribution! Please use this checklist to help reviewers focus on critical areas and ensure overall quality.

Summary
- What does this PR change?
- Why is it needed?
- Any user-facing changes or migration notes?

Risk Assessment
- Performance-sensitive paths touched?
- Concurrency or cross-CPU interactions?
- Unsafe Rust involved?
- Platform-specific behavior (epoll/kqueue/io-uring)?

Critical Areas (call out explicitly if applicable)
- Timers: scheduling accuracy, cancellation semantics, overflow handling, clock source usage.
- Cross-CPU queues: cache contention, memory ordering, wake-up mechanisms, bounded/unbounded behavior.
- Unsafe blocks: invariants, aliasing guarantees, lifetimes/pinning, FFI boundaries, UB hazards.
- io-uring backend: submission/completion handling, SQ/CQ overflow, poll vs. fixed files, cancellation.

Testing
- Unit tests added/updated?
- Integration tests and regressions covered?
- Benchmarks added/updated (if performance-affecting)?

Checklists
- Code formatted (cargo fmt) and lint-clean (cargo clippy --all-targets --all-features -D warnings)
- Dependencies validated (cargo deny check)
- Minimum diff coverage met (>= 80%)

Additional Notes
- Follow-ups or TODOs
- Related issues/PRs

