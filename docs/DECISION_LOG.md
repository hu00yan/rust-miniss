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

## 模板

### YYYY-MM-DD: [决策标题]

**选择了什么：** [最终决策]

**拒绝了什么：** [被否决的替代方案]

**证据：** [数据、分析、或外部参考]

**推翻条件：** [什么情况下该决策需要重新评估]
