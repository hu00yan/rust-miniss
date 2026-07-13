# Runtime Safety Follow-up Ledger

Issues found while designing the safety-test architecture are recorded here and
are not silently mixed into the implementation batch.

1. The old `integration_concurrency` Loom target runs a complete runtime with
   standard atomics and real timers. It is not a useful Loom protocol model and
   enters normal `cargo test`; replace it in this batch.
2. Shard-local auto-trait constraints are described in comments/compile-fail
   examples but are not uniformly asserted. Add negative assertions and minimal
   owner markers in this batch.
3. `ReadyQueue` names a cross-thread bounded notifier, not the shard-local ready
   owner. Rename/split this API in a later compatibility-aware change.
4. Task generations are `u16` and wrap. Define exhaustion/retirement semantics
   before claiming stale keys are harmless for an unbounded runtime lifetime.
5. io_uring terminal cleanup is duplicated across completion paths. Consolidate
   it later; recent follow-up fixes show that single-close handling can drift.
6. Resolved in this batch: the old `bench-smoke` ran twice, parsed arbitrary
   digits, and treated `HEAD~1` as a baseline. It now performs one HTTP run,
   requires matching host/workload metadata, and compares throughput plus p99.
   A fresh same-host Linux baseline still has to be measured before the gate can
   make a performance claim.
7. Runtime termination does not expose one mechanically asserted resource-zero
   snapshot. Add owner-local counters/reporting before treating shutdown drain
   as fully verified.
8. Two legacy cancellation integration files raced immediately completing
   tasks and expected fire-and-forget success for nonexistent ids. They were
   removed: owner acknowledgement is already covered deterministically by
   `multicore::tests::cancel_reports_owner_acknowledgement` and
   `task::tests::join_handle_cancel_waits_for_owner_poll_ack`.
9. The HTTP/2 regression is covered by `tests/hyper_http2.rs` with a bounded
   Hyper/h2 exchange. The QUIC/H3 test remains explicitly ignored while the
   macOS kqueue path is repaired; Linux has a bounded passing exchange. The
   async file regression remains ignored on macOS after its one-second bound
   exposed a kqueue wakeup gap. `just test-fast` remains deterministic.
10. `cargo test --all-targets` also launches benchmark binaries; one of them
    starts a listening HTTP server and can consume a full CPU while waiting for
    clients. The default `just test` recipe now uses `--tests --examples` and
    leaves benchmarks opt-in.
11. The bounded QUIC/H3 exchange passes on Linux io_uring but still times out
    on macOS kqueue. Two completion defects are fixed: QUIC readiness now
    consumes owner-local fused completions, and non-Linux backends route their
    completion queue through the owner-task protocol. The UDP wrapper also sets
    sockets nonblocking. The remaining macOS issue is kqueue readiness delivery
    and must not be treated as verified until the ignored test passes there.
