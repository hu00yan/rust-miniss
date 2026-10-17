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
   Hyper/h2 exchange. The QUIC/H3 exchange is now bounded and runs on both
   macOS kqueue and Linux io_uring. The async file regression is bounded and
   runs on both supported backends.
   `just test-fast` remains deterministic.
10. `cargo test --all-targets` also launches benchmark binaries; one of them
    starts a listening HTTP server and can consume a full CPU while waiting for
    clients. The default `just test` recipe now uses `--lib --tests --examples` and
    leaves benchmarks opt-in.
11. The bounded QUIC/H3 exchange passes on macOS kqueue and Linux io_uring.
    The fixes include owner-local fused completion consumption, portable
    non-Linux completion routing, explicit `PollAdd` completion handling in the
    mio backend, nonblocking UDP sockets, and macOS UDP write-readiness
    handling that avoids unsupported kqueue write filters.
    The current manual evidence is recorded in
    `docs/review/protocol-integration-2026-07-13.md`; there is no GitHub CI
    workflow in this repository, so these platform checks must be rerun when
    the protocol or backend changes.
12. A stale Linux process was found running
    `runtime_regressions::multicore_spawned_async_file_future_completes` for
    more than 12 hours at nearly 100% CPU. The test used to panic directly on
    its receive timeout; unwinding dropped the runtime before the
    force-shutdown path and could block forever joining an io_uring worker.
    Commit `671fd7d` now records the timeout, performs bounded
    shutdown/escalation, and joins before asserting. The bounded test now runs
    on both supported backends; the macOS run has been explicitly validated.
13. The current worktree was rerun on Orb Ubuntu with kernel
    `7.0.11-orbstack-00360-gc9bc4d96ac70`. `sh scripts/linux_safety.sh` passed
    all 22 exact io_uring cases, and the `just stress` recipe's direct Cargo
    command passed all 8 stress tests in 168.03 seconds with no leftover test
    processes. The VM does not have `just`, so the stress recipe was executed
    by its equivalent Cargo command; this is execution evidence, not a claim
    that the VM has the local gate wrapper installed. The same VM's nightly
    Miri run passed all 8 named tests; the executor TLS test took 154.28
    seconds, so Miri remains an explicit slow gate rather than part of CI's
    fast path.
14. The multi-shard QUIC regression now accepts `MINISS_QUIC_CONNECTIONS` for
    explicit pressure probes while retaining 32 as the default. An initial 64
    connection failure was a test lifecycle bug: each client dropped its
    connection after a fixed 200 ms sleep while the server was still draining
    the other streams. Each client task now holds its connection until the
    server publishes the read-complete flag. The pressure test also spaces
    launches above 128 connections and uses the same I/O deadline for connect,
    so the test does not manufacture UDP-burst or premature-client-close
    failures. Orb Linux passed 64, 128, 256, and 512 connections in three
    consecutive runs each. Higher sustained counts and control-thread endpoint
    rotation remain open.
15. The escaped-waker retirement regression now bounds its adversarial wake
    producer; the previous unbounded loop could flood the ready queue and hang
    a feature-matrix process. Default and `rent-traits` exact runs pass, and
    the complete serial feature matrix passes without ignored tests or leftover
    processes.
16. Current explicit safety gates are green: all three production Loom models,
    the unsafe-contract audit, the architecture audit, Orb Linux `linux-safety`,
    and all eight named Orb Linux Miri tests. macOS Miri remains unavailable
    locally because the nightly macOS target is not installed.
17. Added a bounded four-worker UDP receive/shutdown race with 64 owner-local
    sockets and concurrent datagram bursts. It passed on macOS and in three
    consecutive Orb Linux runs, with a zero termination snapshot; longer
    sustained UDP/QUIC close-race pressure remains separate evidence.
18. The HTTP connection integration matrix now also sends conflicting
    `Content-Length` headers over a real TCP connection and verifies bounded
    rejection/cleanup on both macOS and Linux.
19. QUIC now has a bounded handshake-refusal cleanup regression. The server
    explicitly refuses and closes the endpoint; the client connect remains
    bounded and the runtime terminates cleanly on both macOS and Orb Linux.
20. HTTP pure parsing/serialization now has 64-case property coverage for
    arbitrary byte input and arbitrary response bodies, in addition to the
    deterministic malformed-header and connection-lifecycle cases.
21. A default-gate run exposed that the escaped-waker regression asserted
    retirement after a fixed tick count while its bounded producer was still
    publishing wakes. The test now synchronizes on producer completion and
    drains the resulting stale notifications before asserting; ten repeated
    exact runs and `just test-fast` pass. Linux safety also now executes the
    512-connection QUIC pressure test and 64-socket UDP close-race three times
    each instead of relying only on manual pressure evidence.
22. HTTP lifecycle coverage now includes a real TCP request whose declared
    body is truncated before EOF; the server must return `UnexpectedEof` and
    finish within the bounded lifecycle timeout. The exact regression passes
    on the current backend and is included in the Linux safety command.
23. The minimal HTTP parser now rejects `Transfer-Encoding` explicitly instead
    of treating a chunked body as a zero-length request. Both the pure parser
    test and a bounded TCP lifecycle test cover this unsupported framing path.
24. io_uring now has a production-backend test with `drain_batch_size=1`; four
    terminal NOP completions are drained one at a time and all operation state
    must be reclaimed. The exact test passes on Orb Linux and is part of the
    Linux safety command. The suite deliberately targets the supported backend
    contract rather than historical or undocumented kernel ordering.
25. TLS coverage now includes a trusted certificate with an IP subject
    alternative name and a `127.0.0.1` ServerName, in addition to the existing
    DNS-name, wrong-name, private-root, and EOF cases. Public CA and platform
    root-store differences remain environment-dependent evidence.
26. `bench_http_matrix.sh` now preserves every requested round through
    `BENCH_MATRIX_ROUNDS`, and `just bench-matrix` exposes it explicitly. A
    five-round Orb Linux ka32/close32 comparison produced zero request/process
    errors; ka32 passed all runtime stability gates, while close32 correctly
    rejected the glommio CV outlier instead of hiding it.
27. Added `bench_http_topology.sh`/`just bench-topology` to expand worker and
    client-thread combinations while requiring explicit non-overlapping CPU
    placement. Orb Linux verified the 1-worker/1-thread ka8/close8 smoke; the
    full isolated topology matrix requires a host with enough server and
    client CPUs, so the eight-worker case is not claimed from the 8-vCPU VM.
    The same VM has now completed worker `1/2/4` × client threads `1/2/4`
    ka8/close8 smoke coverage: 90 runtime samples with zero request/process
    errors. These are topology coverage samples, not multi-round rankings.
    A further worker `1/2/4` × thread `1` ka200/close32 smoke added 30 high-load
    samples, also with zero request/process errors; topology-dependent rankings
    remain separate from the five-round 4-worker baseline.
28. An opt-in metrics smoke collected RSS successfully (509,916 KiB), while
    Orb denied perf syscall tracepoints. The raw perf error is retained rather
    than converted to zeroes; a privileged Linux environment is still needed
    for task-clock and syscall-counter evidence.
29. HTTP request framing now validates header names before using `Content-Length`
    and rejects versions other than HTTP/1.1. Pure parser tests and a bounded TCP
    lifecycle test cover invalid header names; broader protocol-error matrices
    remain separate work.
30. The new `just test-full` entrypoint completed on macOS: the portable feature
    matrix, all library/integration/example tests, differential models, Loom,
    and the bounded stress gate passed with no ignored tests. Linux io_uring and
    nightly Miri were reported as explicit skips because this host cannot run them.
31. Orb Ubuntu kernel `7.0.11-orbstack-00360-gc9bc4d96ac70` passed
    `scripts/linux_safety.sh` and the all-features Linux suite (format, no-ignored,
    all-target clippy, and all library/integration/example tests). This is evidence
    for the currently supported Linux/io_uring environment; the suite does not
    promise compatibility with undocumented kernel behavior.
32. The same Orb environment reran all eight named nightly Miri tests. Every test
    passed; the executor TLS-context case took 154.28 seconds. Miri remains an
    explicit slow gate and does not replace real io_uring testing.
33. A 1024-connection QUIC pressure run first exposed a real shutdown race:
    Quinn could call the runtime adapter after admission closed, and the local
    `expect` aborted the process. The adapter now logs rejected local admission,
    and the pressure test closes each endpoint and waits for `wait_idle()` before
    runtime shutdown. Re-running 1024 now returns a bounded `accept` timeout
    instead of aborting; 128/256/512 pass on the current Orb ARM64 kernel, so
    1024 remains an unverified capacity target rather than a claimed guarantee.
34. The portable full-suite rerun found a shutdown accounting race: a duplicate
    `CoreMessage::Shutdown` could remain in a bounded inbox after the worker had
    already exited from the direct lifecycle flag. Normal shutdown now uses the
    flag plus the existing notifier only; the message is not enqueued. The
    affected 1/2/4-worker × 16/64-task matrix passes five consecutive rounds.
35. The current worktree now passes `just check` with the regular test set,
    `just test-feature-matrix` with seven feature compilation combinations plus
    one all-feature regular regression, and `just loom` with all three
    production synchronization models. The feature matrix runs no stress tests.
36. The explicit `just stress` gate passes all nine stress targets, including
    loaded shutdown, UDP close-race, retained TCP/UDP lease, and long-running
    pressure coverage. It took 152.66 seconds; this cost is intentionally kept
    in the explicit stress gate rather than repeated for every feature.
37. The complete `just test-full` orchestration now passes on macOS. It includes
    the regular gate, feature compilation matrix, all-feature regular tests,
    production-linked differential models, all three Loom models, and one stress
    run; Linux io_uring and nightly Miri are reported as explicit skips on this
    host.
38. HTTP integration coverage now includes two requests sent in one TCP write.
    `HttpConnection::read_request` must retain and drain the second request from
    its connection buffer; the bounded test also verifies both responses and
    clean EOF. The updated `just test-full` run passed with 10 HTTP integration
    tests and no ignored tests or leftover processes.
39. The Linux `/root/gate.sh` sequence (OrbStack Ubuntu, kernel
    `7.2.9-orbstack-00391-g2b298d05fbd4`, 8 vCPU) reported one intermittent
    failure from `scripts/linux_safety.sh`: the 512-connection QUIC round
    `MINISS_QUIC_CONNECTIONS=512 cargo test --all-features --test quinn_h3
    quinn_endpoints_run_on_separate_owned_shards -- --exact` aborted after 8.19s
    with `aborted by peer: the application or application protocol caused the
    connection to be closed during the handshake`. The same command passed the
    other round in that run (34.84s) and passed 3/3 standalone rounds afterwards
    (36.51s / 36.57s / 37.57s); the unloaded single-connection variant passed
    5/5 (1.52-2.55s). This is a load-sensitive handshake abort at the documented
    512-connection pressure point, so the failing constraint is CPU contention
    during the handshake, not a timeout budget: no timeout was raised. Rerun the
    QUIC rounds on an unloaded host before treating this gate as green.
40. `just check` on the Linux/io_uring host stalled repeatedly inside
    `tests/multicore_tests.rs`. Under `--test-threads=1` (what the
    `test-regular` recipe uses) the binary intermittently never finishes; the
    stuck run's last started test is `test_multicore_concurrent_spawning`, and
    `test_multicore_cross_cpu_communication` was seen stalled once too. Rate on
    the revision **before** this batch (`a2c7f9a`, still the old copy-and-queue
    write shape): 2 stalls in 20 consecutive full-binary runs. With this batch
    applied: 1 in 5, then 1 in 30 — the same order of magnitude. The
    test performs no I/O whatsoever: it spawns 40 tasks from four host threads,
    waits on an `AtomicU32`, then drops a 4-worker `MultiCoreRuntime`. The
    write-path change is therefore not in its path, and the stall reproduces on
    the unmodified tree. A live stall showed three threads, with the main
    thread and one worker both in `futex_wait`. That capture was taken with the
    worker parked, and a later one is not so benign: during the gate for the
    readiness-collapse and abandoned-registration-retirement commits, `cargo
    test --lib` (same `--test-threads=1` recipe, io_uring backend, idle VM)
    reached `multicore::tests::test_runtime_creation` with every preceding lib
    test green and then stalled for 3 m 36 s. `gdb -p <pid> -batch -ex "thread
    apply all bt"` shows the main thread in `MultiCoreRuntime::drop` →
    `JoinHandle::join` (`src/multicore.rs:2729`) while the worker
    `miniss-core-0` is in state `R` with 22.6 s of CPU already burned, spinning
    in `CpuCore::run` (`src/multicore.rs:879`) → `CpuCore::process_io_completions
    (after_park=false)` (`src/multicore.rs:1310`) → `config::io_drain_limit()`
    (`src/config.rs:63`) → `std::env::var(..).ok()` and the drop of its
    `Result<String, VarError>`. So the stall and the busy-spin are one defect
    seen from two sides: the worker keeps taking the `process_io_completions`
    path — which re-reads an environment variable on every call — and never
    reaches its `draining` exit condition, while `Drop` blocks in `join` with no
    bound. This is the live capture
    `tasks/multicore-spawn-shutdown-stall.yaml` asked for, and it is a different
    defect from the epoll spin in #42 (this is the io_uring backend, and nothing
    here consumes an already-drained completion). No timeout was raised and no
    test was skipped, ignored, or deleted to make the suite green; the gate was
    simply re-run until a clean pass, and this entry is the record. The stall
    sits in the multicore spawn/shutdown lifecycle (spawning from non-worker
    threads, or runtime `Drop`), which is outside this batch's `allowed_files`;
    it needs its own task card with bounded-wait and owner evidence before the
    regular suite can be called deterministic.
41. `quic::tests::dropping_owner_socket_drains_pending_receive` is racy by
    construction, on every backend. The test body releases the socket's last
    `Arc` from two places: inside the task on the owning worker shard, and on
    the libtest thread after `spawn_on` returns. Whichever release happens last
    decides the outcome: only the owner's release retires the pending receive
    synchronously, and the assertion is evaluated inside the task's
    non-yielding `park(10ms)` + `drain_completions()` window, during which the
    shard cannot run `process_messages` and therefore cannot honour a
    cancellation posted from another thread. Evidence: on the unmodified
    baseline `c4f46a4`, the test fails 0/20 when run standalone on macOS
    (`--exact --test-threads=1`) and passes when run inside the suite, so the
    flake predates this batch and is not specific to epoll; on Linux/epoll
    after the completion-path fix it passes 3/20 standalone and 2/8 full-suite
    runs, failing only on this test (`FAILED. 123 passed; 1 failed`) with no
    hang, where before the fix the whole epoll lib suite hung in 4-5 of 6 runs.
    No timeout was raised, no test was skipped, ignored or deleted; the failing
    constraint is a race in the test's own construction, and making the
    off-shard retirement observable inside the owner's busy loop needs a
    per-shard cancellation mailbox consumed by the drain, which is a design
    change recorded in `tasks/epoll-pending-op-not-drained.yaml` rather than
    smuggled into this batch.
    Re-measured on an idle VM after the leak in #42 was killed: 20 consecutive
    full-suite runs gave **0 hangs**, 5 runs `124 passed`, 14 runs
    `123 passed; 1 failed` on exactly this test, and 1 run with this test plus
    `io::accept_ownership_tests::accepted_descriptor_closes_when_completion_is_dropped`.
    So the hang counts recorded above were taken with a core stolen by #42; the
    racy test's own in-suite failure rate is ~75% on epoll and 0% standalone on
    macOS. Still no timeout raised and no test skipped.
42. The Linux/epoll investigation behind #40 and #41 left a test binary running
    for **19 h 21 m** with a worker thread **busy-spinning at 100% of a core**.
    The process (`cargo test --lib` from `/mnt/mac/.../rust-miniss`, binary
    `target/debug/deps/rust_miniss-24beb5d344a06b46`, PID 78612) started
    2026-10-16 14:06:38 and was still alive on 2026-10-17 09:28. Its started
    test was `quic::tests::dr…` (the test in #41) and its parked threads sat in
    `arm64_sys_epoll_pwait`/`futex_wait`, so it was an **epoll-backend** build.
    Thread `miniss-core-0` (TID 78877) was in state `R` with a live `wchan` of
    `-`, and `/proc/78877/stat` showed `utime 6476127 + stime 33188` ticks =
    **~18.0 CPU-hours**, all of it in that one thread; every other worker and the
    metrics thread were parked. It was launched at 14:06 on 2026-10-16, and the
    drain-path fix (`e77bd70`) was committed at 15:28 the same day, so the spin
    belongs to the code #41 fixed: on epoll the fused completion path consumed
    nothing, so the worker always had a completion to look at, could never
    retire it, and never reached `park` — a busy loop rather than a sleep. On
    the current revision the same suite is clean: `MINISS_IO_BACKEND=epoll cargo
    test --lib` gives `124 passed; 0 failed; 0 ignored` in 1.30 s, sampled
    per-thread CPU never exceeded 50%, and 20 consecutive runs produced 0 hangs
    (#41). Two consequences are recorded rather than hidden: (i) on a 7-vCPU VM
    one core was saturated for 19 h, so **every Linux timing measurement taken in
    that window was taken with a stolen core** — including the `main` `c4f46a4`
    baseline spread of 275k–397k rps cited in the UDP rent-send decision row;
    those conclusions were cross-checked on macOS and against the untouched
    baseline, but Linux timing evidence should be re-collected on an idle VM
    before it is used for a regression claim; (ii) this is exactly the leak the
    `Test and process hygiene` rule forbids. The process was killed, together
    with its parent `cargo test --lib` and `tail`, and with the hung
    `tokio_compat_tests` binary plus the `just check`/`cargo test` pair left by
    this card's first gate attempt, whose `test_tokio_futures_interop` had been
    spinning for 2 m 43 s at 45% CPU. On the idle VM that same test then passed
    `3 passed; 0 failed` in 0.02 s, 5 runs out of 5, so the hang is attributed to
    the stolen core rather than to a defect of its own; it is not counted against
    the multicore-stall card. The VM now runs nothing but its own daemons.
43. `tests/vectored_io.rs::linux_uring_delayed_submit_writev_survives_relocation_pressure`
    hangs on Linux/io_uring and stalled card (b)'s `just check`. Measured
    standalone under `--test-threads=1` on an idle VM: 6 hangs in 11 runs with
    (b) applied, and 6 in 10 runs with (b)'s source changes stashed away, so the
    write-API shape change is not the trigger. That is also what the diff says:
    the test drives `AsyncWrite::poll_write_vectored`, which (b) does not touch.
    The hang is **not** in the writev. `Runtime::block_on` returns, every
    assertion passes, and the test body is then parked in `peer.join()` (gdb:
    `tests/vectored_io.rs:449`) while the peer thread is parked in its blocking
    `read()` at `tests/vectored_io.rs:389`. `ss -tinp` on the hung process shows
    the client socket still open on fd 5 in `ESTAB` with `send-q 0`/`recv-q 0`:
    the payload was delivered and drained, and **`close(2)` was never issued**, so
    the peer's EOF read can never return. Root cause: the descriptor is an
    `Arc<OwnedFd>` (`src/io/mod.rs:59`) and every submitted op clones it into a
    `DescriptorLease` that the backend holds in `descriptor_leases[slot]` until
    that op's terminal CQE is drained (`src/io/uring.rs` 426/513/1005/1668). An
    op that is never reaped therefore keeps the fd — and the TCP connection —
    alive, and `drop(AsyncTcpStream)` cannot close it. Three existing design
    choices combine to leave one armed: `UringBackend::submit_inner` deliberately
    does not flush (`src/io/uring.rs:994-995`) and `flush()` is a no-op below
    `FLUSH_BATCH_SIZE = 61` (`src/io/uring.rs:170`, 1349-1362), so the arm's SQE
    is normally submitted by `park`, not by the poll; `park_timeout` is **100 µs**
    (`src/config.rs:50`); and when `park` returns with no completion
    `Runtime::block_on` falls through and re-polls the future **with no waker**
    (`src/executor.rs:176-209`). That unsolicited re-poll takes `pollout_token`
    (`src/net.rs:666-672`) — `clear_slot` clears only the cpu-side slot and frees
    nothing in the backend (`src/cpu.rs:537-548`) — and if that retry's `writev`
    succeeds the future is `Ready` and `block_on` returns immediately
    (`src/executor.rs:107-110`) with the registration still armed and undrained.
    Nothing holds its token any more, so `Drop for AsyncTcpStream`
    (`src/net.rs:463-475`) has nothing to cancel, the lease is never released, and
    `close` never runs. Whether the retry lands before or after the CQE is drained
    is a pure timing race, which is the 33–60% rate. The readv sibling of the same
    test never shows it because its peer (`tests/vectored_io.rs:279-286`) writes
    and returns instead of waiting for EOF, so a retained lease is invisible
    there; the writev test is simply the only test that makes lease retention
    observable. No timeout was raised and no test was skipped, ignored or deleted;
    (b) was exonerated by the stash control above, and the gate was re-run.

    **Cause proven by measurement.** A temporary probe — an `Arc::strong_count`
    read of the descriptor identity in `Drop for AsyncTcpStream`, plus a print at
    every readiness arm and at `release_descriptor_lease` — turned the mechanism
    above into an arithmetic invariant over 6 standalone runs on an idle VM:
    `refs at drop = 1 + (arms - lease releases)`. Every hanging run reported
    `refs=2` with `arms/takes/releases` of 193/193/192, 61/61/60, 63/63/62,
    135/135/134 and 125/125/124; the single passing run reported `refs=1` with
    236/236/236. So on a hanging run exactly one lease — always the last arm's —
    outlives the stream, and `close(2)` is therefore never issued: the cause is
    confirmed at the point of failure rather than inferred from the design. The
    probes were removed before the fix was committed.

    **Fix.** `Runtime::block_on` now retires the registrations its completed
    future abandoned before it returns (`retire_abandoned_registrations`,
    `src/executor.rs`). A readiness poll is a pure wakeup registration: it carries
    no payload, so once the stream takes the token back and the retry succeeds
    nothing will ever await it, yet its descriptor lease is released only by its
    terminal completion. That completion has normally been posted already by the
    time the future finishes — the successful `write(2)`/`writev(2)` that ended the
    wait is itself a kernel entry, and with `DEFER_TASKRUN` the completion task
    work for the poll runs on kernel entry/exit — so the first drain retires it,
    with a bounded three-turn `park_timeout` park covering the case where the
    kernel has yet to post it. The two identical drain-and-deliver blocks in
    `block_on` were folded into one helper (`drain_and_deliver`) rather than copied
    a third time. Verification, same probe re-run after the fix: 6/6 runs `rc=0`
    with `releases = takes` every time (273/273/273, 257/257/257, 243/243/243,
    160/160/160, 263/263/263, 249/249/249) — every lease retired — and then the
    card's acceptance loop: **30 consecutive `--test-threads=1 --exact` runs with
    0 hangs** (0–1 s each) plus 3 full `vectored_io` binary runs clean in ~3 s.
    Note what the probe shows and does not: `refs` is still >1 at
    `drop(writer)` on 5 of those 6 runs (60 on one run), because the probe fires
    inside the future, *before* the retire pass. The stream's own drop cannot close
    the fd there; the retire pass is what closes it before `block_on` returns, and
    the test's `peer.join()` runs after that.

    The card had warned that a `block_on`-only drain cannot be the whole answer,
    because "the worker loop and the shutdown path also stop with an armed
    registration". That objection is refuted by the data-plane driver's own code:
    the `MultiCoreRuntime` worker loop drains continuously and does not park
    between a task's poll and the next completion, and its escalated shutdown
    already calls `cancel_all()`, flushes, and then parks and re-drains until
    `pending_operation_count() == 0` (`src/multicore.rs:836-844`), with the ordinary
    drain exit gated on the same counter (`:937`, `:1000`). That counter — the same
    one this fix consults — is exactly "the shard still owes work", so the data
    plane was already retiring to quiescence before stopping; only the
    control-plane compat wrapper returned without that step. The lease semantics
    (`RegisteredDescriptor::lease()` cloning rather than `dup(2)`, so that fd
    ownership follows op retirement) are deliberately left alone: making the lease
    submission-scoped instead of completion-scoped would change the lifetime rule
    for every operation, which is a larger decision than the defect measured here.
    Residual, recorded rather than hidden: a registration whose completion never
    arrives (an armed read poll on a socket that never becomes readable, dropped by
    the completed future) still holds its lease until the runtime itself is
    dropped, which is where every lease is released. Tracked by
    `tasks/vectored-writev-armed-registration-fd-leak.yaml`. Review follow-up #5
    ("io_uring terminal cleanup is duplicated across completion paths") is the
    same hazard seen from the other side.
