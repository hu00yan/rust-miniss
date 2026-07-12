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
9. The former `hyper_http2_connection_runs_on_miniss` test encoded a
   cross-thread `Send` requirement that conflicts with shard-local
   `AsyncTcpStream`; it was removed rather than weakening the ownership
   contract. The QUIC/H3 test is now explicitly ignored because it has no
   bounded Linux-only harness and can outlive the macOS test runner. The async
   file regression is also ignored on macOS after its one-second completion
   bound exposed an epoll wakeup gap; Linux remains the required backend gate.
   `just test-fast` remains deterministic.
10. `cargo test --all-targets` also launches benchmark binaries; one of them
    starts a listening HTTP server and can consume a full CPU while waiting for
    clients. The default `just test` recipe now uses `--tests --examples` and
    leaves benchmarks opt-in.
11. The bounded QUIC/H3 exchange still times out after five seconds on both
    macOS kqueue and Linux io_uring. The test remains explicitly ignored on all
    platforms; this is an integration defect, not evidence that QUIC/H3 works.
