# Rust-Miniss Design Document

> 本文档是项目设计决策的唯一权威记录，与 `AGENTS.md`（架构合同）、`TODO.md`（任务卡片）构成三角约束体系。

---

## 设计哲学

**thread-per-core 的核心是任务亲和与分片所有权，不是“完全没有锁或原子操作”。**

每个 worker 独占自己的 I/O backend、Task arena、ready queue、Timer Wheel 和活跃连接状态。跨 shard 的任务接收、取消、指标采样、I/O 回收和唤醒通过有界 inbox、ready queue 或明确的通知路径完成；已接收的任务不会被 work-stealing 或透明迁移。

### Thread-per-core 与 Work-stealing 的取舍

**Work-stealing**：空闲 worker 可以从其他 worker 窃取可运行任务，有助于吸收负载不均，但任务跨 worker 调度时需要同步，并可能损失 cache locality。任务在本地队列上的每次 poll 并不必然需要跨线程同步。

**Thread-per-core（本运行时的模式）**：任务被某个 shard 接收后始终由该 shard 执行；控制面可通过 `spawn()`/`spawn_on()` 显式选择目标 shard，worker 内可用 `spawn_local()`。这减少了隐式迁移并让可变状态保持 owner-local，但跨 shard admission/wakeup 仍有有界通信成本，且不提供 work-stealing 的自动负载均衡。

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
| **锁** | 热路径可变状态保持 shard-local；显式跨 shard 通知仍使用队列/原子 | 控制面允许轻量锁（如 Mutex） |
| **跨核通信** | 允许有界消息、ready queue 和明确的唤醒通知；不允许隐式迁移任务 | 控制面允许 bounded channel 或原子 |
| **分配** | 按实际 API 在 shard-local 分配或复用池化资源；“零堆分配”不是当前全局保证 | 允许堆分配 |
| **所有权** | 可变状态由 owner shard 独占 | 可共享只读值和聚合指标；当前没有跨 shard 的 `Arc<Config>` 热更新对象 |

**设计规则**：
- 控制面代码不能出现在数据面的 include 路径或调用栈中
- 当前 config 由常量和启动时读取的环境变量组成，没有 live-reload 对象；若未来增加运行时更新，必须通过版本化 broadcast 发布快照，不能让热路径轮询配置是否变化
- 指标聚合是控制面任务，不能在请求路径上打全局 atomic counter
- 控制面的简单锁(如 `std::sync::Mutex`)是允许的，但必须在 `ARCHITECTURE_EXCEPTIONS.md` 登记

### Shard 边界与状态所有权

```
每个 CpuCore（一个 shard）独享：
├── I/O backend 实例（io_uring 或 mio）
├── Task 队列 (ready_queue)
├── Timer Wheel
├── Buffer Pool
├── 所有活跃连接的状态
├── 所有活跃 Task 的状态
├── 本 shard 的计数器 (metrics)
└── 启动时传入的 worker 参数（例如 task poll budget、park timeout）

跨 shard 共享：
├── 聚合指标视图 (控制面定期采样)
├── 每 shard 的 bounded inbox（任务接收、取消、shutdown、指标请求、I/O token 回收等）
├── 面向 owner shard 的有界 ready queue / 显式唤醒
└── 应用按需创建的 SO_REUSEPORT listener（内核在 listener 间分发连接）
```

---

## 目标平台

| 层面 | 决策 | 理由 |
|------|------|------|
| **目标平台** | Linux + macOS | Linux 是 io_uring 性能路径；macOS 是 kqueue 正确性/开发路径 |
| **内核基线** | Linux 6.12 LTS | 6.12 是**性能基线**：multi-shot accept/recv、provided buffer ring inc、IORING_OP_SENDZC、min timeout 启用后才有收益，不是能否构建的前提 |
| **epoll/kqueue** | 保留并验证正确性 | Linux 无法使用 io_uring 时回退 epoll；macOS 使用 kqueue |
| **硬件** | x86_64 / ARM64 | 线程 pin 到物理核 |

构建时按内核版本选择后端：≥ 6.12 选 `io_uring`，< 6.12（或版本探测失败）回退
`epoll` 后端，**不再编译报错**。需要显式指定时用
`MINISS_IO_BACKEND={io_uring|epoll|kqueue}` 覆盖探测（值必须对目标 OS 合法）；
这也是在现代内核上构建并验证 epoll 后端的入口。`build.rs` 的 `uname` 探测读取构建机内核，
默认假设构建机与运行机相同；交叉编译时应显式指定目标可用的 `MINISS_IO_BACKEND`。

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

以下是 owner shard 的概念结构（不是实际 Rust struct 定义）：

```
CpuCore {
    task_queue: TaskArena<Task>             // 固定 65_536 槽的 shard-local 任务表
    ready_queue: ReadyQueue                 // 有界队列，元素是完整 TaskId
    owner_ready_queue: Vec<OwnerCompletion> // owner-side I/O completion
    io_backend: Box<dyn IoProvider>         // 本 shard 的 io_uring 或 mio backend
}
```

`task_queue` 当前使用 `TaskArena`，不是早期草案中的 `FxHashMap<TaskId, Task>`。
`TaskKey::slot` 索引固定槽；完整 `TaskId` 还包含 runtime、shard 与 generation，不能直接当
槽索引。固定槽表避免热路径上的哈希查找和动态表扩容；它不代表 task/future admission
没有任何分配。槽位 generation 用于拒绝 stale wake；generation 用尽后槽位退役而不回绕。

### io_uring 事件循环（backend-specific；mio 使用 readiness poll/drain）

```
1. process_messages()    — drain 本 shard 的 bounded inbox（task、ping、shutdown、cancel、metrics、I/O retirement）
2. poll_io()             — 收割所有可用 CQE
    2a. 先 drain completed_ops （错误/不支持操作的快路径）
    2b. 遍历 CQ，对每个 CQE：
        - multi-shot accept → 压入 accept_results，wake accept task
        - 普通 CQE → 如果 pending_task 存在 → inline poll
                      否则 → completion 写入 `CpuIoState::io_slots` + waker.wake()
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

跨 shard 路径不是只有任务派发和 shutdown。当前实现用有界 `CoreMessage` inbox 处理 task admission、ping、shutdown、task cancellation、metrics snapshot 和 owner-side I/O token retirement；远端 waker 通过 shard 的有界 ready queue 和 worker notification 唤醒 owner。所有已接收任务仍留在其 owner shard：没有 work-stealing，也没有透明的任务迁移。

新连接分发不是运行时自动完成的。应用可在每个 worker 上创建 `SO_REUSEPORT` listener，让内核在这些 listener 间分发连接；`examples/http_echo.rs` 展示了该用法。

### 任务亲和性与背压

运行时提供三类 spawn API，边界必须保持清晰：

| API | 用途 | 失败/背压语义 |
|-----|------|---------------|
| `spawn()` | 控制面或无亲和性要求的任务，按已发布的 ready/inbox 深度选择 shard，以 round-robin 打破平局 | bounded inbox 满时立即返回错误 |
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

`spawn_local()` 的 admission 计数只保护本地队列入队；任务本身不会持有 shutdown admission
票据，否则一个长期运行的 `!Send` 子任务会阻塞 shutdown 的 admission cutoff。

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

这保证 shutdown 不会因为普通 task 消息堆积而完全饿死，也不会粗暴丢掉已入队任务。
`shutdown_with_timeout()` 超时后由 owner worker 执行 force-shutdown：先取消并有限 drain
后端 operation，再丢弃 shard-local task、timer 和 ready state；控制线程不直接触碰这些对象。

---

## I/O 模型

### io_uring 配置

| 参数 | 值 |
|------|-----|
| ring 大小 | 4096 entries |
| CQE 数量 | 4096 |
| 提交模式 | 普通（非 SQPOLL，减少内核线程竞争） |
| 实际使用的特性 | `IORING_FEAT_FAST_POLL`（READ/WRITE/ACCEPT 无需 `IOSQE_ASYNC`） |

下表列出内核特性与运行时实际启用状态。**"内核 ≥ 6.12 基线"不代表这些特性都在用**
——`build.rs` 的 6.12 检查只保证 opcode 存在，具体启用与否取决于运行时探测。

| 特性 | 内核 | 运行时状态 |
|------|------|-----------|
| multishot accept | ≥ 5.19 | ✅ 已启用（`net.rs:73` 探测，`Op::PollAdd { multishot: true }`） |
| `IORING_FEAT_FAST_POLL` | ≥ 5.7 | ✅ 用于网络读写 |
| multishot recv | ≥ 6.0 | ❌ 仅普通 accept 后的单次 recv |
| 固定文件（`IORING_REGISTER_FILES`） | ≥ 5.12 | ❌ 未注册，每次 op 传 fd |
| 注册 buffer / `buf_ring` | ≥ 5.19 | ❌ 未使用，每次 op 传指针 |
| sendzc / `IOSQE_FIXED_FILE` | ≥ 6.0 | ❌ 未使用 |

这张表的意义在 `docs/review/performance-baseline-2026-07-11.md`：42.4 req/enter
与 monoio 持平，而 monoio 用了 fixed file + registered buffer。差距不在实现质量，
在**未启用的机制**。上面每一项都是独立可做的工作，不要当成一个整体的"io_uring 优化"。

### io_uring 性能敏感开关

这些开关不是“越多越好”，而是要按内核能力和负载形态逐个验证。

| 开关 | 当前策略 | 观测结论 |
|------|----------|----------|
| `MINISS_URING_SINGLE_ISSUER` | 仅在实验时开启 | 多轮 800 keep-alive 交错样本中收益不稳定；不适合作为默认值。 |
| `MINISS_URING_SQPOLL` | 仅在实验时开启 | 在 OrbStack VM 的 800 keep-alive 样本里 p99 优于 baseline，但尾延迟仍不及 monoio / compio；对 VM / 内核配置敏感。 |
| `MINISS_URING_DEFER_TASKRUN` | 默认关闭 | 在当前 OrbStack 内核上启用时，runtime 直接报 `No suitable IO backend available`，说明这类 taskrun 语义不是所有环境都可用。 |
| `MINISS_URING_COOP_TASKRUN` | 默认开启（失败时回退） | `MINISS_URING_NO_COOP_TASKRUN=1` 可禁用；当前内核可用，关闭会降低吞吐，不能仅凭较低单轮 p99 默认关闭。 |
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

### I/O operation 与 public API 边界

下表对应 `src/io/mod.rs` 当前的 `Op` enum；各 backend 的支持范围不同，unsupported 操作会返回错误。

| `Op` variant | 作用 |
|---------------|------|
| `Accept` | 接收 TCP 连接；backend 支持时可用 multi-shot accept |
| `Read` / `ReadRent` | 普通读，或转移 owned buffer 的读 |
| `WriteStatic` / `WriteRent` | 写入静态字节，或转移 owned buffer 的写 |
| `Connect` / `Close` | 建连或关闭描述符 |
| `ReadFile` / `WriteFile` | 带显式 offset 的文件读写 |
| `Fsync` / `Fdatasync` | 文件数据与元数据刷盘，或仅刷读回所需数据/元数据 |
| `UdpRecv` / `UdpSend` | UDP datagram 收发 |
| `UdpRecvVectored` / `UdpSendVectored` | UDP vectored 收发 |
| `ReadVectored` | owned buffer 的 vectored read |
| `PollAdd` | readiness backend 的 interest registration，也用于明确的 readiness 等待 |
| `Nop` | io_uring no-op benchmark 操作 |

这不是所有 TCP 方法的列表：`poll_read`、`poll_read_vectored`、`poll_write` 和
`poll_write_vectored` 在 readiness backend 上直接执行借用缓冲区的系统调用，遇到
`WouldBlock` 后通过 `PollAdd` 等待；当前 `Op` enum 没有旧版的 `Write` 或
`WriteVectored` 操作。

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
- `write_rent_at`: 文件写侧的同一形状。`AsyncFile::read_at` 一直返还它填的那个
  buffer，写侧此前只返还字节数，`write_rent_at` 让它对称，`write_at` 则是它的
  借用薄包装。io_uring 上归还的就是同一个分配；mio 后端从线程池写，需要一个自己
  的 owned 载荷，因此那里归还的是内容相同、分配不同的拷贝
- 当前库层接口对 `Buffer` / `Vec<u8>` / `Box<[u8]>` 开放，例子不再需要自己维护专用胶水

写侧因此只有三种形状，且每种都能从签名看出缓冲归谁：
`read_rent`/`write_rent` 是**租**（交出 owned，归还同一个分配）；
`poll_read`/`poll_read_vectored`/`poll_write`/`poll_write_vectored` 是**借**
（直试系统调用，排队 op 永不会比调用方的缓冲活得久）；
`write_all_static` 借的是 `&'static [u8]`，这正是它可以入队的原因。
HTTP 响应的「写头再写体」是响应细节而非流能力，由 `src/http.rs` 的
`send_response` 交给 `write_all_slices`；常见短响应可由一次 `writev(2)` 完成，部分写入时会继续重试。

---

## Waker 设计

**当前实现：Arc-backed owner-aware waker state + shard 的有界 ready queue。**

`WakerState` 用原子状态合并重复唤醒并拒绝已关闭 task 的迟到 wake；ready queue
容量有界，队列满时 owner shard 通过 sticky overflow 标记重试，不改为无界排队。
跨线程 wake 只把完整 `TaskId` 放入其 owner shard 的队列并通知 owner；`TaskArena`
校验 runtime、shard、slot 和 generation，避免陈旧 ID 命中新 task。不要按早期
thread-local queue、裸指针或 `Rc` 草案推导当前生命周期。

**I/O future 生命周期：**

- `IoFuture::poll()` 保存 token、waker 和 pending owner task。
- 终端 completion 进入 owner completion 或 completed slot 后唤醒 owner task。
- `IoFuture::drop()` 只撤销投递和清除交付状态；backend 仍保留资源直到终端
  completion，再依据 discarded 标记回收，不能提前释放 kernel 参数或 descriptor。

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
        Note over poll_io: validate token generation and ownership
        poll_io->>poll_io: route owner completion or completed slot
        alt 有 pending_task
            poll_io->>Task: owner ready queue receives task
            Task->>SQ: owner task may submit a new SQE
            poll_io->>poll_io: flush() at event-loop boundary
        else 没有 pending_task
            poll_io->>poll_io: store completion in CpuIoState::io_slots
            poll_io->>poll_io: take stored waker and wake owner task
            Note over poll_io: stale/discarded tokens are ignored safely
            execute_tasks 后续会 poll 这个 task
        end
    end
```

### O(1) 的三层映射

| 步骤 | 数据结构 | 复杂度 |
|------|---------|--------|
| CQE 匹配 PendingOp | Slab index → `try_remove()` | O(1) |
| PendingOp → Task | owner task field in the I/O slot | O(1) |
| Task → Waker | waker field in the I/O slot | O(1) |
| Waker → ready_queue | bounded ready queue push | O(1) amortized |

Owner completion keeps the task identity and result together until the owner
event loop polls it; no cross-thread task migration is implied by a wake.

---

## Task 系统

### Task 结构体与身份

`Task` 在 owner shard 的 arena 中保存一个可本地 poll 的 Future；本地 `spawn_local()`
可以存放 `!Send` Future，跨 shard 的 `spawn()`/`spawn_on()` 则要求 Future 在传递时为
`Send`。Task 缓存 owner-aware `Waker`，其状态使用有界 ready queue 和显式的跨线程通知。

`JoinHandle<T>` 与 task 共用 shard-local 的 `Rc<JoinState<T>>`、`Rc<Cell<bool>>` 和任务
waker，因此它是 `!Send`/`!Sync`；不得用早期草案中的 `Arc<Mutex<JoinState<T>>>` 描述它。

`TaskKey` 由四个 `u16` 字段组成，`TaskId` 将其打包为 64 位值：

```
bits 63..48 = runtime_generation
bits 47..32 = shard
bits 31..16 = slot
bits 15..0  = slot generation
```

每个 shard 的 `TaskArena` 最多有 65,536 个 slot。slot generation 到达 `u16::MAX` 后永久
退役而不回绕，以避免迟到的 wake/cancel 重新命中新 occupant；runtime generation 用尽时也
fail closed。

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

`TlsStream` 当前通过 `tokio-rustls` + `tokio_util::compat` 复用现成的
Rustls/Tokio trait 生态，并在 miniss 的 `AsyncTcpStream` 之上完成客户端
握手。它不是自研 TLS 协议栈，也不创建 Tokio runtime；当前仓库有编译、
线程归属约束和 TLS 1.3 回环握手/读写/关闭、证书错误与主机名不匹配回归，
但默认根证书路径、平台矩阵和完整 TLS 兼容性仍未验证。

---

## 代码结构

当前 crate 的主要边界如下（以 `src/` 为准；`tls` 受 feature 控制）：

- `multicore.rs` 驱动 worker/shard 生命周期；`cpu.rs` 提供 owner-thread 上下文。
- `task.rs`、`waker.rs` 和 `timer/` 实现 shard-local task、owner-aware wake 与计时器。
- `io/mod.rs` 定义 `IoProvider`、`Op` 和 completion contract；`io/uring.rs` 是 Linux io_uring backend，`io/mio_poll.rs` 是 Linux epoll/macOS kqueue 共用的 mio backend。
- `net.rs` 与 `fs.rs` 提供网络和文件 API；mio 文件操作由 backend 的有界线程池执行，不在 runtime worker 上阻塞。
- `http.rs` 是轻量 HTTP/1.1 实现：请求必须有完整的 `\r\n\r\n` header delimiter，且 header 行只接受 CRLF；按文本解析并验证 method/header token 与控制字符；body 保留任意字节、上限为 8 MiB，且必须与 `Content-Length` 完全一致（未声明时为空）。响应按实际 body 长度生成 `Content-Length`；由于不做 chunk encoding，`Transfer-Encoding` 响应头不予序列化，并过滤不安全的状态行/响应头。`hyper_rt.rs`、`compat.rs`、`quic.rs`、可选 `tls.rs` 提供生态适配。
- `buffer.rs`、`config.rs`、`metrics.rs`、`signal.rs` 和 `cancellation.rs` 分别承载缓冲区、配置、指标、信号与取消支持；`error` 类型定义在 `lib.rs` 中。

这份列表描述稳定的职责边界，不把已删除的 `arena.rs` 或已过期的模块依赖图当作当前结构。

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
| io_uring 后端能力 | 广（multi-shot、buf ring、注册文件...） | 基础覆盖，更新滞后 | 可逐步在后端内部采用，不构成对外 API 要求 |
| 跨平台 | Linux + macOS | Linux io_uring 与 macOS kqueue 均有协议集成测试；Windows/IOCP 不在支持范围 |

#### Driver/Executor 解耦

compio 把 `compio-driver`（管理 ring、提交 SQE、收割 CQE）和 `compio-executor`
（调度 Future、管理 Waker）做成了完全独立的 crate。这使得：
- driver 可以为每个平台做最优实现
- executor 层完全不感知底层差异
- 用户可以只用 driver 而不跑 executor

当前实现中 `IoProvider` trait 的接口边界本质上也做到了类似的分离（
`submit`/`poll_complete`/`flush`/`park`），区别在于 executor 驱动方式不同。

上表比较的是后端实现和分层方式，不是要求 `rust-miniss` 把内核机制暴露给业务代码。

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

正确做法是让上层表达“先写入、再刷盘”这样的依赖语义；是否使用
**`IOSQE_IO_LINK`** 应由 runtime 在内部决定。`join!` 本身只表达并发等待，不能
凭空产生 I/O 顺序保证。即使其他运行时提供底层 driver 选项，也不是本运行时必须
复制的 API。

本项目遵循“上层表达语义、后端选择机制”的边界：provided buffer、multishot
receive、registered buffer、SQPOLL 等同样属于内部优化手段。只有当某个公开 API
承诺的语义无法实现，或者需要新增明确的高层语义（例如有序写后刷盘）时，才应新增
高层 API；不直接暴露 io_uring flag。

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
IORING_OP_NOP 吞吐：记录每秒完成的 no-op 操作
  → 反映 SQE 填充、批量提交、CQE 收割的综合成本
  → 与同一主机、内核、CPU 拓扑和 toolchain 下的基线交错比较；不设通用“正常值”

Task spawn + wakeup roundtrip：spawn 一个 task 后唤醒另一个
  → 记录多轮分布和尾延迟，隔离 executor/Waker 成本
  → 不把其他 runtime 或其他机器的单个数字当作本项目性能门槛
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
- **Pin 核心**：worker 启动时按平台调用线程 affinity（Linux 当前使用
  `sched_setaffinity`）把线程绑定到计划的 CPU。不 pin 不会破坏 Rust 的
  所有权模型，但会削弱 cache locality，并使性能数据混入 OS 调度迁移噪声。
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

正确性验证统一入口为 `just test-full`：它串行执行 portable gate、feature
编译矩阵、一次全 feature 普通回归、差分模型、Loom 和 bounded stress；Linux/Miri gate 按主机能力加入，
不可用时必须显式输出 `SKIP`。

### 如何基准测试 fsync 与 fdatasync

分别记录 backend、文件系统、文件大小、主机和 workload；不能把一种 backend 的 group commit
机制当成另一种 backend 的行为，也不能从 `join!` 推导 I/O 顺序或持久性。

当前实现的行为边界是：

- io_uring 为每个 `Op::Fsync` / `Op::Fdatasync` 提交独立 operation；当前代码没有
  `IOSQE_IO_LINK` 写入—刷盘链，也没有批量刷盘保证。
- mio backend 在同一 shard 的 event-loop boundary 按 `(st_dev, st_ino)` 合并等待中的
  flush 请求，并由 bounded file thread pool 执行；这是 readiness backend 的 group-commit
  行为，不是 io_uring 的行为。

benchmark 应分别测量普通写、`fsync` 和 `fdatasync`，并为每种 backend 记录实际 syscall/完成
数量与吞吐/延迟。若未来引入 io_uring linked operations，应先增加顺序与取消生命周期测试，再
用同机基线验证；在此之前不把 `IOSQE_IO_LINK` 或其他 runtime 的性能数字写成当前能力。
