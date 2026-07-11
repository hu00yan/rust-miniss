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
9. The macOS full integration run stalls in
   `hyper_http2_connection_runs_on_miniss` for more than 90 seconds. CI retains
   the pre-existing full Linux `--all-targets` job, while the local default
   remains `just test-fast`; diagnose the macOS HTTP/2 hang separately.
