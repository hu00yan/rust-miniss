# Rust-Miniss Design Document

> 本文档是项目设计决策的唯一权威记录，与 `AGENTS.md`（架构合同）、`TODO.md`（任务卡片）构成三角约束体系。

---

## 设计哲学

**thread-per-core + io_uring = 不需要锁。**

每核一个线程，每个线程独享自己的 io_uring 实例、Task 队列、Timer Wheel、Buffer Pool。核间通信用 lock-free 消息队列（仅用于 task spawn 和 shutdown 信号）。

这种模型的关键约束：一个 Task 从创建到完成，永远不会跨线程迁移。

### Thread-per-core 与 Work-stealing 的取舍

**Work-stealing（Tokio 模式）**：Tokio 的多线程运行时把 Future 提交到全局队列，空闲线程从其他线程窃取任务。这让负载自然均衡——无需知道任务轻重就能充分利用所有 CPU。代价是每个 poll 都需要跨线程同步（`std::sync::Mutex` 或 atomic CAS），以及 CPU cache 频繁失效。

**Thread-per-core（本运行时的模式）**：任务从创建到完成绑定在一个核心上，永不迁移。这消除了所有跨线程同步开销，CPU cache 局部性极好。代价是负载必须由上层保证均衡。

不均衡最典型地出现在：

1. **业务逻辑复杂度差异巨大的长连接**：一个 RPC 服务器，有的请求只是简单的缓存查询（微秒级返回），有的触发复杂的 DB JOIN（几百毫秒）。如果两类请求碰巧路由到同一个 Core，那个 Core 被拖死，其他 Core 空闲却无法帮忙。
2. **热门 Partition 的热点问题**：Apache Iggy 直接提到了这一点——一旦消息分区的 Partition 按哈希固定在某个 shard 上，热门分区的读写请求就成为瓶颈，而 work-stealing 的调度器能自动将积压的 Future 调度到其他空闲线程。
3. **新连接负载均衡不足**：`SO_REUSEPORT` 能让内核在多个 socket 间均衡新连接，但内核的均衡粒度是连接级的。如果某些连接上的流量远大于其他连接，它无能为力。

所以 thread-per-core 更适合**任务量可以被请求路由层预先控制均衡**的场景：
- 无状态代理、网关（Nginx/Envoy 模型）
- 明确按 hash 分片的存储系统（ScyllaDB/Redpanda）
- 连接固定、流量均匀的消息系统

而对于任务耗时无法预测的通用业务服务，work-stealing 的价值无法替代。

---

### 控制面与数据面分离

这是保证 thread-per-core 架构不退化到"伪共享"的关键设计边界。

| 维度 | 数据面 (Data Plane) | 控制面 (Control Plane) |
|------|--------------------|----------------------|
| **路径** | 每个请求/连接/I/O | 启动、配置更新、shutdown、指标聚合 |
| **频率** | 微秒~毫秒级 | 秒~分级，或一次性 |
| **锁** | 零锁（shard-local） | 允许轻量锁（如控制面 Mutex） |
| **跨核通信** | 无（线程独享状态） | 允许 bounded channel 或原子 |
| **分配** | 无堆分配（预分配/池化/arena） | 允许堆分配 |
| **所有权** | 每个 shard 独占 | 可以共享（只读 config, AggregatedMetrics） |

**设计规则**：
- 控制面代码不能出现在数据面的 include 路径或调用栈中
- 控制面更新 config 必须通过版本化 broadcast，不能在热路径上检查"配置是否变了"
- 指标聚合是控制面任务，不能在请求路径上打全局 atomic counter
- 控制面的简单锁(如 `std::sync::Mutex`)是允许的，但必须在 `ARCHITECTURE_EXCEPTIONS.md` 登记

### Shard 边界与状态所有权

```
每个 CpuCore（一个 shard）独享：
├── io_uring 实例
├── Task 队列 (ready_queue)
├── Timer Wheel
├── Buffer Pool
├── 所有活跃连接的状态
├── 所有活跃 Task 的状态
├── 本 shard 的计数器 (metrics)
└── config 快照的本地缓存 (Rc<Config>)

跨 shard 共享：
├── 只读 config (Arc<Config>, 控制面广播更新)
├── 聚合指标 (控制面定期收集)
├── 跨 shard 消息队列 (bounded channel, 仅 spawn/shutdown)
└── SO_REUSEPORT socket (内核负责分发)
```

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
| **Native bind/listen** | **6.11** | 服务器启动路径完全避开 open/bind/listen syscall |
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

## 竞品架构分析

### 为什么当前运行时性能只有 Monoio 的 1/6

现状：macOS 上跟 Tokio 差不多，Linux 上约为 Glommio 的 1/3、Monoio 的 1/6。

macOS 性能正常说明 **epoll/kqueue 回退路径没有问题**，性能差的根因完全在 Linux io_uring 路径。

#### 最可能的四类根因

##### 根因一：每个 I/O 请求堆分配 + 分配器未优化

io_uring 的 SQE 提交本身应该是零拷贝、零分配的。如果每个请求都 `Box::new()`
或走 `Arc`，高并发下分配器压力会直接把性能打穿。Monoio 用 **Slab allocator** 管理
请求对象，compio 用堆分配但配 **mimalloc** 弥补。

当前实现使用 `slab::Slab<PendingOp>` 管理请求对象，CQE `user_data` 直接是
slab index（O(1) 匹配），这一步的设计是正确的。但需要审计 `Buffer` 的分配路径：
如果每次 I/O 的 `Buffer` 都走了堆分配（而非 slab 或池化），在并发数高时 allocator
会成为隐藏瓶颈。

检查要点：
- `Buffer::new_zeroed(len)` 和 `Buffer::new_uninitialized(len)` 是否每次都 `Vec::with_capacity` → 堆分配？
- 即使是走 `BufferPool`，pool 本身是否用 lock-free 的回收策略？

##### 根因二：SQE 提交模式——未能最大化批量优势

当前实现已经在事件循环末尾做批量提交（`flush()`），且用 `pending_sqes` 计数器
跟踪待提交 SQE，`flush()` 在计数器为 0 时跳过 syscall。

已修复的细节：
- ✅ `flush()` 空提交检查：`pending_sqes == 0` 时直接 return，零 syscall
- ✅ `assert_eq!` → `debug_assert_eq!`：release build 下无 overhead
- ✅ `poll_complete()` 零分配：`mem::take` 用 Vec 复用替代 drain + 新分配
- ✅ 预分配 `cqe_buffer`：CQE 收集不再每次新建 Vec
- ✅ `submit()` 中的 `pending_sqes` 追踪：emergency flush 后计数器归零
- ✅ `park()` 中的 `pending_sqes` 追踪：`submit_and_wait` 后归零

剩余问题：
- `park()` 路径的 double `cq.sync()`：`park()` 调用 `submit_and_wait(1)` 后 CQE
  已可见，`poll_complete()` 中的 `cq.sync()` 是多余的（但仅 barrier，非 syscall）。

最关键的度量指标：**用 `perf stat -e syscalls:sys_enter_io_uring_enter` 对比
你的运行时和 Monoio 在相同负载下的 syscall 次数。** 如果多一个数量级，根因就在这里。

##### 根因三：隐藏的锁竞争

当前 Waker 实现使用 `Rc`（非 atomic refcount）+ `*const SegQueue` raw pointer，
这是正确的 lock-free 设计。但以下路径可能引入隐藏的锁竞争：

1. **`CpuIoState::io_backend: Arc<IoBackend>`**：`IoFuture::poll()` 每次通过
   `crate::cpu::io_state()` 拿到 `&'static CpuIoState`，但 `io_backend` 本身是
   `Arc<IoBackend>`。如果 poll/drop 路径频繁 clone/drop 这个 Arc，会引入不必要的
   atomic RMW 操作。

2. **`Vec<Option<...>>` 的 resize 路径**：`CpuIoState::ensure_capacity` 在热点路径上
   调用 `Vec::resize_with`，这会触发堆分配。如果 IoToken 持续增长，每个新 token
   第一次访问都要 resize vec，分配器锁会成为瓶颈。**解决方案**：用 `Slab<T>` 替代 Vec，
   或起始容量大于 ring 大小（4096）。

3. **`SegQueue` 的 cache line 竞争**：`crossbeam::SegQueue` 是 lock-free 的，但底层
   用 `AtomicPtr` 和 CAS。在极高的 wake 频率下，相邻核心的 cache line 回写仍可能
   造成性能下降。

##### 根因四：Buffer 所有权模型导致不必要的 memcpy

当前 `poll_read(user_buf)` 走方案 B：
1. 分配 `internal_buf`
2. 提交 `READ(fd, internal_buf)` —— 内核写 internal_buf
3. CQE → `internal_buf` → `copy_to(user_buf)`（memcpy）
4. recycle `internal_buf`

每个读操作多一次 memcpy。大包场景（HTTP body、文件 I/O）下 memcpy 带宽消耗显著。
小包场景虽然影响较小，但积少成多。

io_uring 是 completion-based 模型——内核在操作完成前一直持有 buffer。
Rust 的借用检查器无法静态保证 safety（内核在写 buffer 时用户程序可能正在读），
所以必须通过**所有权转移**而非借用来传递 buffer。这正是 Rent 模式存在的价值。

**Rent 路径的 bug 让情况更糟**：`WriteRent` 完成时不返回真实 buffer（返回空），
`ReadRent` submit 时创建了 `with_capacity(0)` 的空 buffer——两条 bug 导致 Rent 路径
实际上从未正常工作，所有 I/O 都回退到有 memcpy 的普通 Read/Write 路径。

### compio 的核心设计选择

compio 是当前（2025~2026）最活跃的 thread-per-core + io_uring 运行时。
Apache Iggy（2026年2月公开迁移）完整评估了 Monoio、Glommio、compio 后选择了 compio。

| 维度 | compio | Monoio | 对本运行时的启示 |
|------|--------|--------|----------------|
| 架构 | driver/executor 解耦 | driver 与调度器耦合 | 可借鉴解耦设计，但不必过度工程 |
| Buffer 模型 | `IoBuf`/`IoBufMut` trait | owned buffer trait | 当前 Rent 路径思路正确，需修复 bug |
| I/O 请求分配 | 堆分配（mimalloc 弥补） | Slab allocator | 当前 Slab 设计是正确的，保持 |
| io_uring 特性覆盖 | 广（multi-shot、buf ring、注册文件...） | 基础覆盖，更新滞后 | 本运行时的 6.12 基线给了更多空间 |
| 跨平台 | Linux + Windows (IOCP) | Linux only | 不需要跨平台，专注 Linux 即可 |

#### Driver/Executor 解耦

compio 把 `compio-driver`（管理 ring、提交 SQE、收割 CQE）和 `compio-executor`
（调度 Future、管理 Waker）做成了完全独立的 crate。这使得：
- driver 可以为每个平台做最优实现
- executor 层完全不感知底层差异
- 用户可以只用 driver 而不跑 executor

当前实现中 `IoProvider` trait 的接口边界本质上也做到了类似的分离（
`submit`/`poll_complete`/`flush`/`park`），区别在于 executor 驱动方式不同。

#### IoBuf / IoBufMut 所有权模型

compio 的 trait 清晰地表达了 completion-based I/O 的所有权规则：

```rust
// 读操作：用户提供 buffer → 内核写入 → 返还用户（零拷贝）
trait IoBuf {
    fn stable_ptr(&self) -> *const u8;
    fn len(&self) -> usize;
}

trait IoBufMut: IoBuf {
    fn stable_mut_ptr(&mut self) -> *mut u8;
    fn capacity(&self) -> usize;
    unsafe fn set_len(&mut self, len: usize);
}
```

这与当前 `ReadRent`/`WriteRent` 设计思路完全一致。Rent 路径需要修复而非重新设计。

#### compio 的 join! ≠ IOSQE_IO_LINK 陷阱

这是 Apache Iggy 团队踩过的最严重的坑：

```rust
let write1 = file.write_all(content1);
let write2 = file.write_all(content2);
join!(write1, write2).await;
```

你以为这是"批量提交两个有序写操作"，但实际上 io_uring **不保证 SQE 的完成顺序**。
两个操作的完成顺序可能是任意的——对于有序依赖的操作（如 WAL 写入），这会静默损坏数据。

正确做法是显式使用 **`IOSQE_IO_LINK`** 标志来链接有顺序依赖的 SQE。
compio 和 Monoio 目前暴露这个能力但需要手动处理。当前运行时尚未暴露此功能，
是后续需要实现的关键能力。

### ntex/neon-uring 的设计选择

neon 是 ntex 框架的内部运行时，它基于 compio 的架构思路但更小、
更专注于网络服务场景（约 1K SLoC 运行时核心）。

**neon-uring 的核心优势：对 io_uring 网络特性的激进使用。**

普通的 io_uring 实现即使用了 io_uring，也往往是"一请求一 SQE，一 CQE 触发一次 re-arm"
的模式，这在 syscall 数量上与 epoll 基本持平，只是异步化了操作本身。

而 neon-uring 更多地使用了 **multi-shot 操作模式**：
- 监听新连接用 **multi-shot accept**：一个 SQE 持续产生新连接的 CQE，直到出错
- 数据接收用 **multi-shot recv** 配合 **provided buffer ring**

这使得在**低负载下也能明显节省 syscall**，而不像其他 io_uring 方案必须在高并发才
能体现优势。这正是社区 benchmark 里 neon-uring 在单线程低负载下也超过 Monoio 的原因。

**neon 的使用限制**：它是 ntex 框架的内部运行时，不能像 `#[compio::main]` 一样
独立驱动任意 async 代码。整个网络服务层需要基于 ntex 的 service/pipeline 体系。
这在构建 HTTP/TCP 服务时没有任何问题，但不适合需要通用 executor 的场景。

---

## 性能诊断方法论

### 分层 Benchmark

运行时 benchmark 必须分层测量，不能用单一综合 benchmark 代替。

**第一层：运行时基础能力微 benchmark**

```
IORING_OP_NOP 吞吐：每秒处理多少个 no-op 操作
  → 反映 SQE 填充、批量提交、CQE 收割的纯开销
  → 正常值：千万级 ops/s
  → 如果很低，ring 管理本身有问题，不需要往上测

Task spawn + wakeup roundtrip：spawn 一个 task 立刻 wake 另一个
  → 反映 executor + Waker 实现效率
  → Monoio/compio 大约几百纳秒
```

**第二层：I/O 原语 benchmark**

```
网络：
  - echo server（单包往返）→ wrk/wrk2（带 latency histogram）
  - TCP pipeline（不等回包连续发）→ 压 SQ 满载，测批量提交能力

磁盘（fio）：
  - randread (4K O_DIRECT) → IOPS 和延迟
  - seqwrite (O_DIRECT + buffered) → 带宽
  - fsync 下测批量 fsync 效率（链路提交关键）
```

**第三层：端到端应用 benchmark**

TechEmpower 风格：plaintext（纯 HTTP 吞吐）→ JSON（加序列化）→ DB query（加数据库 I/O）。
这三个场景的瓶颈依次从网络栈转移到 CPU 再转移到 I/O，能区分运行时在不同压力下的表现。

### 协议选择

| 协议 | 测试目的 | 暴露的运行时弱点 |
|------|---------|----------------|
| 裸 TCP echo | 最纯粹的运行时测试，无协议开销 | 运行时 I/O 路径纯性能 |
| HTTP/1.1 短连接 | 大量 accept + close | multi-shot accept 效果 |
| HTTP/1.1 长连接 | 稳态吞吐，连接复用 | SQ 批量提交能力 |
| HTTP/2 + 多路复用 | 流管理 + 状态机开销 | 高并发下的内存管理 |
| 自定义二进制协议 | 贴近真实消息/数据库场景 | 综合表现 |

### 线程与运行时间确定

- **线程数**：一个 worker thread 对应一个物理 core，不要超。
  context switch 会吞掉 thread-per-core 的所有收益。
- **Pin 核心**：用 `core_affinity` crate pin 线程。不 pin 会导致 OS 随机迁移，
  CPU cache 失效直接把性能打回 Tokio 级别。
- **运行时长**：网络 benchmark 至少 30s，前 5~10s warm-up 丢弃。
  磁盘 benchmark 至少 60s（page cache 状态前几十秒大幅变化）。
- **Percentile 延迟**：测 P50/P99/P999 而非平均值。P999 高说明偶发的调度停顿
  或锁竞争。wrk2 比 wrk 好在能输出完整 latency histogram。

### 快速诊断步骤

在改代码之前，先量化问题：

```bash
# 1. 看 syscall 次数差异
perf stat -e syscalls:sys_enter_io_uring_enter -p <PID>

# 2. 看热点函数
perf record -p <PID> -g -- sleep 30
perf report

# 3. 隔离测试 io_uring 裸路径
# 只提交 IORING_OP_NOP，看每秒能处理多少个
```

如果 io_uring_enter 调用次数比 Monoio 多一个数量级，就是批量提交问题。
火焰图上如果 `_Atomic***` 或 `malloc`/`free` 在热点前几位，就是锁或分配器问题。

### 关于 fsync 的基准测试

fsync 越严格越慢，但它测的是运行时能否**有效地批量 fsync**。
io_uring 的优势在于可以把多个写操作和一个 fsync 用 `IOSQE_IO_LINK` 串成链，
内核按序执行——"写 → 写 → fsync"链只触发一次磁盘同步。如果运行时没有利用
链式提交，每个 fsync 都是独立 syscall 和磁盘写屏障，性能就和 `fdatasync()` 一样差。

Apache Iggy 的 benchmark 里 fsync 模式显示 thread-per-core + io_uring 相比 Tokio
有 +18% 的吞吐提升，原因正是这里——Tokio 的 blocking thread pool 处理 fsync 时
无法做链式批量。

所以 fsync benchmark 的意义：
- **不开 fsync**：测吞吐上限（纯带宽/IOPS）
- **开 fsync**：测运行时是否正确利用 io_uring 的顺序链（链路提交能力）

如果 fsync 场景下比 Monoio 还慢，说明链式操作没有做好，
或 fsync 提交路径退化成了同步阻塞。
