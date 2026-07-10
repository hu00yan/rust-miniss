# Verified Runtime Remediation Plan

Verified against `c749821` (current `HEAD`) by static inspection of the current repository. Source code was not modified and no build, test, sanitizer, or benchmark command was run during this verification. Dynamic Linux/io_uring evidence is therefore still required where called out below.

This document corrects and supersedes the S0/S1 remediation guidance in `docs/review/runtime-audit.md`. It does not supersede that audit's benchmark or lower-severity observations.

## 1. Executive conclusion

The audit's main conclusion is correct: the runtime is not production-safe. The most important confirmed defects are:

- safe `Waker` use can race with or outlive shard-owned scheduler memory;
- safe TLS functions can manufacture references that outlive stack-owned runtime state;
- the type system permits concurrent access to backends whose mutation relies on thread affinity;
- io_uring SQEs can retain pointers to moved inline iovec metadata;
- pending operations do not retain descriptor identity through terminal completion;
- the epoll/kqueue fallback exposes uninitialized storage through safe slices;
- cancellation, admission, and shutdown have no coherent owner-acknowledged lifecycle;
- scheduler identity, fairness, CPU ownership, and runtime ownership do not enforce the architecture contract.

The audit understates the TLS and fallback-backend problems, and it overstates a few narrower claims. In particular, the current atomic orderings are not the principal failure. Most individual Acquire/Release choices are reasonable; the protocols lack linearization points, acknowledgements, and lifetime ownership. Replacing them with `SeqCst` would not repair the design.

## 2. Corrected findings

Severity meanings in this plan:

- **S0**: a safe call path can violate Rust memory safety, corrupt unrelated data, or let the kernel operate on invalid/reused identity.
- **S1**: a production lifecycle, ownership, liveness, or architecture-contract defect that can hang, lose, misroute, or continue work contrary to the public result.

### S0-1: the custom task waker is unsound

**Verdict: confirmed; the lifetime defect is broader than the audit states.**

`MinissWaker` contains an `Rc`-managed allocation and a raw pointer to `VecDeque<TaskId>`, unsafely claims `Send + Sync`, reconstructs the non-atomic `Rc` in its vtable, and creates `&mut VecDeque` from a shared raw pointer (`src/waker.rs:39-108`). A standard `Waker` may safely be cloned, moved, woken, and dropped on another thread. The timer fallback does exactly that by moving a cloned waker into a new thread (`src/timer/sleep.rs:43-57`).

Cancellation and teardown make the defect terminal: an escaped clone can wake after the task's waker has been dropped or after the shard queue has been destroyed. Before teardown this is an `Rc`/queue race; after teardown it is a use-after-free.

The audit's claim that moving the queue owner is itself a proven multicore failure needs qualification. Public `Executor`/`Runtime` values can be moved after wakers capture their queue address, but `CpuCore` is not visibly moved after wakers are created inside `run`. Cross-thread and escaped-waker behavior already prove the multicore implementation unsound without relying on that move argument.

**Required correction:** remove the custom `RawWaker`, raw queue pointer, `Rc` vtable, and unsafe `Send`/`Sync`. A waker must own only address-stable cross-thread-safe state and a lifetime-decoupled shard notifier. Late wakes after cancellation or shutdown must become harmless no-ops.

### S0-2: TLS lifetime is unsound even without panic

**Verdict: confirmed and upgraded.**

`io_state()` and `try_io_state()` safely forge `&'static CpuIoState` from a TLS raw pointer (`src/cpu.rs:400-429`). Safe code can return that reference from `Runtime::block_on`, after the stack-local `CpuIoState` is destroyed (`src/executor.rs:53-64`). The safe public setter can also install a pointer to arbitrarily short-lived state (`src/cpu.rs:433-445`).

Panic and nesting are additional failures. `Runtime::block_on` has a timer guard, but I/O TLS and current-task TLS are manually cleared only on normal paths (`src/executor.rs:73-110`). A panic leaves dangling or stale state. A nested runtime overwrites the outer context and clears it instead of restoring it. `MultiCoreRuntime::block_on` creates a fresh `Runtime` and backend on the caller thread (`src/multicore.rs:1565-1588`), providing a safe route to nesting from a worker.

`CpuCore::run` does clear worker TLS through a guard (`src/multicore.rs:364-382`), but that guard cannot revoke a forged `'static` reference.

**Required correction:** make context installation crate-private and scoped. One RAII guard must save and restore the previous I/O, timer, task, and core context on success, panic, and nesting. Access must be closure-based or otherwise lifetime-bound so a reference cannot escape.

### S0-3: backend ownership is not enforced by the type system

**Verdict: confirmed; scope includes every current backend.**

`IoProvider` requires `Send + Sync` while exposing mutation through `&self` (`src/io/mod.rs:35-123`). `UringBackend` stores its mutable ring, slabs, queues, and vectors in `UnsafeCell`, then unsafely implements `Send + Sync`; its owner check is debug-only (`src/io/uring.rs:98-134`, `src/io/uring.rs:664-673`). `CpuIoState` also unsafely implements both traits and publicly exposes a cloneable backend `Arc` (`src/cpu.rs:61-89`).

The same problem exists in `EpollBackend` (`src/io/epoll.rs:67-96`), and `KqueueBackend` is an alias of that implementation (`src/io/kqueue.rs:7`). The audit was too narrow in naming only io_uring.

**Required correction:** the shard I/O driver and `CpuIoState` must be `!Send + !Sync`. They are owned directly by the worker, not through `Arc<dyn IoProvider>`. Cross-core code receives only a bounded command/notifier handle that cannot call local driver methods.

### S0-4: io_uring vectored metadata moves after pointer formation

**Verdict: confirmed.**

Read and write vectored submissions build an SQE from `SmallVec::as_ptr()` and then move the inline `SmallVec` into `PendingOp` (`src/io/uring.rs:906-948`). For one through four iovecs, the metadata lives inline. Moving it invalidates the pointer stored in the SQE; slab insertion moves the operation again (`src/io/uring.rs:1097-1104`).

`PendingOp` also derives `Clone` (`src/io/uring.rs:18`). No live clone call was found, so this is a latent rather than demonstrated path, but derived cloning is invalid for variants whose C structs contain pointers into sibling allocations.

**Required correction:** allocate all kernel-visible transitive argument graphs before SQE construction in stable boxed/pinned storage. Moving the owning operation must not move the pointee. `PendingOp` must not be cloneable.

### S0-5: pending I/O does not retain descriptor identity

**Verdict: confirmed as a data-integrity S0.**

`Op` carries raw integer descriptors (`src/io/mod.rs:125-180`). File operations borrow the raw descriptor from `AsyncFile` and submit it (`src/fs.rs:144-156`); dropping `IoFuture` only clears the waiter slot (`src/io/future.rs:72-89`). Network stream drop similarly clears slots while the underlying descriptor can close (`src/net.rs:443-461`). An SQE may still be locally queued, or an operation may still be in the kernel, after the user future or resource wrapper has been dropped. Numeric descriptor reuse can redirect the operation to an unrelated file/socket.

The fallback path has the same identity gap: it duplicates the raw integer only when the queued thread-pool job begins, not when the operation is accepted (`src/io/epoll.rs:242-256`).

**Required correction:** accepting an operation must acquire an owned descriptor lease immediately. That lease, buffers, and kernel arguments remain owned by operation state until a terminal completion is observed. Dropping a user future abandons delivery or requests cancellation; it does not reclaim operation memory.

### S0-6: epoll/kqueue exposes uninitialized bytes as initialized

**Verdict: major omission from the audit.**

`Buffer::new_uninitialized` returns a zero-length buffer whose spare capacity is not initialized, and `set_len` requires the new range already to be initialized (`src/buffer.rs:84-99`, `src/buffer.rs:164-179`). The fallback backend sets length before passing the buffer to safe `File::read` and `UdpSocket::recv_from` (`src/io/epoll.rs:255-266`, `src/io/epoll.rs:665-675`). That constructs a safe `&mut [u8]` which claims uninitialized elements are valid values.

Because the macOS kqueue adapter aliases the epoll implementation, this is not Linux-fallback-only.

**Required correction:** use `MaybeUninit`/spare-capacity interfaces or raw syscalls that never form an initialized slice before the read. A zero-filled buffer is an acceptable containment implementation if its cost is measured later. Publish logical length only after validating the returned byte count.

### S1-1: submission failure has no representable result

**Verdict: confirmed.**

`do_flush` ignores submission errors (`src/io/uring.rs:211-228`). `park` ignores `submit_with_args`, then clears `pending_sqes` (`src/io/uring.rs:1166-1204`). If the SQ remains full after an emergency flush, the pending operation is removed but its token is still returned (`src/io/uring.rs:1118-1144`). Unsupported operations return the sentinel token `u64::MAX` (`src/io/uring.rs:1077-1093`); registering that token maps to slot index `u32::MAX`, and `CpuIoState` tries to resize its slot vector through that index (`src/io/mod.rs:257-270`, `src/cpu.rs:113-138`). This can wait forever or abort on allocation.

**Required correction:** `submit` is fallible. No token exists until the driver has installed an operation in a valid state. Every SQ/enter failure transitions that operation to an explicit failed terminal result without orphaning a token.

### S1-2: cancellation is misrouted and its success result is false

**Verdict: confirmed.**

`TaskId` encodes the core in its high bits and sequence in its low bits (`src/waker.rs:20-36`). `cancel_task` broadcasts to every core, discards every bounded-channel error, and returns `Ok(())` (`src/multicore.rs:1451-1468`). Each receiver ignores `cpu_id()` and indexes only by sequence (`src/multicore.rs:609-620`). Cancelling one task can therefore remove the same sequence slot on multiple shards while leaving the intended task running if its inbox was full.

`JoinHandle::cancel` is a different and misleading mechanism: it marks only the join state as cancelled and never stops or removes the executing task (`src/task.rs:327-346`). The task can continue side effects after its handle has reported cancellation.

Dropping a task future does run field destructors, so a contained `SleepFuture` unregisters its timer (`src/timer/sleep.rs:68-75`). The audit's statement that timers are never cancelled is too broad if read as applying to successful task cancellation; it remains correct that shutdown has no timer-cancellation phase.

**Required correction:** route one full generational task key to its owner shard. Cancellation is acknowledged by that owner and has explicit `Accepted`, `NotFound`, `AlreadyTerminal`, `Backpressured`, and `RuntimeStopping` outcomes. A join handle resolves `Cancelled` only after the owner has stopped polling and dropped the future.

### S1-3: shutdown is not a lifecycle state machine

**Verdict: confirmed with two qualifications.**

Shutdown closes the coordinator state, sets per-core flags, sends best-effort messages, and returns (`src/multicore.rs:1399-1449`). A worker exits only when its task count, ready queue, and inbox are empty (`src/multicore.rs:463-469`, `src/multicore.rs:518-524`). A permanently pending future or accept keeps shutdown open forever. Pending timers, SQEs, submitted operations, and accept results do not participate in the exit predicate.

There is no driver cancellation interface (`src/io/mod.rs:35-123`), bounded graceful deadline, immediate escalation, or terminal-completion accounting. `join` blocks without a deadline and returns on the first failed worker, dropping and detaching remaining join handles (`src/multicore.rs:1535-1563`). `Drop` signals but does not join (`src/multicore.rs:1622-1629`). The coordinator owns `OwnedFd` eventfds while workers retain raw integers (`src/multicore.rs:809-827`, `src/multicore.rs:890-925`), so detached-worker paths permit close/reuse while a worker still uses the number.

Qualification one: closing an io_uring context may cause implicit kernel cancellation, so the proven defect is not that the kernel can never cancel. The proven defect is that the program has no explicit cancellation transition, terminal drain/accounting, or portable backend contract before reclaim.

Qualification two: reported worker-backend startup failures and readiness timeouts do attempt to stop and join already-started workers (`src/multicore.rs:942-973`). The unchecked early-`?` paths for eventfd creation and thread spawning still detach prior workers (`src/multicore.rs:878-941`). Startup rollback is partially, not universally, broken.

**Required correction:** one owner must close admission, cancel tasks/timers, request driver cancellation, drain terminal completions while retaining all operation resources, join every worker, aggregate every error, then publish terminal state.

### S1-4: scheduler fairness is not bounded

**Verdict: confirmed.**

Every wake blindly pushes another ready entry (`src/waker.rs:66-73`), and `execute_tasks` drains the ready queue without a poll budget (`src/multicore.rs:638-687`). A self-waking future can keep that function from returning. `MAX_BUSY_ROUNDS` in the outer loop cannot help because the outer loop never regains control.

**Required correction:** one scheduled record per live task plus a per-turn poll budget. I/O completions, timers, remote messages, cancellation, and shutdown must make bounded progress beside a self-waker.

### S1-5: task identity is generationless and wraps

**Verdict: major omission from the audit.**

Each per-core counter is `u64` but is truncated to `u32` when constructing `TaskId` (`src/multicore.rs:1183-1186`). Task storage and cancellation use only that sequence as a vector index (`src/multicore.rs:582-591`, `src/multicore.rs:609-620`, `src/multicore.rs:641-681`). After wrap, a live slot can be overwritten, and a stale wake/cancel can target a replacement task.

**Required correction:** use a fixed-capacity per-shard arena and `TaskKey { runtime_generation, shard, slot, generation }`. A slot is reused only after terminal cleanup and generation advance.

### S1-6: admission is not linearized with shutdown

**Verdict: major omission from the audit.**

Spawn methods first load `Running` and later enqueue (`src/multicore.rs:1063-1077`, `src/multicore.rs:1207-1289`). Shutdown changes state independently (`src/multicore.rs:1402-1438`). A spawn that observed `Running` can enqueue after the shutdown cutoff or after a worker's empty-inbox exit check. `spawn_wait` retry loops do not continuously participate in an admission protocol, and receivers accept task messages while draining.

Acquire/Release visibility does not make state-check-plus-channel-send atomic.

**Required correction:** define one admission linearization point. An in-flight request is either accepted-before-close and owned by a shard, or rejected after close with its future returned/dropped and an explicit result. Successful admission must be acknowledged or otherwise proven owned.

### S1-7: multishot accept lacks capability, bounds, and terminal ownership

**Verdict: confirmed.**

Every io_uring accept is built as `AcceptMulti` (`src/io/uring.rs:679-692`), while support is reported unconditionally (`src/io/uring.rs:1685-1687`). Results accumulate in a per-token `VecDeque` (`src/io/uring.rs:337-349`, `src/io/uring.rs:1673-1682`) without a bound. Accepted descriptors are raw integers in `CompletionKind`, so abandoned queued results do not have RAII close ownership. A terminal CQE removes operation state, but `AsyncTcpListener` retains its token whenever the backend claims multishot support (`src/net.rs:43-89`).

**Required correction:** runtime opcode/flag probing, bounded `OwnedFd` delivery, explicit overflow-close policy, listener-drop cancellation, and terminal rearm/fallback.

### S1-8: scheduler/runtime ownership and affinity APIs contradict the contract

**Verdict: confirmed; physical-core selection is an additional omission.**

`MultiCoreRuntime::block_on` constructs a separate single-thread runtime/backend, and `block_on_cpu` only validates the index before running on the caller thread (`src/multicore.rs:1565-1588`). The primary HTTP example accepts on a reuse-port shard but sends every connection through global cross-core spawn (`examples/http_echo.rs:249-268`). `spawn` scans inbox depths, which measure ingress backlog rather than task load (`src/multicore.rs:1188-1211`). `spawn_local` still requires `Send` and writes to an unbounded TLS `Vec` (`src/multicore.rs:44-56`, `src/multicore.rs:1161-1180`).

The default worker count uses logical CPUs (`num_cpus::get`), affinity maps worker IDs directly to CPU IDs, and binding failure is only a warning (`src/multicore.rs:843-857`, `src/multicore.rs:530-553`). This does not establish one worker per available physical core and fails on SMT or non-contiguous cpusets. Worker readiness is sent before `run` performs affinity and TLS setup (`src/multicore.rs:930-934`, `src/multicore.rs:364-369`).

**Required correction:** enumerate allowed physical cores at startup, fail or record an explicit architecture exception if affinity cannot be established, acknowledge readiness only after owner-thread initialization, and keep accepted work on its accepting shard through a bounded local spawner that supports `!Send` futures.

### S1-9: additional cancellation and ownership omissions

These were absent from the audit and must be scheduled:

1. `CancellationToken::cancel` only stores a flag, while `CancellableFuture` checks it only when polled and registers no cancellation waker (`src/cancellation.rs:24-30`, `src/cancellation.rs:56-82`). Cancellation of a permanently pending inner future may never be observed.
2. Signal handling creates an unrelated flag and sends directly to shard inboxes without entering the runtime lifecycle or notifying eventfds (`src/multicore.rs:1388-1396`, `src/signal.rs:57-96`). Its blocking send can stall on a full inbox. Signal masking is done only inside the newly spawned waiter thread, after workers already exist.
3. The single-thread `Runtime::shutdown_flag` is public but not consumed by its run loop (`src/executor.rs:37-49`).
4. The global runtime is held in `OnceLock<Arc<_>>`; global shutdown can signal it but cannot consume and join its owner (`src/multicore.rs:1631-1660`).
5. QUIC stores only a shard-local numeric I/O token in atomics on an `Arc` socket/poller (`src/quic.rs:99-143`, `src/quic.rs:211-256`). Concurrent polls can submit on different shard drivers and overwrite the token; drop clears the numeric slot through whichever runtime thread is current. This can orphan the original operation, clear unrelated local state, or panic outside a runtime.

## 3. Atomic-ordering assessment

No current S0/S1 finding is repaired by globally strengthening atomic orderings.

- Runtime-state Release publication and Acquire admission reads are adequate for visibility, but not for atomic admission ownership.
- Per-core shutdown Release stores and Acquire loads are adequate for the flag itself, but the flag is not a lifecycle protocol.
- Relaxed task/round-robin increments are adequate for uniqueness/distribution until explicit truncation and generation reuse break identity.
- The worker-exit Relaxed store is not currently used to publish a result; `join` supplies synchronization. It should be replaced by an explicit completion/result channel if non-joining observation is required, not changed in isolation.
- `CancellationToken` Release/Acquire is adequate for the boolean; the missing wake and lost-wake-safe registration protocol are the defect.
- The `sleeping` flag plus conditional eventfd write (`src/multicore.rs:475-491`, `src/multicore.rs:994-999`) lacks a demonstrated linearization proof. A send concurrent with the worker's final inbox check can be observed neither by that check nor by a stale conditional load, delaying work until park timeout. Prefer an unconditional coalesced notifier or a small proven state machine, and verify it with Loom.

Every remaining atomic must document the state transition it publishes and the data protected by that transition. `SeqCst` is not a substitute for an owner acknowledgement.

## 4. Target architecture

The target is one deep shard-runtime module behind a small control interface. Callers must not learn or coordinate ring, queue, TLS, cancellation, or teardown internals.

```text
RuntimeOwner (non-cloneable, control thread)
  owns lifecycle authority, physical-core plan, worker JoinHandles
  owns one RuntimeHandle value and all startup/rollback responsibility
       |
       +-- RuntimeHandle (cloneable, Send + Sync)
             bounded admission/cancel/control messages only
             no backend, task-arena, timer, or queue references
             |
             +-- ShardHandle[physical core]
                   bounded sender + lifetime-safe notifier

Worker thread / ShardRuntime (!Send + !Sync)
  LocalSpawner<!Send futures>  TaskArena  ReadyQueue  TimerWheel
  IoDriver (!Send + !Sync)     OperationArena         PerShardMetrics
```

### 4.1 Ownership seams

- `RuntimeOwner` is the only module that starts, stops, and joins workers. Dropping it enters immediate shutdown and still joins every worker.
- `RuntimeHandle` is capability-limited. It can request work through bounded channels but cannot access local mutable state.
- Each `ShardRuntime` directly owns exactly one driver, task arena, timer wheel, local queue, and inbox for its entire worker lifetime.
- `LocalSpawner` is available only inside the owning shard and accepts bounded `!Send` futures. `RemoteSpawner` requires `Send` and either an explicit destination or a documented dispatcher policy.
- With `SO_REUSEPORT`, the accepting shard owns the connection and spawns its handler locally. No work stealing or task migration is introduced.

### 4.2 Task and wake model

Task identity is `TaskKey { runtime_generation, shard, slot, generation }`. The per-shard arena is fixed/bounded; a live slot is never overwritten.

Task state is:

```text
Vacant -> Ready -> Running -> Pending -> Ready
                    |           |
                    +---- CancelRequested ----> Terminal -> Vacant(next generation)
```

The standard waker is implemented with safe `Arc`/`Wake` state:

- it contains the full `TaskKey`, an atomic scheduled/closed state, and a weak/lifetime-safe shard notifier;
- it never contains a pointer or reference to `ShardRuntime`, `TaskArena`, or `ReadyQueue`;
- at most one ready notification exists per live task;
- the worker clears `scheduled` immediately before polling, allowing one wake during the poll to enqueue the next turn;
- cancellation marks the wake state closed before task storage is reclaimed;
- a late wake after task/runtime teardown is a no-op;
- an unexpected bounded-mailbox-full condition sets a sticky overflow signal that makes the worker rescan scheduled task records; it is never silently dropped.

Each event-loop turn has budgets for messages, CQEs, task polls, and timers. No one source can prevent the loop from returning to the others.

### 4.3 I/O operation model

Driver submission returns `Result<OpKey, SubmitError>`. `OpKey` is shard plus slot plus generation; sentinels are forbidden.

```text
Allocated
  -> SQQueued
  -> Submitted
  -> Completing | CancelRequested
  -> TerminalCQE
  -> Delivered | Discarded
  -> Reclaimed(next generation)

Allocated/SQQueued may instead -> Failed -> Delivered -> Reclaimed
```

Operation state owns:

- an owned descriptor lease acquired when submission is accepted;
- every buffer and kernel-visible C structure;
- stable boxed/pinned vectored metadata and transitive pointees;
- waiter/delivery state separately from kernel state;
- multishot substate, bounded accepted `OwnedFd` results, and a terminal marker.

Dropping an `IoFuture` removes or marks the waiter and may request cancellation. It never releases descriptor, pointer, or buffer ownership before terminal completion. Shutdown retains the entire operation arena until every operation is terminal, then closes the driver.

### 4.4 Runtime context model

The local runtime owns its driver and timer for its whole lifetime; `block_on` reuses them. It is `!Send + !Sync`.

TLS installation is one scoped context guard which saves and restores the previous context. Access is closure-based, for example conceptually `with_io_state(|state| ...)`, with a higher-ranked lifetime that cannot be returned. No public setter and no forged `'static` reference remain.

`MultiCoreRuntime` never creates another runtime/backend from `block_on`. A control-thread blocking facade may submit a task and wait for an acknowledged result, but calling it from a worker is rejected. Running on a particular shard is an asynchronous `run_on(shard, future)` operation, not caller-thread execution disguised as `block_on_cpu`.

### 4.5 Lifecycle model

One coordinator lifecycle is used by explicit shutdown, signals, `Drop`, and global helpers:

```text
Initializing -> Running -> Quiescing -> Cancelling -> DrainingIo -> Joined
       |          |             |             |             |
       +----------+-------------+-------------+-----------> Failed
```

- Startup is transactional. The owner retains every worker handle and notifier until all workers acknowledge affinity, TLS, driver, and event-loop readiness.
- `Quiescing` is the admission cutoff. Every in-flight admission is acknowledged as owned-before-close or rejected-after-close.
- Graceful shutdown has a deadline. At expiry it enters `Cancelling`.
- Cancellation drops task futures on owner shards, unregisters timers, and requests driver cancellation without reclaiming operation memory.
- `DrainingIo` waits for terminal completions, closes surplus accepted descriptors, unregisters wake resources, and reports per-shard resource counts.
- `join` attempts every handle and aggregates all errors. Terminal state is published only after every worker resource is reclaimed.

## 5. Required invariants

The implementation is not complete until all of these are mechanically enforced or tested.

### Memory and lifetime

1. No safe interface can return a runtime-state reference whose lifetime exceeds the scoped installed context.
2. I/O, timer, task, and core TLS are restored on normal return, panic, and nesting.
3. A `Waker` may be cloned, moved, woken, and dropped on any thread and after task/runtime teardown without accessing freed shard memory.
4. Kernel-visible pointers and every transitive pointee remain address-stable from SQE construction through terminal completion.
5. Pending operations are non-cloneable and retain descriptor, buffer, and argument ownership through terminal completion.
6. Uninitialized storage is exposed only through valid spare-capacity/`MaybeUninit` or raw-syscall interfaces; logical length is published only after a checked byte count.

### Shard ownership and scheduling

7. Local driver, task arena, ready queue, timer wheel, and mutable metrics are `!Send + !Sync` and accessed only by their owning worker.
8. Cross-core traffic uses bounded messages or an equivalent bounded, proven notification structure. No wake, cancel, admission, or shutdown message is silently discarded.
9. Task and operation identities contain shard, slot, and generation; stale or wrong-shard identities cannot touch a live replacement.
10. At most one scheduled notification exists per live task, and each loop turn has a finite poll budget.
11. A successful reuse-port accept keeps the handler on the accepting shard unless an explicit bounded migration protocol is introduced and documented.
12. Startup creates exactly one driver/runtime per worker and maps workers to allowed physical cores, not blindly to logical indices.

### Cancellation and lifecycle

13. Admission has one linearization point. After the cutoff no new task becomes owned, and every success is either run or explicitly completed as cancelled.
14. Cancellation both records intent and causes the owner to observe it; check/register/recheck prevents lost cancellation wakes.
15. A cancellation success is acknowledged by the owner shard and means the future has stopped or reached a precisely documented accepted state.
16. User-future abandonment is distinct from kernel-operation termination.
17. Runtime termination requires zero live tasks, registered timers, queued SQEs, submitted operations, queued accepted descriptors, and live workers.
18. No startup, signal, shutdown, `Drop`, or worker-error path detaches a worker.
19. Every accepted descriptor not delivered to a stream is closed exactly once.

### Atomics and observability

20. Every atomic has a documented state transition and synchronization purpose. Ordering stronger than Relaxed is used only when it publishes or consumes associated data.
21. Notification proves “work observed or wake delivered”; timeout is not the correctness mechanism.
22. Terminal reports expose per-shard outstanding task/timer/operation/descriptor counts and aggregated worker failures.

## 6. Ordered implementation batches

### Batch 1 — memory-safety containment and owner-typed foundations

Remove every confirmed S0 safe-API path: scoped TLS, local-only driver types, safe wakers and generational task identity, stable kernel arguments, descriptor leases, and initialized-buffer correctness. Add a task poll budget because the new wake protocol must not preserve the current starvation behavior.

Completion of this batch does **not** make the runtime production-ready; cancellation, admission, and shutdown semantics remain blocking work.

### Batch 2 — fallible operation and cancellation state machines

Make submission fallible; introduce generational operation slots; distinguish waiter abandonment, cancellation request, terminal completion, delivery, and reclamation; route acknowledged task cancellation to one owner; make `CancellationToken` wake-safe; bind QUIC operation identity to its originating shard.

### Batch 3 — transactional startup and bounded shutdown

Split `RuntimeOwner` from `RuntimeHandle`; make startup rollback total; acknowledge readiness after affinity/TLS/driver initialization; linearize admission close; implement graceful deadline, immediate escalation, program-controlled driver cancellation, terminal drain, all-worker join, aggregated errors, and terminal resource reports. Route signals, `Drop`, and global helpers through the same lifecycle.

### Batch 4 — accept/backpressure/affinity restoration

Probe multishot capabilities at runtime; bound accept results and local spawn; own accepted descriptors; implement terminal rearm/fallback; enumerate physical cores/cpusets; replace fake `block_on_cpu`; keep reuse-port handlers local; document the dispatch model and any architecture exceptions.

### Batch 5 — gates, stress evidence, and performance work

Repair the existing build/test gates, run Miri/Loom and Linux fault/stress suites, establish repeated Linux baselines against monoio/compio, and only then optimize. Throughput, p99, CPU, allocation, syscall, and cache evidence remain mandatory. No safety or lifecycle claim is accepted from a benchmark.

## 7. Detailed specification for Batch 1

### 7.1 Goal and exit condition

Batch 1 establishes type-enforced shard ownership and removes all currently identified safe paths to use-after-free, data race, invalid kernel pointer, descriptor-reuse corruption, and uninitialized safe slices.

It exits only when:

- no local driver or `CpuIoState` implements `Send` or `Sync`;
- no safe TLS getter returns a reference and no public context setter exists;
- no task waker contains `Rc`, a raw queue pointer, or a custom unsafe vtable;
- no live task is addressed by sequence alone;
- no kernel-visible pointer refers to inline/movable metadata;
- every accepted operation owns a descriptor lease until terminal completion;
- no fallback read forms an initialized slice over uninitialized bytes;
- scheduler polling is bounded per turn;
- all targeted static, Miri, Loom, and Linux tests below pass.

### 7.2 Explicit non-goals

Batch 1 does not finalize public graceful-shutdown outcomes, signal integration, multishot fallback policy, global-runtime reclamation, or performance tuning. It may introduce the internal types those later batches require. Any compatibility shim that would recreate unsafe ownership must be removed or return an explicit unsupported/error result rather than preserve behavior.

### 7.3 Work package A — local driver and scoped runtime context

Planned scope: `src/cpu.rs`, `src/io/mod.rs`, all backend modules, `src/executor.rs`, and `src/multicore.rs`.

1. Remove `Send + Sync` from the local driver interface and remove unsafe `Send`/`Sync` implementations from `CpuIoState`, `UringBackend`, and `EpollBackend`.
2. Replace `Arc<dyn IoProvider>` inside shard state with direct owner-thread storage (`Box`, concrete enum, or another non-shared local representation). A separate `ShardHandle` may be `Send + Sync`, but it exposes only bounded commands and notification.
3. Make the single-thread `Runtime` explicitly `!Send + !Sync`. Construct its driver/timer once in `Runtime::new`; every `block_on` reuses that owned state.
4. Replace `set_current_*`/`clear_current_*` pairs with one `RuntimeContextGuard` that snapshots and restores I/O, timer, task, and core context in reverse order.
5. Replace `io_state()`/`try_io_state()` reference-returning interfaces with closure-based access whose state lifetime cannot escape. Make installation functions crate-private.
6. Define nested behavior explicitly: same-runtime re-entry is rejected unless supported; a scoped foreign runtime restores the previous context on return/unwind. Calling a control blocking facade from a worker is rejected.
7. Remove or internalize the unused public `Runtime::shutdown_flag`; do not leave a field that implies unimplemented lifecycle semantics.

Acceptance evidence:

- compile-fail/static-assert tests prove local runtime, driver, and state are not `Send`/`Sync`;
- a catch-unwind test panics inside `block_on`, then runs new I/O and timer work without observing stale context;
- a nesting test proves the outer context is restored;
- it is impossible to compile code that returns the local state reference from a context closure;
- runtime move/re-entry tests cannot invalidate internal addresses.

### 7.4 Work package B — safe task identity, wake, and fairness

Planned scope: `src/waker.rs`, `src/task.rs`, `src/executor.rs`, `src/multicore.rs`, and timer tests.

1. Introduce bounded per-shard task arenas and full `TaskKey` identity before replacing the waker. A slot cannot be reused while an old queued notification remains capable of targeting it.
2. Implement the waker with safe `Arc`/`Wake` machinery. `TaskWake` contains only its full key, scheduled/closed atomic state, and a weak or otherwise lifetime-safe notifier. It never points into the shard runtime.
3. Deduplicate wakes with a state transition. The worker clears the scheduled state immediately before poll. A wake during poll can enqueue one subsequent turn; duplicate wakes coalesce.
4. Size the bounded ready mailbox from the maximum active-task invariant. If the mailbox is unexpectedly full, set a sticky overflow bit and ring the notifier; the worker rescans scheduled task records. Never report success after dropping a required wake.
5. Mark the wake state closed before task future/storage reclamation. A late cross-thread wake does nothing.
6. Add a configurable per-turn task-poll budget. Returning to messages, I/O, timers, cancellation, and shutdown is mandatory when the budget expires.
7. Keep any same-thread fast path behind the same safety/state interface; do not create a second raw local-waker implementation.

Acceptance evidence:

- cross-thread clone/wake/drop under contention;
- escaped waker after task cancellation, runtime shutdown, and runtime drop;
- executor/runtime movement after waker creation;
- duplicate wake storm keeps ready depth bounded;
- a self-waker beside an I/O completion, timer, remote message, and shutdown request demonstrates bounded progress for each;
- stale-key and slot-reuse tests prove no ABA target;
- Miri covers local waker/task teardown; Loom covers scheduled/closed/notify transitions and the overflow fallback.

### 7.5 Work package C — stable operation memory and descriptor leases

Planned scope: `src/io/mod.rs`, `src/io/uring.rs`, `src/io/future.rs`, `src/fs.rs`, `src/net.rs`, and any resource wrappers that construct `Op`.

1. Remove `Clone` from `PendingOp` and make accidental clone a compile error.
2. Replace raw-fd-only operation identity with an internal descriptor lease acquired at operation acceptance. Use an owned duplicate or a shard-local shared owner whose lifetime cannot end before terminal completion.
3. Convert accepted raw descriptors to `OwnedFd` immediately. Completion queues and error paths own them until transfer to `TcpStream`; dropping an abandoned result closes it exactly once.
4. Allocate vectored metadata as stable boxed/pinned slices before building the SQE. Validate that buffer allocations and every C pointer reachable from `msghdr` remain stable independently of slab/vector relocation.
5. Store SQE state and operation resources together in an operation arena. `IoFuture::drop` changes only waiter/delivery state; it cannot clear or reclaim kernel-owned resources.
6. Add a minimal driver teardown safety rule for this batch: operation resources cannot be dropped while the driver considers the operation queued/submitted. Full deadline/escalation semantics land in Batches 2–3, but unsafe early reclamation is forbidden now.

Acceptance evidence, on Linux with io_uring:

- delayed-flush `readv`/`writev` for one, two, three, four, and heap-spilled iovec counts;
- forced slab/vector relocation between SQE construction and submission;
- drop source wrapper and aggressively reuse numeric fds before flush; the operation still targets its leased open-file description;
- drop `IoFuture` before submit and after submit; buffers/descriptors remain until a terminal result;
- SQ-full and enter-failure hooks do not return a live token from a reclaimed operation;
- sanitizer/fault-injection runs show no invalid kernel pointer access or leaked accepted descriptor.

### 7.6 Work package D — initialized-buffer correctness

Planned scope: `src/buffer.rs`, `src/io/epoll.rs`, `src/io/kqueue.rs` if it ceases to be an alias, and fallback-backend tests.

1. Add a buffer spare-capacity interface based on `MaybeUninit<u8>` or use a raw syscall destination. Do not set logical length before the OS write completes.
2. For safe `Read`/`recv_from` interfaces that require `&mut [u8]`, use initialized storage as the initial containment implementation.
3. Validate `0 <= bytes_read <= requested_capacity` before publishing length.
4. Apply the rule consistently to file, TCP, UDP, and vectored fallback reads.

Acceptance evidence:

- Miri exercises file/UDP/fallback read paths and cancellation/drop edges;
- short read, EOF, interrupted, would-block, and error paths never expose or retain an invalid length;
- a backend parity test returns identical initialized bytes and lengths for io_uring and fallback implementations.

### 7.7 Work package E — review and architecture evidence

The Batch 1 task card should restrict edits to the files named above plus dedicated tests and architecture documentation. Its architecture audit must answer, for every changed seam:

- Did it add shared state, locks, atomics, runtime creation, blocking calls, or cross-core traffic?
- Why is each `Arc`/atomic in the wake interface necessary at the cross-thread seam rather than in shard-local hot state?
- What bounds the task arena, ready mailbox, and overflow recovery?
- Which type prevents local driver/state from crossing a thread?
- Which owner retains each kernel pointer, buffer, and descriptor at every operation state?

Required gates:

1. `cargo fmt --all -- --check`
2. `cargo check --locked --all-targets --all-features`
3. `cargo clippy --locked --all-targets --all-features -- -D warnings`
4. `cargo test --locked --all-targets --all-features`
5. `just arch-audit` and `just diff-audit`
6. targeted Miri for TLS, waker, task teardown, pinning, and fallback-buffer tests
7. targeted Loom for wake/close/overflow notification state
8. Linux io_uring delayed-submit, fd-reuse, SQ-pressure, and sanitizer/fault-injection tests

The current repository's pre-existing all-target gate failures must be recorded separately from Batch 1 regressions, then repaired or explicitly included in the task card. No safety finding may be closed merely because library-only compilation succeeds.

### 7.8 Batch 1 blocking review checklist

Reject the batch if any of the following remains:

- a local backend/state is reachable through cloneable `Arc` or implements `Sync`;
- a safe function returns TLS-backed runtime state by reference;
- a waker contains raw shard memory or uses non-atomic cross-thread lifetime management;
- a ready notification can be silently dropped;
- task identity can address a live replacement after slot reuse;
- an SQE points to inline or otherwise movable argument metadata;
- a pending operation can outlive the descriptor identity it was created for;
- `IoFuture::drop` reclaims resources before terminal completion;
- a fallback path sets length before initialization;
- a self-waker can prevent the event loop from returning;
- the patch creates a new runtime/backend in a request, connection, task, or worker-local compatibility path.

## 8. Verification limits and follow-up evidence

This verification is static. It independently matched the audit's important S0/S1 claims to the current source and corrected scope, causality, and ordering, but it did not reproduce kernel behavior. The following remain evidence requirements rather than confirmed dynamic results:

- Linux delayed-submission reproduction of the inline-iovec pointer defect;
- fd-reuse reproduction across queued and submitted operations;
- actual opcode/flag behavior on kernels without multishot accept;
- sanitizer/Miri/Loom results;
- SQ/CQ pressure and injected submit/enter failures;
- physical-core and cpuset behavior in the deployment environment;
- bounded shutdown under pending accept, read, timer, permanent task, worker failure, and partial startup failure.

Until Batches 1–3 are complete with this evidence, the runtime must remain classified as unsafe for production and unsuitable as a basis for performance claims.
