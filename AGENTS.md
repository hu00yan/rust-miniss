## Agent skills

### Issue tracker

Issues and PRDs live as local markdown files under `.scratch/`. See `docs/agents/issue-tracker.md`.

### Triage labels

This repo uses default triage label strings. See `docs/agents/triage-labels.md`.

### Domain docs

This repo uses a single-context domain docs layout. See `docs/agents/domain.md`.

---

# Architecture Contract

> 以下为不可违反的硬约束。所有代码变更必须逐条遵守。例外必须登记在 `docs/ARCHITECTURE_EXCEPTIONS.md`。

## MUST

| # | Rule | Rationale |
|---|------|-----------|
| 1 | **Data plane uses thread-per-core architecture.** One worker thread per physical core. No work-stealing, no task migration across cores. | Cache locality, zero-contention hot path. Core architectural premise. |
| 2 | **Create exactly one local runtime/executor per worker thread at process startup.** No runtime creation in request/connection/task handling. | runtime/executor construction is expensive and implicitly assumes ownership of the calling thread. |
| 3 | **Hot-path mutable state must be shard-local.** Owned by `CpuCore` or via thread-local storage (`Cell`/`RefCell`/`Rc`). No cross-core shared mutable state in the hot path. | Shared mutable state requires synchronization. Thread-per-core avoids this. |
| 4 | **Cross-core communication uses bounded channels or explicit shard messages.** No unbounded queues for data-plane traffic. | Unbounded channels hide backpressure, cause OOM under load. |
| 5 | **Read-only config may use `Arc<T>`.** Control-plane updates use versioned broadcast or arc-swap style replacement. | Read-only sharing is safe. Mutating shared config from control plane must not block data plane. |
| 6 | **Metrics are per-shard counters, aggregated periodically outside the hot path.** No global atomic counter incremented on every request. | Per-shard counters avoid cache line bouncing. Aggregation is a background task. |
| 7 | **Every worker owns its accept loop or receives connections through an explicit dispatcher.** Document which model is used (SO_REUSEPORT kernel dispatch or explicit shard dispatcher). | Connection ownership determines shard affinity. |
| 8 | **Every change must include an architecture audit explaining whether it added shared state, locks, atomics, runtime creation, blocking calls, or cross-core traffic.** | Audit trail prevents silent erosion of the architecture. |

## MUST NOT

| # | Rule | Rationale |
|---|------|-----------|
| 1 | No `Arc<Mutex<T>>`, `Arc<RwLock<T>>`, `std::sync::Mutex`, `tokio::sync::Mutex`, `DashMap`, or global shared `HashMap` in the **hot path**. | These reintroduce the contention thread-per-core was designed to eliminate. |
| 2 | No runtime/executor creation after startup (`Runtime::new`, `Builder::new_current_thread`, `Builder::new_multi_thread` in request/connection/task handling). | Each runtime binds a thread. Creating runtimes at runtime is a design error. |
| 3 | No `block_on` or `spawn_blocking` in async hot path. | These block the worker thread, defeating async I/O. |
| 4 | No global mutable request state (`static mut`, `lazy_static`, `once_cell::sync::Lazy`). | Global state is invisible shared mutable state — the worst kind. |
| 5 | No unbounded channel (`crossbeam::channel::unbounded`, `tokio::sync::mpsc::unbounded_channel`) for data-plane communication. | Hides backpressure. Use bounded channels with explicit overflow handling. |
| 6 | No global queue without explicit justification in `ARCHITECTURE_EXCEPTIONS.md`. | A global queue in data plane is equivalent to giving up thread-per-core. |
| 7 | No blocking syscall in async context (e.g. `std::fs::read`, `std::net::TcpStream::connect`). | Blocking a worker thread stalls all tasks on that shard. |
| 8 | If a compile error seems to require `Arc<Mutex<T>>`, **stop and propose a shard-local design instead**. | This is a signal that state ownership is wrong, not that a lock is needed. |

---

# Gate Definitions

每个 gate 是一条 `just` recipe。串成 `just gate` 作为提交前检查。

## `just check`

`cargo fmt --all -- --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets`

## `just arch-audit`

运行 `scripts/audit_architecture.sh`。扫描 `src/`、`tests/`、`benches/`。

**Hard fail**（hot path 发现即失败）：`Arc<Mutex` / `Arc<RwLock` / `std::sync::Mutex` / `tokio::sync::Mutex` / `DashMap` / `Runtime::new` / `Builder::new_*` / `block_on` / `spawn_blocking` / `static mut` / `lazy_static` / `once_cell::sync::` / `std::sync::LazyLock` / `unbounded`（channel）

**Needs justification**（不自动失败，需解释）：`Ordering::SeqCst` / `#![allow]` / `unsafe`（需 safety comment）

## `just diff-audit`

对比当前 diff 与任务卡 `allowed_files`。越界改文件 → 打回。

## `just bench-smoke`

运行固定 microbench（`bench_nop`），输出 JSON：`{ bench, throughput_ops_per_sec, p50_us, p99_us, commit, baseline_commit }`。throughput 回退 >5% 标记警告。

## `just review`

审查 git diff，每条意见必须附带文件:行号。

**标准 review** — 三类输出：1. Contract violations、2. Missing evidence（无测试/benchmark/注释支持的 claim）、3. Required blocking changes。

**Adversarial review** — 假设 patch 有隐藏错误：跨核锁竞争、thread-local 所有权被破坏、runtime 隐藏在 helper 中、benchmark 造假、tail latency 退化、测试通过但生产异常。

## `just gate`

`just check && just arch-audit && just diff-audit && just bench-smoke`。返回码：0=GREEN、1=YELLOW、2=RED(架构红线)、3=RED(越界改文件)。

---

# Workflow

## 流程

```
任务卡 (tasks/<id>.yaml) → Grill(审设计) → Code(只改allowed_files) → Gate → Review → Decide
```

## Decide 规则

### GREEN
- `just check` / `arch-audit` / `diff-audit` 通过，`bench-smoke` 有 baseline，review 无 blocking 项
- **操作**：`git commit`，更新 `docs/DECISION_LOG.md`

### YELLOW（可修复）
- 仅有编译/测试/clippy/格式错误
- 送回 coding agent 修复，最多 2 次，仍失败 → RED

### RED（丢弃）
- 架构红线（hot path `Arc<Mutex>`、请求路径创建 runtime、全局 queue）
- 越界改文件、删已有测试/benchmark、性能 claim 无 benchmark 数据、同一模式失败 2 次
- **操作**：`git reset --hard`，记录到 `docs/FAILED_ATTEMPTS.md`

---

# Decision Log

架构决策记录在 `docs/DECISION_LOG.md`。每次记录四件事：选择了什么 / 拒绝了什么 / 证据 / 推翻条件。
