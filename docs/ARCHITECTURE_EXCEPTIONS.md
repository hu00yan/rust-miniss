# Architecture Exceptions

登记所有对 Architecture Contract（AGENTS.md）的例外。

| # | 文件:行 | 模式 | 理由 | 为何不在热路径 | 预期移除时间 | 审查人 |
|---|---------|------|------|--------------|-------------|-------|
| EX-001 | `src/lib.rs:block_on` | `Runtime::new` / `block_on` | Public convenience wrapper for top-level callers. | Not used by worker hot path; creates a runtime only when user explicitly calls the crate-level helper. | Replace with documented startup API before production release. | pending |
| EX-002 | `src/multicore.rs:MultiCoreRuntime::block_on` | `Runtime::new` / `block_on` | Compatibility wrapper for synchronous callers awaiting one future. | Not invoked by request handling unless user chooses this control-plane API; worker threads still create one backend at startup. | Replace with bounded core-dispatching `block_on_cpu` implementation. | pending |

## 模板

```yaml
exception_id: EX-001
file: src/example.rs:42
pattern: "std::sync::Mutex"
reason: "仅用于 JoinHandle，控制面一次性操作，不在请求路径内"
why_not_hot_path: "JoinHandle 只在 task 创建/结束时接触"
expected_removal: "加入跨核消息传递后移除"
reviewer: "pending"
```
