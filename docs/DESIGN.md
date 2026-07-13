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
| **目标平台** | Linux + macOS | Linux 是 io_uring 性能路径；macOS 是 kqueue 正确性/开发路径 |
| **内核基线** | Linux 6.12 LTS | 需 multi-shot accept/recv、provided buffer ring inc、IORING_OP_SENDZC、min timeout |
| **epoll/kqueue** | 保留并验证正确性 | Linux 无法使用 io_uring 时回退 epoll；macOS 使用 kqueue |
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
4. flush()               — 一次 io_uring_enter 提交批量 SQE
5. process_timers()      — 过期 timer → waker.wake()
6. park_if_idle()        — submit_and_wait(1) + min timeout
```

### SQE 提交策略

**方案 B：事件循环最外层统一提交。**

```
Task::poll() 可能产生 0~N 个 SQE，这些 SQE 塞进 SQ 但不立即提交。
事件循环末尾调用一次 flush() → 一次 io_uring_enter 提交累积 SQE。

例外：inline completion 路径中，如果 inline poll 产生了新 SQE，
      会调用 flush()，但仍遵守正常 batch 阈值。
      实验过 force-submit continuation：低连接场景会明显增加 syscall 压力，
      对 ka8/close8 不稳定且可退化，因此拒绝作为默认策略。

默认策略优先控制 `io_uring_enter` 频率：`FLUSH_BATCH_SIZE = 61`。这个值故意避开 32/64 这类二次幂，
减少 batch 和 page/cache 边界耦合导致的抖动。低连接场景（连接数约等于核心数）
可能因为每 shard batch 太小而延迟提交，但不能把默认 batch 降到 1；那会把 syscall 数重新打爆。
`MINISS_URING_FLUSH_BATCH_SIZE` 只用于实验不同负载下的 batch/latency tradeoff。

短连接路径使用 `AcceptMulti` 的 `SOCK_NONBLOCK | SOCK_CLOEXEC` flags，让内核直接返回非阻塞 fd。
net 层只在 backend 明确声明 accepted stream 已非阻塞时跳过重复 `set_nonblocking()`，避免每个 accepted
connection 多一次 fcntl syscall。
TCP send/recv buffer 大小在 listener 创建时设置一次，不在 accepted stream 热路径重复 `setsockopt()`。

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
│ flush() 一次提交 SQ batch（常规） │
└─────────────────────────────────┘
```

### 跨核协作

只做两件事：
- `spawn_on_core(core_id, task)` — 跨核扔一个 task 过来
- `shutdown()` — 所有核广播 shutdown 信号

不做 work-stealing，不做任务迁移。

新连接的分发：用 `SO_REUSEPORT` 让内核负载均衡。不在运行时层做。

### 任务亲和性与背压

运行时提供三类 spawn API，边界必须保持清晰：

| API | 用途 | 失败/背压语义 |
|-----|------|---------------|
| `spawn()` | 控制面或无亲和性要求的任务，round-robin 选择 shard | bounded inbox 满时立即返回错误 |
| `spawn_on(core)` | 显式指定 shard，适合 dispatcher 或测试 | bounded inbox 满时立即返回错误 |
| `spawn_local()` | worker 内创建同连接/同 shard 子任务 | 直接进入 thread-local 本地 spawn 队列；必须在 worker 线程调用，否则返回错误 |
| `spawn_wait()` / `spawn_on_wait()` | 控制面需要等待 bounded inbox 容量时使用 | timer backoff 重试，直到入队或 timeout |

HTTP keep-alive 连接处理使用 `spawn_local()`：accept task 和连接 handler 保持同 shard，避免
连接状态、I/O token、timer 在不同核心之间跳动。

`spawn_local()` 不能通过 cross-core channel 给本核心发消息。worker poll 过程中产生的本地子任务先进入
thread-local spawn 队列，`CpuCore::execute_tasks()` 在同一轮 drain 到本 shard 的 `task_queue` 和
`ready_queue`。这保留 task affinity，并避免短连接场景每个 accept 都触碰跨核消息队列。

`spawn_wait()` 不引入 unbounded queue，也不阻塞 worker 线程；它只是在 bounded inbox 满时按
1ms timer backoff 重试。它不是负载均衡机制，也不会把任务迁移到其他核心。

### Shutdown Drain

`shutdown()` 分两层信号：

1. control plane 把 runtime state 切到 `ShuttingDown`，阻止后续 `spawn*` 接收新任务；
2. 每个 worker 进入 draining 模式，继续处理已经入队、已经 ready、或正在等待 I/O/timer 的任务。

worker 的退出条件是：

```
draining
&& local_task_count == 0
&& ready_queue.is_empty()
&& message_inbox.is_empty()
```

这保证 shutdown 不会因为普通 task 消息堆积而完全饿死，也不会粗暴丢掉已入队任务。它仍然不是
强制终止机制：如果业务 task 永不完成，worker 会继续等待。未来如果需要生产级优雅关闭，应增加
显式超时和 force-shutdown 阶段，但不能把强制取消塞进数据面热路径。

---

## I/O 模型

### io_uring 配置

| 参数 | 值 |
|------|-----|
| ring 大小 | 4096 entries |
| CQE 数量 | 4096 |
| 提交模式 | 普通（非 SQPOLL，减少内核线程竞争） |
| 特性 | `IORING_FEAT_FAST_POLL`、`IORING_FEAT_EXT_ARG` |

### io_uring 性能敏感开关

这些开关不是“越多越好”，而是要按内核能力和负载形态逐个验证。

| 开关 | 当前策略 | 观测结论 |
|------|----------|----------|
| `MINISS_URING_SINGLE_ISSUER` | 仅在实验时开启 | 800 keep-alive 的 10s 样本里 p99 有改善，但仍明显慢于 monoio / compio；不适合作为默认值。 |
| `MINISS_URING_SQPOLL` | 仅在实验时开启 | 在 OrbStack VM 的 800 keep-alive 样本里 p99 优于 baseline，但尾延迟仍不及 monoio / compio；对 VM / 内核配置敏感。 |
| `MINISS_URING_DEFER_TASKRUN` | 默认关闭 | 在当前 OrbStack 内核上启用时，runtime 直接报 `No suitable IO backend available`，说明这类 taskrun 语义不是所有环境都可用。 |
| `MINISS_URING_COOP_TASKRUN` | 默认关闭 | 同上，当前 VM 上不能把它当成稳定默认项。 |
| `MINISS_URING_FLUSH_BATCH_SIZE` | 默认 61 | 已验证过把它调高并不总是更好；对 p99 的影响比吞吐更敏感。 |

实测结论先记在这里：

- `setup_single_issuer()`、`setup_sqpoll()` 属于可以继续 benchmark 的调参项，但默认不应打开。
- `setup_coop_taskrun()` / `setup_defer_taskrun()` 需要先确认目标内核确实支持，再考虑作为默认优化。
- 当前优先级仍然是让 `park()` / `drain()` 的尾延迟稳定，而不是把所有 ring feature 一次性开满。

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

### PendingOp Slab 后续优化

当前实现为了避免 CQE 晚到时 slot 复用导致 ABA/misroute，保留了 `token_id → slab_idx`
查找层。这个设计牺牲了少量 hash lookup，但优先保证完成事件路由正确。

未来可以把 token 设计成 `generation + slab_idx`：

- `user_data` 携带 slab index 和 generation，CQE 回来后先校验 generation
- slab slot 复用时递增 generation，晚到 CQE 会被识别为 stale completion
- `CpuIoState` 的 waker/completion/pending-task 索引也必须使用同一 token 编码，不能只改
  `PendingOp` 查找层
- 必须配套 ABA 回归测试：提交 op A → 释放 slot → op B 复用 slot → A 的迟到 CQE
  不得唤醒/完成 B
- 必须配套 pending-op churn benchmark；只有当 hash lookup/分配在 perf 中成为可见瓶颈时，
  才把这个优化设为默认

这条路线的潜在收益是减少 HashMap 内存访问和哈希成本，尤其适合短连接、大量 pending op、
多 I/O/请求的真实 workload。但在当前 HTTP keep-alive echo 基准里，主要瓶颈仍是
event loop/syscall/TCP batch，不能用这个高风险改动替代更直接的 I/O 路径优化。

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

真正的 zero-copy 只能通过 `read_rent` / `write_rent`（用户传自己的 owned buffer 所有权）实现。
`Rent` 模式是 thread-per-core + completion-based I/O 的最佳匹配：
- `read_rent`: 用户提供 owned buffer → 内核直接写入 → 返还用户（零拷贝）
- `write_rent`: 用户提供 owned buffer → 内核直接读取 → 返还用户（零拷贝）
- 当前库层接口对 `Buffer` / `Vec<u8>` / `Box<[u8]>` 开放，例子不再需要自己维护专用胶水

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
生态目标不只包括原生适配的库，也包括那些“只认 `tokio::io` trait”的现成库；这类库优先走 compat 层，不强迫重写协议栈。

### 已实现

### Hyper

- `src/net.rs` 直接实现了 `hyper::rt::Read` / `hyper::rt::Write`
- `tests/hyper_http1.rs` 验证了 `hyper::server::conn::http1::Builder::serve_connection` 能直接跑在 miniss 上
- 没有引入 tokio runtime，也没有靠 `hyper-util` 做二次包装
- `src/hyper_rt.rs` 提供了 `hyper::rt::Executor` / `hyper::rt::Timer`

### tokio trait 生态

- `src/compat.rs` 提供 `tokio::io` trait 兼容层，保留给只认 Tokio IO trait 的第三方库
- `tests/tokio_compat_tests.rs` 和 `tests/tokio_io_compat.rs` 继续验证 tokio trait 兼容
- `TlsStream` 仍然通过 compat 复用 `tokio-rustls`，这是最短路径的 TLS 入口
- `quinn + h3-quinn` 已有原生 runtime 适配和端到端回归测试

### 进行中

- hyper 的少量连接生命周期收尾细节还可以继续收紧

### 未做

- 删除 compat 层，强迫所有库改成 miniss 专用 trait
- 自己重写 TLS / HTTP / QUIC 的完整协议栈
- 把还在用 tokio trait 的第三方库当成“不支持的生态”

### TLS 集成

```
TlsStream {
    inner: rustls (直接调用，不依赖 tokio-rustls)
}
```

**当前实现** 还是通过 `tokio-rustls` + compat 复用现成生态。等后续要做纯 rustls 版本时，再把这段迁移成原生实现。

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
### compio 的核心设计选择

compio 是当前（2025~2026）最活跃的 thread-per-core + io_uring 运行时。
Apache Iggy（2026年2月公开迁移）完整评估了 Monoio、Glommio、compio 后选择了 compio。

| 维度 | compio | Monoio | 对本运行时的启示 |
|------|--------|--------|----------------|
| 架构 | driver/executor 解耦 | driver 与调度器耦合 | 可借鉴解耦设计，但不必过度工程 |
| Buffer 模型 | `IoBuf`/`IoBufMut` trait | owned buffer trait | 当前 Rent 路径思路正确，需修复 bug |
| I/O 请求分配 | 堆分配（mimalloc 弥补） | Slab allocator | 当前 Slab 设计是正确的，保持 |
| io_uring 特性覆盖 | 广（multi-shot、buf ring、注册文件...） | 基础覆盖，更新滞后 | 本运行时的 6.12 基线给了更多空间 |
| 跨平台 | Linux + macOS | Linux io_uring 与 macOS kqueue 均有协议集成测试；Windows/IOCP 不在支持范围 |

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

运行时 benchmark 必须分层测量，但**默认以微基准为主**。
微基准最容易暴露基础问题：调度、唤醒、提交/收割、buffer 管理、timer、poll 路径。
端到端 HTTP/QUIC benchmark 仍然需要保留，但它们是验证层，不是主要诊断层。

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

端到端场景的作用是回答两个问题：

- 微基准暴露的问题，是否真的会传导到真实协议栈
- 在接近业务的负载下，runtime 还能不能维持可接受的吞吐和 p99

所以不能反过来用一个大而全的 HTTP/QUIC 场景替代微基准。
那样会把“问题出在 timer、executor、I/O backend 还是协议状态机”混在一起。

### 协议选择

| 协议 | 测试目的 | 暴露的运行时弱点 |
|------|---------|----------------|
| 裸 TCP echo | 最纯粹的运行时测试，无协议开销 | 运行时 I/O 路径纯性能 |
| HTTP/1.1 短连接 | 大量 accept + close | multi-shot accept 效果 |
| HTTP/1.1 长连接 | 稳态吞吐，连接复用 | SQ 批量提交能力 |
| HTTP/2 + 多路复用 | 流管理 + 状态机开销 | 高并发下的内存管理 |
| 自定义二进制协议 | 贴近真实消息/数据库场景 | 综合表现 |

连接数只是压力手段，不是目标本身。`ka8`、`ka200`、`ka800` 的意义是把
相同协议路径拉到不同并发区间，观察 p99 和吞吐是否出现拐点。
如果微基准已经显示某个提交把 submit/drain、wake、timer 或 waker 弄慢了，
HTTP 场景只是进一步确认，不是第一证据。

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

基线优先用 `bench/scripts/bench_http_matrix.sh <base_port>` 跑同一组场景，
保存 `summary.csv`、原始 perf CSV 和 server/wrk 日志，避免只看单次吞吐。

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
