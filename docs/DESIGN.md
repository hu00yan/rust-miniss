# Rust-Miniss Design Document

> 本文档是项目所有设计决策的唯一权威记录。

---

## 设计哲学

**thread-per-core + io_uring = 不需要锁。**

每核一个线程，每个线程独享自己的 io_uring 实例、Task 队列、Timer Wheel、Buffer Pool。核间通信用 lock-free 消息队列（仅用于 task spawn 和 shutdown 信号）。

这种模型的关键约束：一个 Task 从创建到完成，永远不会跨线程迁移。

---

## 目标平台

| 层面 | 决策 | 理由 |
|------|------|------|
| **目标平台** | Linux only | io_uring 是唯一目标 |
| **内核基线** | Linux 6.12 LTS | 需 multi-shot accept/recv、provided buffer ring inc、IORING_OP_SENDZC、min timeout |
| **epoll/kqueue** | 保留，不改，不优化 | macOS/旧内核用于开发，不作为性能目标 |
| **硬件** | x86_64 / ARM64 | 线程 pin 到物理核 |

构建时如果内核 < 6.12，编译直接报错要求升级内核。

### 内核特性的利用策略

| 特性 | 首次出现 | 在运行时中的用途 |
|------|---------|-----------------|
| Multi-shot accept | 5.19 | 一个 SQE 持续接收新连接，无 re-arm syscall |
| Provided buffer ring | 5.19 | 内核直接把数据写入 ring buffer，零分配 |
| Send/recv bundles | 6.10 | 单次 recv 消费多个 provided buffer，减少 SQE 数 |
| **Native bind/listen** | **6.11** | 服务器启动路径完全避 open/bind/listen syscall |
| **IORING_SETUP_DEFER_TASKRUN** | **6.11** | 远端 posting 优化，多核间减少唤醒延迟 |
| **Provided buffer inc (IOU_PBUF_RING_INC)** | **6.12** | 大 buffer 被多次小 recv 分段消费，消除内存放大 |
| **Min timeout** | **6.12** | 低负载时 `submit_and_wait` 更精细的等待策略 |
| Registered buffer cloning | 6.12 | 大 buffer 注册从 1s 降到 17µs |
| Ring resize | 6.13 | 运行时动态调整 ring 大小 |
| DMA-BUF zero-copy recv | 6.16 | 真正的零拷贝网络接收 |

---

## 调度模型

```
每一个 CpuCore {
    task_queue: FxHashMap<TaskId, Task>    // 所有活跃 task
    ready_queue: SegQueue<TaskId>          // 可被 poll 的 task ID
    timer_wheel: TimerWheel                // 本核定时器
    io_backend: UringBackend               // 本核 io_uring
}
```

### 事件循环（单次迭代）

```
1. process_messages()    — 接收跨核消息（新 task / shutdown）
2. poll_io()             — 收割所有可用 CQE
    2a. 先 drain completed_ops （错误/不支持操作的快路径）
    2b. 遍历 CQ，对每个 CQE：
        - multi-shot accept → 压入 accept_results，wake accept task
        - 普通 CQE → 如果 pending_task 存在 → inline poll
                      否则 → completed_io map + waker.wake()
    2c. flush() — 提交 inline poll 过程中产生的新 SQE
3. execute_tasks()       — poll 所有 ready task
4. flush()               — 一次 io_uring_enter 提交所有批量 SQE
5. process_timers()      — 过期 timer → waker.wake()
6. park_if_idle()        — submit_and_wait(1) + min timeout
```

### SQE 提交策略

**方案 B：事件循环最外层统一提交。**

```
Task::poll() 可能产生 0~N 个 SQE，这些 SQE 塞进 SQ 但不立即提交。
事件循环末尾调用一次 flush() → 一次 io_uring_enter 提交所有累积 SQE。

例外：inline completion 路径中，如果 inline poll 产生了新 SQE，
      立即 flush，避免等待整个事件循环周期。
      这发生在 poll_io() 内部的 needs_flush 路径。

优化：当 SQ occupancy > 75% 时主动提前 flush，避免 push 失败回退。
```

```
┌─────────────────────────────────┐
│ poll_io()  drain CQE            │
│   ├ inline poll → task 产生 SQE │
│   └ flush() 立即提交（例外）     │
├─────────────────────────────────┤
│ execute_tasks()  poll task       │
│   └ 每个 poll 可能 push SQE     │
├─────────────────────────────────┤
│ flush() 一次提交所有 SQ（常规）   │
└─────────────────────────────────┘
```

### 跨核协作

只做两件事：
- `spawn_on_core(core_id, task)` — 跨核扔一个 task 过来
- `shutdown()` — 所有核广播 shutdown 信号

不做 work-stealing，不做任务迁移。

新连接的分发：用 `SO_REUSEPORT` 让内核负载均衡。不在运行时层做。

---

## I/O 模型

### io_uring 配置

| 参数 | 值 |
|------|-----|
| ring 大小 | 4096 entries |
| CQE 数量 | 4096 |
| 提交模式 | 普通（非 SQPOLL，减少内核线程竞争） |
| 特性 | `IORING_FEAT_FAST_POLL`、`IORING_FEAT_EXT_ARG` |

### Slab 分配器（PendingOp 管理）

使用 `slab::Slab<PendingOp>`，内部维护嵌入 slot 的 freelist（空闲 slot 的内存直接存放下一个空闲 slot 的 index，无需额外的 Vec 或堆分配）。

```
Slab<PendingOp> {
    slots: Vec<Slot<PendingOp>>,     // Slot 内部嵌入 freelist 指针
    len: usize,                       // 已用 slot 数
}
```

- 每次 `submit()` → `pending_ops.insert(pending_op)` → 返回 slot index
- slot index = `user_data` = CQE 匹配 key（O(1) 无 hash）
- 每次 CQE → `pending_ops.try_remove(idx)` → 取出 PendingOp → O(1) 完成
- CQE 的 `user_data` 直接 cast 为 Slab 索引，跳过所有查找表

**关键：slab slot 存的是 PendingOp（请求描述符），不是 Waker。**
Task 通过 `CpuIoState::pending_tasks` 的 `IoToken → TaskId` 映射找到。

### 支持的 I/O 操作

| Op | io_uring opcode | multi-shot | 备注 |
|----|-----------------|-----------|------|
| Accept | `ACCEPT_MULTI` | ✓ | 一个 SQE 管所有新连接 |
| Read | `READ` | — | 普通 buffer，有 memcpy |
| ReadRent | `READ` | — | zero-copy，buffer 所有权转移 |
| Write | `WRITE` | — | 用户数据拷贝进内部 buffer |
| WriteRent | `WRITE` | — | zero-copy |
| Connect | `CONNECT` | — | 异步 TCP 连接 |
| Close | `CLOSE` | — | 异步关闭 fd |
| Fsync | `FSYNC` | — | 落盘 |
| RecvMsg | `RECVMSG` | — | UDP recv |
| SendMsg | `SENDMSG` | — | UDP send |
| Readv | `READV` | — | vectored read |
| Writev | `WRITEV` | — | vectored write |

### Read Buffer 策略

**现状：方案 B（内部 memcpy）**

```
poll_read(user_buf):
    internal_buf = Buffer::new_uninitialized(len)  // 从 pool 取或分配，不零填充
    submit READ(fd, internal_buf)                   // 内核写 internal_buf
    CQE → internal_buf → copy_to(user_buf)         // memcpy
    recycle internal_buf → pool
```

**目标：方案 C（provided buffer ring + 增量消费，6.12+）**

```
poll_read(user_buf):
    ring_buf = buf_ring.take_buffer()              // 从 ring 取，零分配
    submit READ(fd, ring_buf)                       // 内核直接写 ring buffer
    CQE → ring_buf → copy_to(user_buf)              // 仍需 copy（用户传入的是 &mut [u8]），
                                                     // 但 buffer 分配/回收零开销
    // 6.12 增量消费：IOU_PBUF_RING_INC 允许大 buffer 被分段消费
    // 混合流量场景下不会因为 buffer 不够用而被迫新分配
```

真正的 zero-copy 只能通过 `read_rent`（用户传自己的 Buffer 所有权）实现。
`Rent` 模式是 thread-per-core + completion-based I/O 的最佳匹配：
- `read_rent`: 用户提供 Buffer → 内核直接写入 → 返还用户（零拷贝）
- `write_rent`: 用户提供 Buffer → 内核直接读取 → 返还用户（零拷贝）

---

## Waker 设计

**方案 B：thread-local queue，无 Arc，无 Mutex。**

```
MinissWaker {
    task_id: TaskId,
    queue: *const SegQueue<TaskId>,    // 指向本核的 ready_queue
}

wake():
    queue.push(task_id)     // crossbeam::SegQueue，lock-free push
```

- Waker 的 `Rc` 是 `std::rc::Rc`（非 atomic），因为所有 clone/wake/drop 在同线程
- `*const SegQueue` 代替 `Arc<SegQueue>` 避免 atomic refcount
- 没有 per-waker 的 Mutex 或 Atomic 状态机

**Waker 生命周期：**
- `IoFuture::poll()` → `state.store_waker(token, cx.waker().clone())`
- CQE 到达 → `poll_io()` → `state.take_waker(token).map(|w| w.wake())`
- `IoFuture::drop()` → `state.take_waker(token)` 清理

---

## CQE → Future 唤醒

```mermaid
sequenceDiagram
    participant SQ as Submission Queue
    participant Kernel
    participant CQ as Completion Queue
    participant poll_io as poll_io()
    participant Task as Task::poll()

    poll_io->>CQ: drain CQE
    loop 每个 CQE
        CQ->>poll_io: token_id, result, flags
        poll_io->>poll_io: process_cqe(token_id)
        Note over poll_io: slab.try_remove(idx) O(1)
        poll_io->>poll_io: state.take_pending_task(token)
        alt 有 pending_task
            poll_io->>Task: inline poll（同一循环内立即调用 task.poll()）
            Task->>SQ: 可能 push 新 SQE
            poll_io->>poll_io: flush() 提交新 SQE
        else 没有 pending_task
            poll_io->>poll_io: state.insert_completed(token, result)
            poll_io->>poll_io: state.take_waker(token).wake()
            Note over poll_io: waker 将 task_id 压入 ready_queue
            execute_tasks 后续会 poll 这个 task
        end
    end
```

### O(1) 的三层映射

| 步骤 | 数据结构 | 复杂度 |
|------|---------|--------|
| CQE 匹配 PendingOp | Slab index → `try_remove()` | O(1) |
| PendingOp → Task | `FxHashMap<IoToken, TaskId>` | O(1) |
| Task → Waker | `FxHashMap<IoToken, Waker>` | O(1) |
| Waker → ready_queue | `SegQueue::push()` | O(1) amortized |

Inline completion 跳过最后两步：CQE → PendingOp → TaskId → 直接 `task.poll()`，
回到事件循环的当前迭代，减少一次对 ready_queue 的 push/pop roundtrip。

---

## Task 系统

### Task 结构体

```rust
pub struct Task {
    id: TaskId,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    waker: Option<Waker>,    // 缓存的 Waker，首次 poll 时创建，后续复用
}
```

- Task 持有缓存的 Waker。Waker 首次 `get_or_create_waker()` 创建，后续 poll 时克隆（Rc refcount bump，无堆分配）。
- Task 的 re-schedule 通过 `ready_queue.push(task_id)` 触发
- JoinHandle 的 completion 通过 `Arc<parking_lot::Mutex<JoinState<T>>>` 通信（这是唯一需要轻量锁的地方，因为 JoinHandle 可能被跨 task await）

### TaskId 编码

```
bit 63..32 = cpu_id
bit 31..0  = 本核自增序号
```

---

## 生态集成策略（thread-per-core 兼容）

### 核心原则

第三方库的 `spawn` / `Executor` trait 不能在背后另起线程。必须复用当前线程的执行上下文。

### Hyper 集成（未实现，延期）

```
hyper::rt::Read for TcpIo
    → AsyncTcpStream::poll_read （走 io_uring Read，最终 memcpy）

hyper::rt::Write for TcpIo
    → AsyncTcpStream::poll_write（走 io_uring Write）

hyper::rt::Executor<F> for HyperExecutor
    → 当前 CpuCore 的 ready_queue.push(task)
    → 不创建新线程

hyper::rt::Timer for MinissTimer
    → timer::sleep() → TimerWheel
```

**推迟实现。** 当前阶段专注于运行时核心性能，Hyper/Quinn 集成等运行时性能达标后再做。

### Quinn 集成（未实现，延期）

```
quinn::Runtime for MinissQuinnRuntime
    spawn → 当前 core 的 spawn_on_core
    new_timer → timer::sleep
    wrap_udp_socket → 用 io_uring 的 RecvMsg/SendMsg
    now → Instant::now

quinn::AsyncUdpSocket for MinissUdpSocket
    try_send → io_uring SendMsg 提交（不 flush）
    poll_recv → io_uring RecvMsg → CQE
```

**推迟实现。**

### TLS 集成（未实现，延期）

```
TlsStream {
    inner: rustls (直接调用，不依赖 tokio-rustls)
}
```

**推迟实现。** `tokio-rustls` 内部依赖 `tokio::spawn`，在 thread-per-core 模式下无法使用。

---

## 代码结构

```
src/
├── lib.rs              # 入口，pub re-export
├── executor.rs         # 单线程 Executor + Runtime（测试用，主力是 multicore）
├── multicore.rs        # MultiCoreRuntime，CpuCore 事件循环
├── cpu.rs              # CpuIoState，线程本地上下文（raw pointer 访问）
├── task.rs             # Task 结构体，JoinHandle，JoinTaskFuture
├── waker.rs            # MinissWaker，TaskId，VTable
├── buffer.rs           # Buffer 类型，线程本地回收池
├── arena.rs            # bumpalo arena（feature-gated）
├── io/
│   ├── mod.rs          # IoProvider trait，Op enum，IoToken，CompletionKind
│   ├── uring.rs        # UringBackend，Slab<PendingOp>，BufRing，process_cqe
│   ├── future.rs       # IoFuture — 等待 I/O 完成的 Future
├── net.rs              # AsyncTcpListener/Stream，AsyncUdpSocket
├── fs.rs               # AsyncFile（io_uring 文件读写）
├── http.rs             # HTTP Request/Response 解析（简单实现，不依赖 Hyper）
├── tls.rs              # 暂存，未实现（需要 Rustls 直接调用）
├── timer/              # TimerWheel，sleep，timeout，Interval
├── signal.rs           # 信号处理
├── compat.rs           # futures-io ↔ tokio-io compat re-export
├── config.rs           # 常量配置
├── cancellation.rs     # 取消令牌
└── error.rs (在 lib.rs 中) # 错误类型
```

### 模块间依赖关系

```
┌────────────────────────────────────────┐
│  lib.rs  (re-export, public API)        │
│    ↓                                     │
│  ┌─────────────────────────────────────┐│
│  │  multicore.rs  (事件循环,调度器)      ││
│  │    ↓                     ↑           ││
│  │  cpu.rs ←→ io/uring.rs  │           ││
│  │  task.rs ←→ waker.rs    │           ││
│  │  net.rs → io/future.rs  │           ││
│  │  fs.rs  → io/future.rs  │           ││
│  │  timer/  → waker.rs     │           ││
│  └─────────────────────────────────────┘│
└────────────────────────────────────────┘
```

---

## 改进计划（P4→P2→P1→P3）

按优先级排列，每个优先级内由易到难：

### P4: Buffer / Rent 路径修复

- [ ] **UDP `new_zeroed` → `new_uninitialized`**: 内核写 buffer，零填充浪费 CPU 周期
- [ ] **Buffer ring unregister on drop**: `BufRing.mem` 注册给内核后从未注销，存在内存安全问题
- [ ] **`ReadRent`/`WriteRent` `Buffer::with_capacity(0)` 死分配**: submit 路径中创建了无用的空 Buffer
- [ ] **Rent 路径性能调查**: 为什么 rent 路径减半（可能是 2 次 write / request 而非 1 次）

### P2: SQE 批量提交

- [ ] **Preemptive flush**: SQ occupancy > 75% 时提前刷，避免 push 满回退
- [ ] **测量 `submit_and_wait` 频率**: 确保 idle 路径不会过度阻塞

### P1: Slab 分配器

- [ ] **确认 PendingOp 不持有 Waker**: 检查所有 PendingOp 变体，确保没有 waker 字段
- [ ] **自定义 Slab 评估**: `slab` crate 的 freelist 已经嵌入 slot，确认不需要替换

### P3: Waker / 锁消除

- [ ] **消除 `Arc<CpuIoState>` 冗余**: `CpuCore` 的 `io_state: Arc<CpuIoState>` 改为 `CpuIoState` 直接嵌入，thread-local 已经是 raw pointer
- [ ] **`Ordering::SeqCst` 审计**: grep 非测试代码中的 SeqCst，替换为 Acquire/Release

---

## 性能目标

| Benchmark | 当前 | P4后 | P2后 | P1后 | P3后 | 目标 |
|-----------|------|------|------|------|------|------|
| HTTP echo (1 core, 100 conn) | 355K | ~400K | ~450K | ~500K | ~550K | 650K+ |
| Task spawn throughput | 7M/s | 7M/s | 7M/s | 10M+ | 10M+ | 10M+ |
| io_uring NOP throughput | 未测 | 未测 | 未测 | 10M+ | 10M+ | 20M+ |
