# Decision Log

每次架构决策记录四件事：

| 日期 | 决策 | 选择 | 拒绝 | 证据 | 推翻条件 |
|------|------|------|------|------|---------|
| 2026-06-27 | Bounded data-plane queues | Core messages use bounded channels; epoll thread-pool completions use bounded channel plus shard-local completion Vec. | Unbounded `SegQueue`; `Arc<std::sync::Mutex<Vec<_>>` completion queue. | `scripts/audit_architecture.sh` has no hard-fail findings; `cargo test --all-features` passes on macOS and Orb Ubuntu. | Re-evaluate if benchmark shows bounded-channel backpressure dominates task dispatch latency. |
| 2026-06-27 | Scoped arena allocation API | `arena::with_bytes` scopes arena slices to a callback. | Returning pseudo-`'static mut` slices from resettable thread-local arena. | Eliminates `static mut` audit hard-fail pattern and prevents slice escape across arena reset. | Re-evaluate if arena users require owned buffers or stable lifetimes. |
| 2026-06-27 | Control-plane runtime helper exception | Keep crate-level and compatibility `block_on` helpers with explicit audit exceptions. | Treating tests/examples/control-plane helpers as hot-path runtime creation. | Exceptions registered in `ARCHITECTURE_EXCEPTIONS.md`; audit hard-fail list is clean. | Remove once `MultiCoreRuntime::block_on_cpu` dispatches through bounded worker messages. |
| 2026-07-11 | Keep incomplete remediation cards YELLOW | Do not claim rm-03/rm-07/rm-10/rm-12/rm-13/rm-14/rm-15/rm-16/rm-17 complete while their required API, ownership, lifecycle, affinity, or toolchain evidence is absent. | Silently widening a card or replacing a missing proof with a weaker substitute. | Orb Ubuntu lib/integration tests, strict check/clippy, and architecture audit pass; implementation status records the exact remaining gaps. | Revisit each card only when its documented acceptance evidence and ownership invariant are implemented. |
| 2026-07-11 | Fallible submission is the public boundary | `IoProvider::submit` returns `Result<IoToken, IoError>`; no token is exposed until descriptor admission and backend operation installation succeed. | Sentinel tokens or synthetic completion tokens for pre-admission failures. | API-wide call-site migration compiles; invalid-fd, unsupported-op, SQ-full, and registration failures are explicit errors; Orb Linux io_uring regressions pass. | Revisit only if a backend needs a richer typed `SubmitError` without weakening the no-orphan invariant. |
| 2026-07-11 | Expand rm-09 call-site scope | Include fs/net/quic/multicore and the NOP example in rm-09 `allowed_files`; all callers must handle the new fallible submit contract. | Partial trait migration that leaves operation owners assuming a token always exists. | The migration is compile-driven and every changed caller now returns or stores the admission error without creating a slot. | Narrow the scope only after a separate compatibility adapter proves identical failure ownership. |
| 2026-07-11 | Global runtime join ownership | Keep worker join handles in a control-plane-only `parking_lot::Mutex` and expose bounded `join_global`/`shutdown_global_with_timeout`; workers never touch this lock. | A `OnceLock<Arc<Runtime>>` that can signal but permanently strands its owner handles. | Global join integration test passes; architecture audit has no hot-path lock finding. | Replace with an explicit non-cloneable process owner if global runtime lifecycle becomes more complex. |
| 2026-07-11 | Layered safety and performance gates | Make static locality contracts, ordinary unit tests, and two 64-case pure lifecycle models the default fast gate; keep named Miri/Linux tests separate; require matching host/workload metadata for HTTP baseline comparison. | Full-runtime or independent-bitset Loom presented as production evidence, default Miri/io_uring stress, and treating HEAD~1 as benchmark data. | `just test-fast` passes 67 tests in about 3.6 s cold/0.3 s warm; comparator checks throughput, p99, and metadata. `just loom` fails until production seams exist. | Add Loom only when the actual wake, cancellation, or sleep protocol can share the modeled state machine. |

## 模板

### YYYY-MM-DD: [决策标题]

**选择了什么：** [最终决策]

**拒绝了什么：** [被否决的替代方案]

**证据：** [数据、分析、或外部参考]

**推翻条件：** [什么情况下该决策需要重新评估]
