# Runtime Safety Contracts

Status: design and enforcement map, verified against the current worktree on 2026-07-12.
Passing these tests is not a proof that the runtime is safe. The contracts below
state only what is mechanically checked and what remains a Linux/kernel obligation.

## Test architecture and cost budget

| Layer | Layout / command | Exact scope | Expected warm runtime |
|---|---|---|---|
| Fast | crate unit tests plus `tests/runtime_safety_models.rs`; `just test-fast` | fmt, negative trait assertions, 64 bounded task sequences, 64 bounded operation sequences | 2-10 s |
| Loom obligation | `just loom` | fails clearly until production synchronization seams are extracted | not runnable |
| Miri | named crate unit tests; `just miri-safety` | TLS restore, waker teardown, fallback buffer publication | 30-120 s |
| Linux | existing named io_uring regressions; `just linux-safety` | kernel lifetime and descriptor ownership | 5-30 s, excluding stress |

`just test-fast` does not invoke Loom, Miri, Linux lifetime/stress targets,
sanitizers, fault injection, sleeps, real sockets, or wall-clock assertions
from the property models. Existing ordinary unit tests may initialize the host
backend; CI separately retains the ordinary integration suite.

## Production seams

The reference models are deliberately independent of the runtime. Production
changes are limited to negative auto-trait markers for shard-local owner types.
No production notifier/cancellation/sleep seam is currently suitable for Loom,
so no independent bitset model is presented as evidence. The Arc-backed bounded queue held by a waker is a cross-thread
notifier, despite the historical `ReadyQueue` alias; shard-local task/ready
ownership remains in the worker and task arena.

## Contract map

| # | Protected state/resource | Unique owner | Permitted cross-thread operation | Forbidden transition/access | Mechanical enforcement | Linux/io_uring gap |
|---|---|---|---|---|---|---|
| C1 | `Runtime`, `CpuIoState`, executor/task arena, timer wheel, driver mutable state | worker/shard thread | bounded commands and notifier tokens only | move/share local mutable owner state | negative `Send`/`Sync` assertions in crate tests; architecture audit | backend thread-affinity under real CQE load |
| C2 | shard-local mutable references | worker/shard thread | handles may carry keys, bounded senders, atomics and owned values | cross-thread handle exposes local reference/raw pointer | partial: positive Send/Sync assertions for `WakerState`, `ReadyQueue`, `TaskId`, and `MultiCoreRuntime`; ordinary late-wake tests; review for new handles | no Rust auto-trait assertion can reject every semantically wrong shared reference; API-surface obligation remains |
| C3 | kernel-pending operation storage | driver operation arena | operation key and cancellation request | clone pending operation or kernel-visible pointees | `PendingOp: !Clone` assertion in its defining module; operation model | pointer stability through kernel terminal CQE |
| C4 | task identity | shard task arena | full `TaskKey { runtime_generation, shard, slot, generation }` | stale/wrong-shard key mutates replacement | layout assertion, ordinary arena tests, task model after every action | `u16` generation/runtime-generation exhaustion is unverified |
| C5 | ready notification bit/entry | live task wake state; consumed by worker | coalesced wake notification | more than one live ready notification | task model and ordinary wake dedup tests | wake/close concurrency lacks a production-linked Loom seam |
| C6 | task/waker lifetime | task owner, then closed wake state | clone/wake/drop via Arc state | late wake accesses retired task/runtime memory | ordinary waker teardown tests, task model, named Miri tests | concurrent teardown lacks a production-linked Loom seam |
| C7 | buffers, descriptor leases, kernel argument memory | driver operation slot until terminal completion | waiter abandonment/cancel request | future drop reclaims resources before terminal completion | operation model; existing fd-lease unit tests | queued vs submitted drop/cancel and terminal CQE ordering |
| C8 | buffer initialized and logical lengths | operation/buffer owner | publish checked completion length | logical length exceeds initialized length or submitted capacity | operation model, ordinary buffer tests, named Miri tests | malicious/impossible CQE lengths from a real backend |
| C9 | cancellation state | task/operation owner | request plus owner acknowledgement | report success without owner-observed state transition | task and operation models; ordinary owner-ack tests | cancel/register concurrency and kernel terminal CQE semantics |
| C10 | admission cutoff | runtime owner plus admission gate | acknowledged pre-cutoff admission | post-cutoff work silently accepted or pre-cutoff work lost | task model and existing admission unit tests | worker exit racing real channel/eventfd traffic |
| C11 | accepted descriptor | accepting driver slot, then exactly one consumer | transfer one `OwnedFd` or owner close | leak, double-close, or duplicate transfer | operation model and ordinary `OwnedFd` tests | multishot overflow, parse failure, cancellation CQEs |
| C12 | operation token namespace | driver arena | return `Result<OpKey, SubmitError>` after valid allocation | sentinel/apparently valid token on rejection | operation model and rejected-submit unit test | SQ full/enter failure injection |
| C13 | worker sleep notification | notifier protocol, observed by worker | set pending and unpark/notify | live target silently loses event while sleeping | explicitly unverified; `just loom` fails | production seam plus eventfd/kernel integration |
| C14 | termination accounting | runtime owner after every shard acknowledgement | shutdown/cancel/drain messages | publish terminal with live tasks, timers, queued/submitted ops or retained accepts | component models prove their own owner-finalize cleanup only | production timers/messages/multiple ops and a full zero-resource report are not mechanically exposed |
| C15 | scoped runtime TLS | current thread and context guard | nested scoped closure access | reference escape or failed panic restoration | ordinary panic/nesting tests and named Miri tests | backend syscalls intentionally excluded from Miri |
| C16 | event-loop fairness | shard worker | bounded polls/messages/completions per turn | self-waker prevents I/O/timer/control progress | ordinary poll-budget tests; task model notification bound | tail latency under real load is performance evidence, not a safety proof |

## Exact generated properties

The task-slot model generates sequences of at most 32 actions from allocate,
wake, poll, cancel request, owner cancellation, terminal cleanup, slot reuse,
and stale wake/cancel. After every action it checks: identity matches the unique
live occupant; stale keys cannot change it; notification count is zero or one;
owner acknowledgement follows an observed transition; and a closed task treats
late events as no-ops.

The operation model generates sequences of at most 32 actions from allocate,
queue, submit, abandon waiter, request/observe cancellation, complete, deliver,
discard, and reclaim. After every action it checks: resources remain owned until
terminal completion; no token exists after rejection; cancellation success is
owner-observed; delivery and discard are exclusive; accepted descriptors are
transferred or closed once; length publication is bounded; and reclamation is
terminal-only.

Each proptest uses 64 cases and shrinks the action vector. Neither model uses
threads, sleeps, clocks, sockets, file descriptors, or a runtime backend.

## Exact enforcement index

- C1-C3: `executor::tests` static assertions for `Runtime`/`Executor`;
  `cpu::tests` for `CpuIoState`; `multicore::tests` for `CpuCore`;
  `waker::tests` for `TaskArena`; `timer::tests` for `TimerWheel`;
  `io::epoll::locality_contract_tests` for `EpollBackend`; and Linux-only
  `io::uring::tests` for `UringBackend: !Send + !Sync` and
  `PendingOp: !Clone`.
- C4-C6, C9-C10, C16:
  `tests/runtime_safety_models.rs::task_slot_actions_preserve_identity_and_ready_invariants`;
  `waker::tests::task_arena_rejects_stale_key_after_slot_reuse`;
  `waker::tests::escaped_waker_wakes_from_other_thread_and_late_wake_is_harmless`;
  `executor::tests::poll_budget_bounds_self_waking_task_and_runs_peer`;
  `multicore::tests::admission_rejects_spawn_after_shutdown_cutoff`; and
  `multicore::tests::cancel_reports_owner_acknowledgement`.
- C13 and the concurrent portions of C5/C6/C9 remain explicit obligations.
  `just loom` exits non-zero and lists the missing production seams; no
  standalone reference bitset is counted as synchronization evidence.
- C7-C8, C11-C12, C14:
  `tests/runtime_safety_models.rs::operation_actions_retain_resources_until_terminal_reclamation`;
  `io::descriptor_lease_tests::lease_survives_user_fd_close_until_drop`;
  `io::accept_ownership_tests::accepted_descriptor_closes_when_completion_is_dropped`;
  `io::future::tests::rejected_submit_future_returns_error_without_runtime_or_slot`;
  and the named `just linux-safety` regressions.
- C8 and C15 Miri targets are the exact tests listed by `just miri-safety`:
  `cpu::tests::runtime_context_guard_restores_timer_task_and_core_on_nesting`,
  `executor::tests::tls_context_restores_after_nesting_and_panic`, the four
  named `waker::tests`, and the two named `buffer::tests`.

## Explicitly unverified obligations

- generation and runtime-generation wrap/exhaustion policy;
- kernel pointer stability and descriptor leases through actual queued and
  submitted io_uring operations;
- SQ pressure, `io_uring_enter` failure, cancellation terminal CQEs, multishot
  accept overflow/rearm, and shutdown drain;
- a production termination snapshot proving zero tasks, timers, messages,
  queued/submitted operations, and retained accepted descriptors;
- performance and p99 tail-latency non-regression.

`just linux-safety` lists/runs the available Linux evidence and prints missing
obligations. Model tests never claim to cover those kernel properties.
