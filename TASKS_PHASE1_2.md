# Phase 1.2 Task Checklist

This file tracks the compilation errors and audit findings that need to be addressed.

## Compilation Errors

- [ ] **`src/cpu.rs`**: `set_cpu_affinity` called on non-Linux platforms.
- [ ] **`src/multicore.rs`**: `cannot move out of type MultiCoreRuntime` which implements the `Drop` trait.

## Warnings

- [ ] **`src/task.rs`**: Unused import `crate::cpu::CrossCpuMessage`.
- [ ] **`src/cpu.rs`**: Unused import `unbounded`.
- [ ] **`src/multicore.rs`**: Unused mutable variable `handles`.

## Audit Findings

- [ ] **Issue #1**: Implement `Drop` for `MultiCoreRuntime` for graceful shutdown.
- [ ] **Issue #3**: Use bounded channels for back-pressure control.
- [ ] **Issue #5**: Improve panic handling in multicore `block_on`.
- [ ] **Issue #6**: Use a global atomic counter for robust task ID generation.

