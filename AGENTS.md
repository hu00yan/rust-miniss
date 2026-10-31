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

> 2026-10-31 修订：gate 收拢为三层（设计全文与证据：
> `docs/review/gate-hierarchy-2026-10.md`；业主批准记录见文末修订注记）。
> 每个 gate 是一条 `just` recipe。

| 层 | 入口 | 何时跑 | 预算 |
|----|------|--------|------|
| Tier 1 | `just fast`（`test-fast` 为兼容别名） | 开发内循环，每次修改后 | 秒级（热缓存） |
| Tier 2 | `just gate TASK=<task.yaml> BASELINE=<summary.csv> [BASE=<commit>]` | 每次提交前 | 分钟级 |
| Tier 3 | `just nightly`（`test-full` 为兼容别名） | 夜间 schedule、合并集成前手动 | 30-90 分钟 |

Tier 1 只拦最愚蠢的错误。提交 gate 使用真实同机 baseline；Tier 3 是周期
全量兜底，不是每提交义务。

## `just fast`（Tier 1）

`cargo fmt --all -- --check` + `scripts/check_no_ignored_tests.sh` +
`scripts/audit_architecture.sh` + `cargo test --lib` + `cargo test --test
runtime_safety_models`（64-case bounded property models）。

**负清单（硬约束）：Tier 1 的任何测试不得使用 socket、fd、thread、sleep 或
wall clock；protocol-sim 型测试一律不得进入本层。** 不得运行 Loom、Miri、
独立 Linux lifetime/stress jobs。bounded 后端 I/O 回归（`runtime_regressions`
等）属 Tier 2，由 `just check`/`just affected` 承载，不在此层。

## `just check`（Tier 2 核心）

`cargo fmt --all -- --check` + `scripts/check_no_ignored_tests.sh` +
`cargo clippy --all-targets -- -D warnings` + `cargo check --all-targets
--all-features` + `just check-backends` + `just test-regular` + `just docs-audit`。

`cargo test --all-targets` is intentionally not the default check: Cargo treats
benchmark binaries as test targets, and some of them start listener processes or
long-running workloads. Run benchmarks only through an explicit `just bench-*`
recipe and stop/clean every server and client started by that recipe.

**`unsafe-audit` 刻意不在 check 内**：它是 RED（exit 2）语义，被 check 捕获
会沦为 YELLOW（exit 1）；`just gate` 顶层独立运行它。

## `just check-backends`

cfg(io_backend) 后端编译矩阵：Linux 上 `MINISS_IO_BACKEND={io_uring,epoll}`
各做一次 `cargo check --all-targets --all-features`，macOS 编译 kqueue 默认。
仅编译、无测试无行为。这是 P2（未编译回退）防线的本地半边：每个 cfg 组合
必须在它的原生 OS 上保持编译（D16/D22/D39 类缺陷的前置）。

## `just affected`（受影响目标选择；Tier 2 的 gate 步骤）

`scripts/test-affected.sh [--dry-run] [BASE]` 读相对 BASE 的
`git diff --name-only`，经 `scripts/test-affected-mapping.yaml` 把改动子系统
映射为具体测试命令（含该子系统的后端与平台腿）并执行。BASE 缺省依次尝试
`origin/main`、`main`、`HEAD~1` 的 merge-base；任务卡工作流传 baseline 提交。

- **映射表是 gate 契约的一部分**：修改映射表的提交必须附
  `scripts/test-affected.sh --dry-run <BASE>` 的输出，自证没有静默缩减覆盖。
- **拒绝永不静默**：空选择、映射/配置错误以退出码 2 拒绝；兜底规则 `""`
  必须存在。唯一豁免是显式 `MINISS_AFFECTED_ALLOW_EMPTY=1`。

## `just arch-audit`

运行 `scripts/audit_architecture.sh`。扫描 `src/`、`tests/`、`bench/`、
`examples/`。

**Hard fail**（hot path 发现即失败）：`Arc<Mutex` / `Arc<RwLock` / `std::sync::Mutex` / `tokio::sync::Mutex` / `DashMap` / `Runtime::new` / `Builder::new_*` / `block_on` / `spawn_blocking` / `static mut` / `lazy_static` / `once_cell::sync::` / `std::sync::LazyLock` / `unbounded`（channel）

**Needs justification**（不自动失败，需解释）：`Ordering::SeqCst` / `#![allow]` / `unsafe`（需 safety comment）

## `just diff-audit`

对比当前 diff 与任务卡 `allowed_files`。越界改文件 → 打回。

## `just bench-smoke`

运行一次 5 秒 `http_echo --cores 1` HTTP smoke，与显式提供的同机
`summary.csv` 比较并输出 JSON。throughput 回退 >5% 或 p99 增长 >10% 失败；
无 baseline 时明确失败，不把 `HEAD~1` 冒充测量证据。它是 ±40% 量级的仪器
（DECISION_LOG 2026-10-17 行），不得单独支撑 ±5% 的性能主张。

## 专项安全命令

- `just loom`：在生产同步 seam 尚未抽取时必须明确失败；不得用独立 bitset
  reference model 冒充生产并发证据。
- `just miri-safety`：仅 TLS、waker、buffer 的定名测试。
- `just linux-safety`：仅 Linux/io_uring 生命周期回归；非 Linux 明确失败。

## `just nightly`（Tier 3；`test-full` 为兼容别名）

周期/手动全量：`just check` + `just test-feature-matrix` + `just
test-differential` + `just loom` + `just stress` +（Linux 上）`just
linux-safety` +（nightly 工具链可用时）`just miri-safety`。夜间 CI schedule
与合并集成前手动运行；受影响选择看不见的跨子系统语义交互由这一层兜底。

## `just test-mutants-quic`（Tier 3 变异测试）

cargo-mutants，`--file src/quic.rs --file src/multicore.rs`，`--timeout 60`
必须携带（防变异体死循环）。nightly/手动运行，**严禁进入 fast/gate**（Tier 1/2
的秒级/分钟级预算与受影响选择的确定性不可被变异测试污染）。判读纪律：
**存活变体 = 性质/套件对应当补的覆盖洞——补性质（生成器/oracle）或补映射腿，
永不以放宽 oracle、跳过用例换取 GREEN。** 工具缺失时明确失败（exit 2）。

## `just review`

`just review` 是唯一入口；标准/对抗是它的两个模式，不是两个独立 gate。
审查 git diff，每条意见必须附带文件:行号。

**标准 review** — 三类输出：1. Contract violations、2. Missing evidence（无测试/benchmark/注释支持的 claim）、3. Required blocking changes。

**Adversarial review** — 假设 patch 有隐藏错误：跨核锁竞争、thread-local 所有权被破坏、runtime 隐藏在 helper 中、benchmark 造假、tail latency 退化、测试通过但生产异常。

## `just gate`

`just check` → `just affected` → `just arch-audit` → `just unsafe-audit` →
`just diff-audit TASK` → `just bench-smoke BASELINE`（顺序即失败顺序）。
返回码：0=GREEN、1=YELLOW（编译/测试/clippy/选中的腿失败）、2=RED（架构
红线、unsafe 文档缺失、**affected 拒绝——rc=2 的映射/配置错误按 RED 透传，
永不降级为 YELLOW**）、3=RED（越界改文件）。

## Test and process hygiene

- Every network/integration test must have a bounded wait for readiness and
  completion. A test that can leave a worker, server, `wrk`, or benchmark
  process behind is not acceptable in the default test path.
- Prefer `shutdown_with_timeout`/explicit join in cleanup paths. Cleanup must
  run before an assertion can panic, or use a guard that owns the process and
  shuts it down on drop.
- Long-running, Linux-only, QUIC, io_uring, stress, Miri, and Loom tests must be
  ignored or separately gated with a documented command. They must not be
  pulled into Tier 1 (`just fast`) by `--all-targets`.
- When a test times out, record the exact test and backend in
  `docs/review/runtime-safety-findings.md`; do not increase the timeout merely
  to make the suite green.
- Before handoff, inspect and stop any process started by the current task and
  remove only task-owned temporary output. Do not leave benchmark servers or
  clients running in the workspace.

## 修订注记（2026-10-31，业主批准的合同增量）

1. Tier 1 定义收窄为 `just fast`（`test-fast` 别名），负清单从"property
   models 不得用 socket/fd/thread/sleep/wall-clock"扩到整层，protocol-sim
   一律禁入（业主裁定：只拦最愚蠢错误）。
2. `check` 增 `check-backends`（原零引用孤儿配方归位）与 `docs-audit`
   （原仅 1/3 CI 腿）。
3. `unsafe-audit` 维持 `gate` 顶层、刻意不入 `check`（RED 语义不得被
   YELLOW 捕获——业主评审否决并入方案）。
4. 新增 `affected` 受影响目标选择为 gate 步骤；映射表纳入契约，改动需
   dry-run 自证；空选择/配置错误以 rc=2 拒绝。
5. `gate` 序列更新为 check → affected → arch-audit → unsafe-audit →
   diff-audit → bench-smoke；affected 的 rc=2 按 RED 透传。
6. `test-full` 重定位为 Tier 3 周期入口，别名 `nightly`。
7. `arch-audit` 扫描目录修正：`bench/`（非 `benches/`）并补 `examples/`。
8. `review` 收拢单入口，标准/对抗为其两个模式。

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
