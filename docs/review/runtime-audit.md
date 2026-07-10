# Rust-Miniss Repository Audit

  Audited c749821e5a6abbf65ba5ac183939b316fd7fdf52. Worktree remained
  unchanged and clean; branch is one commit ahead of origin/main.

  ## A. Executive Assessment

  The implementation is a hybrid of:

  - A single-thread Runtime that creates a fresh backend inside each
    block_on.

  - A MultiCoreRuntime with one worker, task queue, timer wheel, and ring
    per logical CPU.

  - Bounded crossbeam inboxes plus eventfd wakeups.
  - Shard-local operation slabs and buffer pools.

  The intended thread-per-core shape is visible, but its safety boundary
  is not enforced by Rust’s type system. There are confirmed memory-safety
  defects in vectored I/O, the custom waker, backend thread affinity, and
  TLS lifetime handling. Cancellation and shutdown do not model kernel-
  operation lifetimes. The primary HTTP benchmark also routes accepted
  connections through cross-core scheduling despite using SO_REUSEPORT.

  Assessment: not safe for production use, not ready for further
  optimization, and current benchmarks cannot justify performance claims.
  Memory safety and lifecycle repair must precede throughput work.

  ## B. Actual Architecture

  Primary paths:

  - Remote task: spawn → O(N) inbox-depth scan → bounded channel →
    optional eventfd → process_messages → shard task_queue/ready_queue →
    Task::poll.

  - Local task: spawn_local → unbounded TLS Vec → drain_local_spawns →
    shard queues.

  - Network read: nonblocking syscall fast path → Op::Read → PendingOp
    slab → SQE → flush/park → CQE decode → CpuIoState slot → waker → copy
    to caller.

  - Filesystem read: AsyncFile::read_at → raw-fd Op::ReadFile → owned
    buffer/SQE → CQE → buffer length publication → future result.

  - Timer: SleepFuture → TLS TimerWheel → slab entry → worker expiry →
    task waker.

  - HTTP: one reuse-port listener per core, but accepted handlers call
    global spawn, not spawn_local (examples/http_echo.rs:249).

  - Shutdown: reject new remote spawns → set per-core atomics → best-
    effort messages/eventfd → workers wait until tasks vanish; no
    cancellation phase or deadline.

  ## C. Critical Findings

  1. S0, confirmed: vectored SQEs contain dangling kernel pointers.
     ReadVectored and WriteVectored build SQEs from SmallVec::as_ptr(),
     then move the inline SmallVec twice before submission (src/io/
     uring.rs:906). For 1–4 iovecs the pointer addresses stack/inline
     storage that no longer exists when deferred submission occurs.
     Consequence: kernel reads invalid iovec metadata, permitting invalid
     memory reads/writes, disclosure, corruption, or EFAULT. Store iovecs
     in stable pinned/boxed operation memory before forming the SQE.
     Validate 1–4 segment delayed-submit readv/writev under Linux
     sanitizers.

  2. S0, confirmed: the custom RawWaker is unsound. It combines non-atomic
     Rc, an unsynchronized raw VecDeque pointer, unsafe Send/Sync, and
     direct &mut reconstruction (src/waker.rs:39). Safe futures may move,
     clone, wake, or drop Waker across threads; moving Runtime/Executor
     also relocates the queue header. An in-tree fallback thread already
     wakes cloned wakers from another thread (src/timer/sleep.rs:40).
     Consequence: Rc races, queue data races, and use-after-free. Replace
     it with an address-stable, cross-thread-safe bounded shard
     notification mechanism.

  3. S0, confirmed: safe APIs permit concurrent mutation of UringBackend.
     IoProvider requires Send + Sync; UringBackend and CpuIoState unsafely
     implement them while mutating rings, slabs, and vectors through
     UnsafeCell (src/io/mod.rs:35, src/io/uring.rs:98, src/cpu.rs:61). The
     backend is publicly exposed as a clonable Arc; affinity is only a
     debug assertion. Safe code can call submit concurrently and cause UB.
     Make the local backend !Send + !Sync and expose only owner-thread
     capabilities.

  4. S0, confirmed data-corruption path: deferred SQEs do not own
     descriptor identity. File and network operations store raw fd
     integers, while small SQ batches remain unsubmitted (src/fs.rs:149,
     src/io/uring.rs:695, src/io/uring.rs:1147). Dropping a future clears
     only its waiter slot; it neither removes the SQE nor cancels it (src/
     io/future.rs:77). Closing and reusing the fd before io_uring_enter
     can execute the operation against an unrelated file/socket. Queued/
     submitted operations must retain descriptor identity until terminal
     completion.

  5. S0, confirmed: panic leaves dangling runtime TLS. Runtime::block_on
     installs a pointer to stack-local CpuIoState but clears it only on
     normal completion (src/executor.rs:53); TLS access returns an unsafe
     forged 'static reference (src/cpu.rs:389). Catching a user-future
     panic and then calling runtime I/O can dereference freed state. Use
     an RAII context guard that restores prior I/O, timer, and task TLS on
     every exit.

  6. S1, confirmed: submission failures become lost operations. do_flush
     discards errors, park clears pending counts after an ignored
     submit_with_args result, and second SQ-push failure removes PendingOp
     but still returns its token (src/io/uring.rs:211, src/io/
     uring.rs:1098, src/io/uring.rs:1166). Unsupported operations return
     u64::MAX; registering that token attempts to resize a slot vector
     through index u32::MAX, potentially aborting from OOM. Submission
     must be fallible and preserve an explicit queued/submitted/failed
     state.

  7. S1, confirmed: cancellation targets unrelated tasks and can silently
     fail. Task IDs encode core and sequence, but every core handles a
     broadcast cancellation using only the sequence index (src/
     waker.rs:22, src/multicore.rs:609). Canceling core 0/sequence 1 can
     remove sequence 1 on every shard. Full inbox errors are ignored while
     returning success (src/multicore.rs:1452). JoinHandle::cancel only
     changes the reported result; the task continues executing (src/
     task.rs:320). Route by full task identity and define accepted/not-
     found/backpressured outcomes.

  8. S1, confirmed: shutdown is not a kernel-lifecycle state machine.
     Pending tasks, accepts, timers, SQEs, and submitted operations are
     never cancelled; join has no deadline; Drop signals but does not join
     (src/multicore.rs:1399, src/multicore.rs:1535, src/
     multicore.rs:1622). A permanently pending task or accept hangs
     graceful shutdown. Dropping runtime-owned eventfds while detached
     workers retain raw fds also creates descriptor-reuse risk. Partial
     initialization can detach already-started workers.

  9. S1, confirmed: multishot accept has unbounded and invalid terminal
     behavior. Accept is always submitted as AcceptMulti without opcode
     probing; support always reports true (src/io/uring.rs:682, src/io/
     uring.rs:1675). Results accumulate in an unbounded VecDeque;
     abandoned results contain raw accepted fds that are not closed. A
     terminal CQE removes the operation, but the listener retains the dead
     token and later hangs (src/net.rs:45). Use bounded RAII OwnedFd
     delivery, explicit overflow-close policy, terminal rearm/fallback,
     and listener-drop cancellation.

  10. S1, confirmed: one self-waking task can monopolize a shard. Wakes
     blindly enqueue duplicates and execute_tasks drains without a poll
     budget (src/waker.rs:66, src/multicore.rs:638). A future that wakes
     itself and returns Pending prevents the outer I/O/timer/message
     budget from running. Add scheduled-state deduplication and a bounded
     per-turn poll budget.

  11. S1, confirmed architectural drift: multicore APIs and benchmark
     traffic bypass affinity. block_on builds another complete runtime/
     ring; block_on_cpu does not run on the requested core (src/
     multicore.rs:1565). The HTTP accept loop re-dispatches every
     connection using global spawn, incurring an O(N) sender-depth scan
     and cross-core channel/eventfd path (src/multicore.rs:1188). This
     contradicts the documented SO_REUSEPORT ownership model.

  12. S2, confirmed long-run resource growth. Eventfd wake completions
     bypass pending_ops.try_remove, leaking one slab entry per rearm (src/
     multicore.rs:340, src/io/uring.rs:268). IoToken slot tables are
     indexed by a monotonic low-32-bit counter rather than reusable slab
     slot, so memory grows with lifetime operation count. Multishot
     completions also bypass drain budgets, threatening fairness under
     accept floods.

  13. S2, confirmed build/test gate defects. cargo fmt, all-target check/
     tests, and clippy currently fail. Documentation and just bench-smoke
     request nonexistent feature io_uring; the smoke gate runs only HEAD,
     calls it a baseline, omits latency, and parses arbitrary digits
     (justfile:52, Cargo.toml:142). Build-time host-kernel detection is
     not runtime feature probing and prevents reliable cross-compilation
     (build.rs:13).

  No S3 item should be scheduled before these findings.

  ## D. Unsafe-Code Ledger

   Site                Vectored SQE pointers
   Required invariant  iovec metadata remains immovable through CQE
   Status              Violated
  ────────────────────────────────────────────────────────────────────────
   Site                Raw waker/Rc/queue pointer
   Required invariant  all operations same-thread; queue address/lifetime
                       stable
   Status              Violated by safe API
  ────────────────────────────────────────────────────────────────────────
   Site                Backend/CpuIoState Send + Sync
   Required invariant  only owner thread accesses UnsafeCell
   Status              Not enforced
  ────────────────────────────────────────────────────────────────────────
   Site                TLS 'static references
   Required invariant  context outlives every returned reference and
                       unwinding
   Status              Violated on panic/nesting
  ────────────────────────────────────────────────────────────────────────
   Site                PendingOp buffers and C structs
   Required invariant  allocation remains stable until terminal CQE
   Status              Mostly established for boxed UDP state; not for
                       inline iovecs
  ────────────────────────────────────────────────────────────────────────
   Site                CQE decoding and set_len
   Required invariant  generation matches and result ≤ submitted capacity
   Status              Code establishes basic bounds; lacks fault/stress
                       evidence
  ────────────────────────────────────────────────────────────────────────
   Site                Buffer ManuallyDrop/TLS pool
   Required invariant  single ownership and initialized length
   Status              Plausible locally; Miri coverage is incomplete
  ────────────────────────────────────────────────────────────────────────
   Site                Pin projections
   Required invariant  fields never move after pointer formation
   Status              Several sites rely on comments rather than
                       structural projection

  ## E. Concurrency and Ownership Ledger

   Mutable state                    Intended owner       Mechanism /
                                                         cross-core risk
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━
   Task and ready queues            CpuCore              Raw waker
                                                         bypasses
                                                         ownership;
                                                         duplicate/
                                                         unbounded
                                                         scheduling
  ───────────────────────────────  ───────────────────  ──────────────────
   Ring, PendingOps, completion     worker shard         UnsafeCell;
   slots                                                 public Arc +
                                                         Send + Sync
                                                         breaks isolation
  ───────────────────────────────  ───────────────────  ──────────────────
   Remote inbox                     destination shard    Bounded 4096
                                                         crossbeam MPMC;
                                                         backpressure
                                                         exists but some
                                                         errors are
                                                         discarded
  ───────────────────────────────  ───────────────────  ──────────────────
   Local spawn queue                worker TLS           Unbounded Vec;
                                                         one task can
                                                         enqueue to OOM
  ───────────────────────────────  ───────────────────  ──────────────────
   Runtime control state            coordinator          Acquire/Release
                                                         atomics are
                                                         mostly coherent
  ───────────────────────────────  ───────────────────  ──────────────────
   Join state                       caller/task          Arc<parking_lot:
                                                         :Mutex>; not
                                                         multicore hot
                                                         path, but
                                                         cancellation
                                                         semantics are
                                                         false
  ───────────────────────────────  ───────────────────  ──────────────────
   Buffer pool/timer wheel/         worker TLS/shard     Correct
   metrics                                               ownership
                                                         direction;
                                                         lifecycle
                                                         observability
                                                         incomplete

  ## F. io_uring Lifecycle Model

  Target lifecycle should be:

  Allocated → SQ-queued → Kernel-submitted → Completing/Cancel-requested →
  Terminal CQE → Result delivered/discarded safely → State reclaimed

  Current invalid transitions include:

  - SQ-queued → waiter cleared → fd closed/reused → later submitted
  - SQ push failed → PendingOp removed → token waits forever
  - Unsupported → sentinel token → huge slot allocation
  - Multishot terminal → PendingOp removed → listener retains token
  - Accept CQE → waiter absent → raw accepted fd leaked
  - Shutdown → worker exit/ring Drop without explicit cancellation and
    terminal-CQE accounting

  ## G. Shutdown Model

  Implemented: stop admission, set flags, wake shards, and wait
  indefinitely for local task count to reach zero.

  Required: stop admission → freeze queued SQEs → cancel logical tasks/
  timers → submit kernel cancellations → retain all buffers/fds → drain
  every terminal CQE → close surplus accepted fds → unregister resources →
  close ring → clear TLS → report worker result → join. A bounded graceful
  phase and explicit immediate phase are both missing.

  ## H. Test-Gap Matrix

   Invariant/failure            Current coverage         Required
                                                         deterministic
                                                         test
  ━━━━━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━━━━━━  ━━━━━━━━━━━━━━━━━━
   Waker thread/lifetime/       Same-thread unit         Cross-thread
   address stability            tests only               clone/wake/drop,
                                                         executor move,
                                                         escaped waker
                                                         after
                                                         cancellation
  ───────────────────────────  ───────────────────────  ──────────────────
   I/O pointer/fd lifetime      Basic round trips        1–4 iovec
                                                         delayed flush;
                                                         close/reuse
                                                         before submit
  ───────────────────────────  ───────────────────────  ──────────────────
   Submission saturation/       None                     SQ-full plus
   errors                                                injected EINTR,
                                                         EBUSY, submit/
                                                         enter failure
  ───────────────────────────  ───────────────────────  ──────────────────
   Cancellation                 Tests only assert API    Every operation
                                returns Ok               boundary; prove
                                                         target stops and
                                                         neighbors
                                                         survive
  ───────────────────────────  ───────────────────────  ──────────────────
   Multishot/CQ pressure        One happy accept         Terminal CQE,
                                                         abandoned
                                                         listener,
                                                         bounded
                                                         overflow, fd
                                                         accounting
  ───────────────────────────  ───────────────────────  ──────────────────
   Shutdown                     Finite tasks/timers      Pending accept/
                                only                     I/O, queued SQE,
                                                         permanent task,
                                                         repeated/drop/
                                                         partial-init
  ───────────────────────────  ───────────────────────  ──────────────────
   Fairness                     None                     Self-waker
                                                         beside I/O,
                                                         timer, remote
                                                         message,
                                                         shutdown
  ───────────────────────────  ───────────────────────  ──────────────────
   Feature absence              None                     Opcode/flag
                                                         fallback without
                                                         rebuilding on
                                                         another host
                                                         kernel

  Miri is useful for TLS, RawWaker, pinning, and ManuallyDrop; Loom is
  useful for notification state. Neither validates kernel ownership.
  Kernel-mediated lifetimes need Linux fault injection and sanitizers.

  ## I. Benchmark Assessment

  The historical 30-second HTTP data is useful only as a directional
  snapshot. A later artifact reports miniss 219,728 req/s, p99 22.95 ms,
  monoio 297,634/14.40 ms, and compio 305,006/7.66 ms at 800 connections
  (.scratch/http-800-20260701/summary.csv:1). It is not a current baseline
  because the current all-target build fails and Linux could not be rerun.

  The HTTP harness preserves raw logs and percentiles, but matrix cells
  are single 5–10 second samples, diagnostics silently become zero when
  perf is unavailable, and current matrix.csv contains only its header.
  There is no run randomization, confidence interval, CPU-frequency/NUMA
  control, or comparable allocation/cache evidence.

  Microbenchmarks do not isolate their claims: scheduler throughput
  includes channel creation/rendezvous and runtime lifetime; cross-core
  latency includes two spawn hops plus a synchronous channel; timer
  benchmarks primarily measure wall-clock sleep or the wheel data
  structure. Missing are SQ/CQ throughput, local-vs-remote spawn,
  cancellation, idle CPU, wake storms, allocation counts, and 1-to-N-core
  scaling.

  ## J. Prioritized Remediation Program

  1. Memory-safety containment: stabilize vectored metadata, replace
     RawWaker, remove unsafe backend sharing, add TLS restoration.
     Completion: compile-fail affinity tests, Miri-clean local tests,
     Linux delayed-submit vectored stress.

  2. Operation identity/state machine: own descriptor identity, make
     submission fallible, add queued/submitted/cancelling/terminal states.
     Completion: fault-injected SQ/enter tests and no orphan tokens.

  3. Cancellation and shutdown: full TaskId routing, kernel cancellation,
     bounded graceful plus immediate shutdown, transactional startup,
     reliable join. Completion: all lifecycle-boundary tests and zero
     outstanding resources.

  4. Multishot/backpressure/fairness: probe operations, bound accept
     delivery, fix terminal handling, deduplicate/budget scheduling, bound
     local spawn. Completion: CQ/accept pressure and self-wake fairness
     tests.

  5. Architecture/API restoration: make shard-local spawn support !Send,
     implement or remove block_on_cpu, keep connection handlers on
     accepting shard, separate blocking file open. Completion: documented
     ownership paths match code and architecture audit.

  6. Only then performance: repair gates, establish repeated Linux
     baselines against monoio/compio, capture p99, CPU, allocations,
     syscalls, and cache behavior before selecting an optimization.

  ## K. Validation and Missing Evidence

  Executed read-only:

  - cargo fmt --all -- --check: failed, formatting differences.
  - cargo check --locked --all-targets --all-features: failed, private
    IoToken::from_id used by tests/runtime_regressions.rs:63,75,76.

  - cargo test --locked --all-targets --all-features: failed to compile;
    zero tests ran.

  - cargo clippy --locked --all-targets --all-features -- -D warnings:
    failed, identical branches at src/multicore.rs:732-736.

  - Library-only cargo check --locked --lib --no-default-features and
    --all-features: passed.

  - cargo tree --locked -d: passed; duplicates include http, hyper,
    metrics, socket2, thiserror, and rand.

  - Linux/io_uring tests, sanitizers, profiling, and fresh benchmarks: not
    run because the Orb VM timed out starting. Host was Darwin 25.5 arm64,
    Rust/Cargo 1.88.0.

  - rtk was unavailable, so raw read-only commands were used.
  - Final git diff --stat was empty.

  Unresolved external evidence: actual kernel feature set, SQ/CQ behavior
  under pressure, sanitizer reproduction of kernel pointer faults, runtime
  CPU topology/cpuset behavior, and dependency-unused status pending
  cargo-udeps.
