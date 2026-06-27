# Decision Log

每次架构决策记录四件事：

| 日期 | 决策 | 选择 | 拒绝 | 证据 | 推翻条件 |
|------|------|------|------|------|---------|
| 2026-06-27 | Bounded data-plane queues | Core messages use bounded channels; epoll thread-pool completions use bounded channel plus shard-local completion Vec. | Unbounded `SegQueue`; `Arc<std::sync::Mutex<Vec<_>>` completion queue. | `scripts/audit_architecture.sh` has no hard-fail findings; `cargo test --all-features` passes on macOS and Orb Ubuntu. | Re-evaluate if benchmark shows bounded-channel backpressure dominates task dispatch latency. |
| 2026-06-27 | Scoped arena allocation API | `arena::with_bytes` scopes arena slices to a callback. | Returning pseudo-`'static mut` slices from resettable thread-local arena. | Eliminates `static mut` audit hard-fail pattern and prevents slice escape across arena reset. | Re-evaluate if arena users require owned buffers or stable lifetimes. |
| 2026-06-27 | Control-plane runtime helper exception | Keep crate-level and compatibility `block_on` helpers with explicit audit exceptions. | Treating tests/examples/control-plane helpers as hot-path runtime creation. | Exceptions registered in `ARCHITECTURE_EXCEPTIONS.md`; audit hard-fail list is clean. | Remove once `MultiCoreRuntime::block_on_cpu` dispatches through bounded worker messages. |

## 模板

### YYYY-MM-DD: [决策标题]

**选择了什么：** [最终决策]

**拒绝了什么：** [被否决的替代方案]

**证据：** [数据、分析、或外部参考]

**推翻条件：** [什么情况下该决策需要重新评估]
