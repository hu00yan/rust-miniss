# rust-miniss Roadmap

提交前默认门禁是 `just test-fast`。Linux/io_uring 生命周期、长时间压力、
Loom、Miri 和性能矩阵必须使用各自的显式命令，不得偷偷并入默认快速测试。

## P0：生产同步与生命周期验证

- [ ] 继续抽取真实生产同步 seam，让 Loom 覆盖剩余协议；不能用独立的
      bitset reference model 冒充生产并发证据：
  - [x] waker 与 task close/slot retirement 并发：生产 `Executor` 测试持续从
        其他线程 wake，并在唯一 slot 退休/复用后验证旧 waker 不会复活新任务；
        `WakeControl` 的关闭/wake 竞态也由 `just loom` 穷举。
  - [x] cancellation 与 waiter registration 并发：`just loom` 使用生产
        `CancellationState` 的 Loom 实现验证注册/取消竞态。
  - [x] worker sleep transition 与 notification 并发：生产
        `WorkerSleepGate` 使用单原子 CAS 状态，`just loom` 穷举通知先到和
        进入睡眠后通知两种顺序，并接回 Linux eventfd park 路径。
- [x] `linux-safety` 已验证真实 io_uring 下 submitted TCP read drop 后的 late
      data、terminal drain，以及 descriptor lease 生命周期。
- [x] 覆盖 queued read cancel 的 terminal CQE 和 operation 归零（当前 Linux
      内核）。
- [x] 覆盖 SQ-full flush/retry accounting（当前 Linux 内核）。
- [x] 覆盖 multishot accept rearm 的有界 burst（当前 Linux 内核）。
- [x] 增加 64-client multishot accept burst，并验证 pending accept 在 shutdown
      时能完成 bounded drain 和 termination snapshot 清零。
- [x] 增加多轮 multishot accept overflow 回环：暂停消费端制造队列压力，并在
      listener drop 后验证 retained descriptors 归零。
- [x] 在多个连续 overflow burst 下验证 accept descriptor 回收；Linux
      `linux-safety` 运行 3 轮 128-client burst 后再 drop listener，并检查
      termination snapshot 清零；新增单元测试以 `drain_batch_size=1` 验证小批次
      CQ drain 仍能回收所有 terminal operation。
- [x] 通过关闭并恢复真实 ring fd 注入 `io_uring_enter`/`EBADF`，验证 queued
      SQE 保留、errno 不丢失，并可在 fd 恢复后重试提交（Linux）。
- [x] 增加生产 termination snapshot，证明 shutdown 完成时 task、timer、
      message、queued/submitted operation 和 retained descriptor 全部归零；
      已覆盖含 pending timer 的 forced shutdown。
- [x] 增加 4-worker、256 个并发 timer task 的有限负载 shutdown 回归，并在
      macOS/kqueue 与 Linux/io_uring 上验证 termination snapshot 全部归零；
      持续高负载矩阵仍待补齐。
- [x] 定义并测试 generation 耗尽策略：task generation 仍拒绝饱和，runtime
      generation 用尽后永久拒绝分配，绝不回绕复用旧身份。
- [ ] 在真实多核 Linux 持续负载下继续验证跨核 wake、bounded inbox、eventfd/notifier
      和 worker affinity 的组合行为；macOS/Linux 现有 1024-task bounded pressure
      回归覆盖短时组合行为；`just stress` 现提供 30 秒多核 admission pressure
      和 3 轮、每轮 10 秒负载中 shutdown/清理 gate，跨 worker 长时间终止矩阵
      以及 1/2/4 worker × 16/64 task 的 6 组合矩阵已覆盖；不同任务类型和
      retained-fd 长时矩阵已有 1/2/4 worker × 8/32 pending UDP socket 组合，
      immediate/timer 两种 task 形态已纳入矩阵，pending UDP lease 组合重复 2 轮；
      TCP accepted-fd 8/32 连接组合也重复 2 轮，并在仍持有连接时强制 shutdown
      验证回收。
- [x] 已在 Linux 回归中证明：目标 worker 进入 parked 状态后，跨核
      `spawn_on` 仍能通过 eventfd 唤醒并在指定 worker 上执行；真实高负载矩阵
      仍待补齐。
- [x] 增加 256 次固定目标 shard 的跨核 admission stress，验证 bounded inbox、
      eventfd 唤醒、worker affinity 和 termination snapshot。

## P1：协议和功能覆盖

- [x] 增加 TLS client 的真实端到端 handshake、读写和关闭测试；当前为 TLS 1.3
      自签名证书回环测试。
- [x] 增加 TLS 证书错误/域名不匹配和握手失败清理测试；macOS/kqueue 与
      Linux/io_uring 均已通过。
- [ ] 补充 TLS 默认根证书路径和更完整的平台/证书兼容性矩阵；默认 bundled
      root store 已有私有证书拒绝 smoke test，且已覆盖 trusted IP SAN；公有 CA
      与平台差异仍未覆盖。
- [x] 增加 HTTP/2 并发 stream 和跨多个 DATA frame 的大 body 回环测试。
- [x] 增加 HTTP/2 flow-control、错误响应、取消和连接关闭基础矩阵；客户端
      使用 16 KiB stream/64 KiB connection window，并显式释放每个 DATA frame。
- [x] 增加 QUIC datagram、原生 stream reset 和显式 connection close 回环测试；
      基础 H3 请求/响应也已覆盖。
- [x] 增加 QUIC/HTTP-3 migration 测试；客户端 endpoint rebind 后继续完成
      H3 请求/响应和 datagram 交换。
- [x] 增加 QUIC 0-RTT 测试；首个连接交换 session ticket，第二个连接验证
      客户端接受 0-RTT 且服务端收到的 stream 标记为 early data。
- [x] 增加 `MinissUdpSocket`/`MinissUdpPoller` 的 foreign-shard ownership 回归；
      socket 和 poller 都拒绝跨 shard 访问。
- [x] 增加 2-worker QUIC 回环：server/client endpoint 分别在所属 shard 创建并
      驱动，验证 socket ownership 不跨 shard。
- [ ] 在真实多连接 QUIC 负载中继续证明 socket、I/O token 和 handler 不会被错误地
      跨 shard 使用；测试现在支持显式的 `MINISS_QUIC_CONNECTIONS`，Orb Linux 已验证
      2-worker、64/128/256/512 条连接各重复 3 轮通过。连接启动有最小节流，且 connect
      deadline 与压力场景的 I/O deadline 一致，避免测试自身的 UDP 突发和过短 deadline
      制造假失败；Linux safety gate 现在自动重复 512 连接 3 轮。更高连接数的持续压力，
      以及从控制线程创建并轮转同一 endpoint 仍待验证。当前 1024 连接在 Orb ARM64
      上于 5 秒 accept deadline 失败；128/256/512 已通过，不能据此宣称 1024 连接能力。
- [x] 增加 UDP 基础收发、截断、多 sender、压力和 pending receive cancellation
      回环测试。
- [x] 增加 UDP socket/poller ownership、foreign-shard 拒绝和 owner socket
      close race 回归。
- [ ] 在真实多核 UDP/QUIC 负载中继续验证归属和 close race；当前已增加
      2-worker、128 个并发 client socket 的 UDP ownership/close 清理回环，以及
      4-worker、64 个 owner-local receive 与并发 datagram/shutdown close-race 回归；
      Linux safety gate 现在自动重复 64-socket close-race 3 轮；更长时间持续压力与
      更复杂的异常 close 竞态仍待补充。
- [ ] 为 HTTP、TLS、QUIC 的连接生命周期补充异常路径测试，不只验证成功的
      loopback exchange；当前已补 HTTP partial-EOF、TLS handshake peer-EOF，
      HTTP oversized-header/conflicting-Content-Length rejection、TLS handshake/post-handshake peer-EOF、
      TLS invalid server-name、QUIC peer-close/reset/close、以及已有的
      QUIC handshake refusal/cleanup、HTTP/2 cancel、HTTP incomplete-body EOF、未支持的
      `Transfer-Encoding` rejection、非法 HTTP 版本/header-name rejection 和
      HTTP/1.1 pipelined request buffering，仍需更完整的协议错误矩阵。

## P1：可复现性能证据

- [ ] 在 Linux/Orb 上重新生成并保留 `summary.csv`、`metadata.json`、wrk
      原始日志、server 日志、CPU/RSS/task-clock 和 syscall 记录；已生成
      3 轮固定 CPU 的 miniss-only summary/metadata/wrk/server/verdict artifact，
      keep-alive 与 `Connection: close` 的 32-connection 场景已有三轮结果；
      另外已完成一次完整 `ka8/32/200/800` 与 `close8/32/800` 的 runtime shape matrix；
      新增 `BENCH_MATRIX_ROUNDS`/`just bench-matrix`，并完成固定 CPU 的 ka32/close32
      五轮全 runtime 矩阵；ka32 verdict 全部通过，close32 仅因 glommio CV 超门限而
      整体拒绝，原始异常仍保留。RSS/perf counters 与全连接形状的多轮矩阵仍待补齐，见
      `docs/review/performance-baseline-2026-07-14.md`。
- [x] 将 server 与 client 固定到不重叠的 CPU；记录 affinity、CPU 型号、
      kernel、backend、worker 数和 client 线程数。
- [ ] 运行交错的至少五轮矩阵，覆盖 worker `1/2/4/8`、client threads
      `1/2/4`、connections `8/32/200/800`；新增
      `bench_http_topology.sh`/`just bench-topology` 可机械展开这些组合并要求
      非重叠 CPU placement，当前已完成 worker `1/2/4` × threads `1/2/4` 的
      ka8/close8 单轮 smoke（90 个 runtime 样本）；worker=8 需要更大隔离主机，
      另完成 worker `1/2/4` × thread `1` 的 ka200/close32 单轮高负载 smoke
      （30 个 runtime 样本）；worker=8、全拓扑高连接数和五轮拓扑稳定性仍待补齐。
- [ ] 同时保留 HTTP keep-alive（`ka8/32/200/800`）和新连接/关闭
      （`close8/32/800`）结果；每组记录 throughput、p50、p99、p99.9、错误
      数和进程泄漏；已有一轮 shape matrix，仍需多轮稳定性数据。
- [x] 用同一 workload、同一 CPU placement 和同一 metadata 重新比较
      miniss、monoio、compio、tokio、glommio；不把共享 VM contention 样本当成最终
      排名；已完成 ka32/close32 五轮固定 CPU 比较，ka32 稳定性门通过，close32
      因 glommio CV 失败而拒绝整体 verdict，尚不能形成最终排名，见
      `docs/review/performance-baseline-2026-07-14.md`。
- [ ] 建立稳定 baseline，并让 `just bench-verdict`/`just bench-smoke` 使用
      保留下来的原始证据进行回归判断。
- [ ] 对 flush batch、park timeout、task poll budget、COOP_TASKRUN 等调参
      只接受交错多轮结果；同时报告吞吐和尾延迟，不以单轮结果改默认值。

## P2：自动化和文档

- [x] 增加 macOS/Linux CI gate；Linux 硬性要求 6.12+，并覆盖完整 feature
      测试、feature-off/独立 feature matrix 测试、架构审计、`linux-safety`、无 ignored 测试检查和
      指定协议回归和 30 秒 admission + 3 轮 10 秒 loaded sustained pressure、
      1/2/4 worker × 16/64 task 矩阵、pending UDP socket lease 矩阵；另有独立 Loom/Miri
      job 执行生产同步与内存安全 gate。
- [x] 连续运行 feature 编译矩阵；8 个组合均完成，并由独立的全 feature
      普通回归覆盖运行时行为，未出现失败、ignored、超时或残留进程。
- [x] 增加单一的 `just test-full` 正确性套件入口；它串行编排 portable gate、
      differential/Loom/stress，并按主机显式加入 Linux safety/Miri，环境不具备时打印
      `SKIP` 而不是伪装成通过。
- [x] 为当前性能结论保留可复核的原始 artifact，不只保留 Markdown 汇总表；
      完整单轮 shape/runtime matrix 位于 `.scratch/bench-http-matrix-20260715-full/`，
      多轮稳定性结论仍必须引用对应的多轮目录和 verdict 文件。
- [x] 在协议、后端或 ownership 语义变化后同步 `README.md`、`README_CN.md`、
      `docs/DESIGN.md`、`docs/DECISION_LOG.md` 和本文件；当前 `just test-full` 入口、
      HTTP framing 校验和剩余环境型验证边界均已记录。

## Future / Exploration

这些是经过上述 correctness 和 baseline gate 后再评估的 io_uring 探索项，
不是当前已验证的性能承诺，也不是当前对外 API 的功能缺口。实现时必须保持
现有上层语义不变；除非确实需要新增高层语义，否则不要把内核 flag 或配置泄漏到
业务 API。

| 项目 | 内核需求 | 目标 |
|------|---------|------|
| IOSQE_IO_LINK 链式操作 | 6.0+ | 评估 WAL/fsync 场景的提交开销 |
| Provided Buffer Ring | 6.12+ | 降低 buffer 分配与回收开销 |
| SQPOLL 模式 | 5.11+ | 评估减少提交 syscall；需要权限和独立证据 |
| Registered Buffers | 5.7+ | 评估大 I/O 的页固定开销 |
| Multi-shot recv | 6.10+ | 评估减少 recv SQE 数量 |
| DMA-BUF zero-copy recv | 6.16+ | 评估真正的网络接收零拷贝 |
