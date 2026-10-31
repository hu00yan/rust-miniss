# QUIC 资源管理审计(2026-10-31)

> **结论:部分确认。**"只实现了网络"字面成立:`src/quic.rs`(796 行)全部
> 是 quinn 的**驱动适配层**(Runtime trait / UDP / poller / timer),crate 内
> 不存在任何连接管理层——h3 甚至只是 dev-dependency,crate 不含一个生产级
> QUIC 消费者。但"残缺"要修正定性:**驱动层自身的生命周期正确性有测试与
> 台账背书,缺的是一整层从未被写过的东西**——在 quinn 的模型里这层本来就
> 归应用,miniss 既没提供 TCP 那样的便利层,也没有一句话文档把职责说清楚。
>
> 方法:读码审计(本仓库 + quinn 0.11.11 / quinn-proto 0.11.15 源码),
> 未重跑测试。tests/quinn_h3.rs 在 CI macOS + Linux(6.12+)双平台常跑。
> 版本:quinn 0.11.11 / quinn-proto 0.11.15 / quinn-udp 0.5.14 /
> h3 0.0.8(dev)/ h3-quinn 0.0.10(dev)。

## 0. 先回答业主的问题:QUIC 的生命周期为什么是应用层的事

**TCP**:连接状态——重传、乱序排序、流控、keepalive、accept backlog、
收发缓冲上限——全部在**内核**。应用只读写字节,内存有 rcvbuf/sndbuf 兜底,
活性有内核 keepalive 兜底,连接数有 backlog/文件描述符约束。

**QUIC**:以上全部搬进**用户态**。每个连接一个 quinn-proto 状态机(乱序
重组缓冲、发送队列、丢包恢复 timer、流控窗口),外加每连接一个驱动任务
(quinn `connection.rs:66` 的 `runtime.spawn`)、每 endpoint 一个
(`endpoint.rs:153`)——在 miniss 上就是 shard 上的任务、时间轮上的 timer、
进程内存里的缓冲。

所以 TCP 下"应用不用管"的事——**连接数上限、每连接内存、空闲回收、
关停排水、握手防洪**——在 QUIC 下都变成运行时/应用必须显式管理的对象。
这就是"应用层管理生命周期"的出处。miniss 现状:该职责被完整留给用户,
没有设施,也没有文档。

## 1. 已存在且验证过的部分(驱动层,不属于"残缺")

| 事项 | 证据 |
|---|---|
| quinn Runtime trait 适配:timer→时间轮、spawn→shard(核上 `spawn_local`/核外 `spawn_detached`)、UDP 包装 | `src/quic.rs:44-72` |
| 跨 shard 退役:owner-shard 捕获 + 有界退役通道,**提交前预留**(inbox 满也能退役,不阻塞不竞争) | `quic.rs:141-177, 250-267`;DECISION_LOG 2026-10-18 行;`cross_shard_socket_drop_retires_under_full_inbox_pressure` |
| 外来 shard 拒绝(owner 绑定,迁移不静默) | `quic.rs:228-247`;`udp_socket_and_poller_reject_foreign_shards` |
| macOS kqueue UDP 写就绪不可靠的绕行(0 超时 poll 采样 + 1ms 重试) | `quic.rs:471-510`,代码内注释给明依据 |
| GSO/GRO 透传 quinn-udp | `quic.rs:355-365` |
| 每 socket SO_SNDBUF/SO_RCVBUF 4 MiB | `quic.rs:125, 182-197` |
| 生命周期语义测试:h3+datagram+0-RTT+endpoint rebind(连接迁移)、0-RTT ticket 重用、handshake refuse 有界清理、peer close 观察、2 shard × 32~N 连接压力 + `close`+`wait_idle` 排水 | `tests/quinn_h3.rs`(5 测试);CI 双平台 `cargo test --tests` |
| 已知测试体竞态(#41)及 19h epoll 自旋事故(#42,#46 的 epoll 祖先,已修) | ledger #41/#42 |

## 2. 缺失部分(按责任分类)

### (a) miniss 该建没建——"上层资源管理"的真实清单

| # | 缺口 | 现状 | 证据 |
|---|---|---|---|
| A1 | **连接管理层**:计数、上限、拒绝策略 | 无设施。测试手搓 accept 循环;连接数只是测试 env(`MINISS_QUIC_CONNECTIONS`);quinn 本身无内置 max-connections,`endpoint.accept()` 裸露给应用 | `tests/quinn_h3.rs:25-31, 458` |
| A2 | **metrics 桥**:`Connection::stats()` / `Endpoint::stats()` 无人采集进 per-shard 指标 | 无 | quinn `connection.rs:537`、`endpoint.rs:94`;`src/metrics.rs` 无 quic 系列 |
| A3 | **shutdown 桥**:运行时关停不感知 endpoint,无 drain 编排;quic.rs 的 spawn 注释承认竞态(驱动任务准入失败→仅日志→endpoint 静默死) | 无;测试全是显式 `close`+`wait_idle` 手动排水 | `quic.rs:49-63` 注释 |
| A4 | **示例与文档**:examples/ 零 QUIC;DESIGN.md 只有一句"quic.rs 提供生态适配" | examples/ 共 8 个,全部 TCP/UDP/HTTP | `examples/`;`docs/DESIGN.md:511, 532` |
| A5 | **内存预算浮现**:8 MiB/endpoint 固定(4 MiB×2 socket buffer,无配置项)+ quinn 连接级 `receive_window` 默认无上界(见 (b)3)——miniss 不提示不约束 | 无 | `quic.rs:125`;quinn-proto `config/transport.rs:367` |

### (b) quinn 默认够用、非 miniss 缺口

1. **空闲/活性**:`max_idle_timeout` 30s(RFC 9308 §3.2 推荐)、
   `keep_alive_interval` None——语义合理,调优经 `TransportConfig` 即可。
2. 并发流 bidi/uni 各 100;流接收窗口 1.25 MiB;CUBIC 拥塞控制;
   TLS 1.3 内建(无 TLS 版本策略问题)。
3. **注意项**:`receive_window`(连接级聚合流控窗口)= `VarInt::MAX`
   ——连接级**无上界**;实际内存上界 = 流窗口 × 并发流数(最坏
   200 流 × 1.25 MiB ≈ 250 MiB/连接的**理论上界**,实际由应用读取速度
   决定)。生产建议显式收紧。datagram 缓冲:收 1.25 MiB / 发 1 MiB。
4. **握手防洪**:stateless retry 在 quinn 可用(`ServerConfig::use_retry`
   / `Incoming::retry`),miniss 不拦不用——应用决策,测试未覆盖 retry 路径
   (quinn 上游测试背书)。
5. TLS 证书轮换:`Endpoint::set_server_config` 支持热换,应用可用。

### (c) 生态限制(miniss 改不了,记录在案)

1. `QuinnRuntimeTrait::spawn` 无准入结果——驱动丢失只能 `tracing::error`
   (`quic.rs:52-62` 的注释即为此做的显式取舍);备选是预留容量或响亮 panic,
   当前选择"安静 + 日志"。
2. h3 0.0.8 / h3-quinn 0.0.10:0.0.x,pre-1.0,API churn 与成熟度风险;
   且只是 dev-dep——**crate 连生产消费者都不含**。
3. 每连接 1 驱动任务 + 每 endpoint 1 驱动任务:10 万连接 = 10 万个 shard
   任务,成本模型与 TCP(内核 socket、零任务)根本不同——QUIC 的固有税,
   选型时必须知道。

## 3. 次要观察(非缺陷,供审查)

- `ensure_owner` 每次 `poll_recv`/`try_send` 都做一次
  `owner_core` CAS——首绑后可改 load 快路径;单 socket 单 owner 无竞争,
  每包 ~20 cycles 级,影响小。
- macOS poller 未就绪时每 1ms 一次 0 超时 `poll` + timer 重试:忙度有界
  (≤1000 次/秒/endpoint),注释已给 kqueue EVFILT_WRITE 不可靠的依据。
- `poll_recv` 用 `Op::PollAdd`(readiness)而非 Recv(completion):先
  recvmmsg 后武装,与借式流 API 同型。若 ④ 分支的 multishot recv/buf_ring
  将来覆盖 UDP,可评估常驻多射,但 PollAdd 本身便宜,收益需实测。

## 4. 生产化最小集(按性价比排序)

1. **QUIC echo 示例 + DESIGN.md 一段"QUIC 生命周期归你"**——一天级,
   消灭最大误解面(A4)。
2. stats→metrics 桥:`Connection::stats` 采样进 per-shard 指标(A2)。
3. accept 包装设施:连接计数 + 上限 + `refuse`(A1)。
4. shutdown 桥:runtime 关停钩子触发 `endpoint.close`+`wait_idle`(A3)。
5. TransportConfig 推荐档案:收紧 `receive_window`、retry 决策、
   socket buffer 可配置化(A5/b3)。

## 5. 未验证项

- 未重跑 `tests/quinn_h3.rs`(本审计为读码;CI 双平台常跑)。
- retry 路径 miniss 侧零覆盖。
- orb io_uring 上 >128 连接压力(`MINISS_QUIC_CONNECTIONS`)未在本机复现。

**台账**:无新增缺陷条目。#41 已记录的测试体竞态是唯一已知缺陷
(测试自身构造问题,非产品代码);A1-A5 是缺失功能不是缺陷,不入缺陷台账。
