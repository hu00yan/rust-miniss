# QUIC 完整功能:最快正确路径(2026-10-31)

> **结论:"h3 能搞定大部分"——对传输层成立,对管理面不成立。**
> QUIC 传输(RFC 9000/9001/9002)quinn 0.11.11 已全部给出,而 miniss 的
> 驱动适配层(`src/quic.rs`,796 行)已建成且有测试背书——这一层**零新增工作**。
> HTTP/3(RFC 9114)h3 0.0.8 覆盖核心语义(请求/响应、QPACK、trailers、
> 优雅关停)≈ 单请求-响应型服务的全部需要;**不含** server push、HTTP/3
> datagram(在独立 crate `h3-datagram` 0.0.2)、WebTransport(0.0.8 里只有
> 2 行 stub)。真正的缺口是 quic-resource-audit-2026-10.md 定位的管理面
> A1-A5——**没有**任何生态 crate 替 miniss 建这层,而且 h3 目前只是
> dev-dependency,crate 连一个生产 QUIC 消费者都没有。总计:
> 传输层 100% 免费、H3 语义 ~85-90% 免费、管理面 0% 免费(但每项都是
> S/M 规模,且设计约束已经明确)。决策:**adapter-on-quinn,不搞原生实现**,
> 前提是步骤零的微基准证明每连接任务成本可控(见 §4,发现 C 直接适用)。
>
> 版本基准(Cargo.lock 解析):quinn 0.11.11 / quinn-proto 0.11.15 /
> quinn-udp 0.5.14 / h3 0.0.8 / h3-quinn 0.0.10。quinn 以
> `default-features = false, features = ["rustls-ring"]` 引入
> (Cargo.toml:42)——qlog、bloom 等 feature 未启用。
> 输入:`wt-quic-audit/docs/review/quic-resource-audit-2026-10.md`(A1-A5 出处)、
> `wt-http-audit/docs/review/performance-baseline-2026-10-30-gcloud.md` 发现 C、
> AGENTS.md、`src/quic.rs`、quinn/h3 本地源码逐条核对。

## 1. quinn/h3 免费能力清单(逐条源码核对)

判定分三档:**免费**(已在本仓库跑通或有 quinn 上游测试背书,零新增)、
**要 miniss 工作**(能力在,设施缺)、**不可用**(生态当前版本没有)。

| 能力 | 判定 | 证据 |
|---|---|---|
| TLS 1.3 握手 + ALPN | **免费** | rustls-ring provider;`tests/quinn_h3.rs:33-66` 完整 TLS13 配置,CI 双平台常跑 |
| 双向/单向流 + 并发流限 | **免费** | `quinn-0.11.11/src/connection.rs:327`(accept_uni)、`:344`(accept_bi);默认 bidi/uni 各 100(审计 §2b2) |
| 流控 + 拥塞控制(CUBIC) | **免费**(调优经 TransportConfig) | quinn-proto 内建;连接级 `receive_window` 默认 `VarInt::MAX` 无上界(审计 §2b3)→ A5 收口 |
| 0-RTT | **免费** | `connection.rs:82-120`(`into_0rtt`/`ZeroRttAccepted`);`tests/quinn_h3.rs` 已含 0-RTT ticket 重用测试 |
| 连接迁移/rebind | **免费** | PathChallenge/PathResponse 处理 `quinn-proto-0.11.15/src/connection/mod.rs:2767,2790`;rebind 测试在 `tests/quinn_h3.rs` |
| QUIC datagram(RFC 9222) | **免费**(QUIC 层) | `connection.rs:436` send_datagram、`:464` send_datagram_wait、`:352` read_datagram |
| Stateless retry | **免费**(quinn 侧)/ **要 miniss 工作**(覆盖为零) | `incoming.rs:52` `Incoming::retry`、`ServerConfig::use_retry`;quinn 上游测试背书,miniss 侧零覆盖、从未调用(审计 §2b4) |
| 密钥更新 | **免费**(自动) | quinn-proto 在 AEAD 约束处自动更新;手动触发 `initiate_key_update` 只在 proto 层(`mod.rs:1299`),quinn::Connection 不暴露——够用 |
| qlog | **要 miniss 工作**(一个 feature + 接线) | `quinn-proto-0.11.15/Cargo.toml:61` `qlog = ["dep:qlog"]`、`proto/lib.rs:53` `QlogStream`;miniss 未启用 |
| 连接/endpoint 统计 | **要 miniss 工作**(= A2 桥) | `connection.rs:537` `Connection::stats()`、`endpoint.rs:94` `Endpoint::stats()`;`ConnectionStats{udp_tx,udp_rx,frame_tx,frame_rx,path}`(`quinn-proto .../connection/stats.rs:163`)数据全在,无人采集 |
| 证书热轮换 | **免费** | `endpoint.rs:275` `set_server_config` |
| 优雅关停/排水原语 | **免费**(原语)/ **要 miniss 工作**(= A3 编排) | `endpoint.rs:299` close、`endpoint.rs:323` wait_idle;测试全是手动 close+wait_idle |
| GSO/GRO、MTU-D | **免费** | `quic.rs:355-365` 透传 quinn-udp;`endpoint.rs:141` `allow_mtud = !socket.may_fragment()` |
| h3 请求/响应 + QPACK + trailers + 优雅关停 | **免费** | `h3-0.0.8/src/server/connection.rs:85,105,123,160`;`server/stream.rs:119,160,174,181`(send_response/send_data/send_trailers/finish);`src/qpack/` 动态表 |
| h3 server push | **不可用** | `client/connection.rs:384-385`:`shutdown(_max_push)` 的 TODO 原文 "once server push is implemented" |
| HTTP/3 datagram(RFC 9297) | **不可用**(h3 0.0.8 内)/ **可选外部 crate** | `h3-datagram 0.0.2`(hyperium/h3 仓库,crates.io 108.8 万下载,2025-05 更新);h3-quinn 0.0.10 不提供该扩展绑定;compio-quic 的 `h3` feature 用它 |
| WebTransport | **不可用** | `h3-0.0.8/src/webtransport/mod.rs` 全文 2 行(仅 session_id) |
| **连接数上限/拒绝策略、metrics 桥、shutdown 排水、内存预算浮现、示例文档** | **全部要 miniss 工作** | 审计 A1-A5;quinn 无内置 max-connections,`endpoint.accept()` 裸露(`endpoint.rs:174`) |

要点:协议栈本身没有"要写协议代码"的缺口——所有标"要 miniss 工作"的条目
都是管理/观测/资源设施,不是 QUIC 或 H3 实现。

## 2. 先例与 adapter-vs-native 决策

业主问的"monoio-quic"**不存在**。逐个核实(2026-10-31,crates.io API):

| 先例 | 实际是什么 | 证据 | 对本决策的含义 |
|---|---|---|---|
| **monoio-quiche**(ByteDance/monoio 生态) | 包 Cloudflare **quiche** 的薄封装(477 行 Rust),非原生 | crates.io:仅 2 个版本(2023-03 的 0.0.1、2025-04 的 0.0.2),总下载 2175、近期 3;lib.rs 标注 unofficial + "Do not use it in production" | thread-per-core 生态自己也没走通"包 C/独立状态机"路线:两年两版,实质弃维护 |
| **compio-quic**(compio 生态,thread-per-core + 纯 io_uring,与 miniss 同族) | **quinn-proto 状态机 + 自研 completion 式驱动**,3249 行 | crates.io:17 版(2024-09 → 2026-09),55.5 万总下载/近期 19.9 万;feature 表:`ring = quinn-proto/rustls-ring`、`qlog = quinn-proto/qlog`、`h3 = [dep:h3, dep:h3-datagram]` | **决定性先例**:同族运行时选择复用 quinn-proto 而非原生实现;即便如此也花了 ~2 年/3.2k 行才到 0.8,且证明 h3 0.0.x 可在非 tokio 运行时上承载生产路径 |
| **quinn on glommio/kimojio 式运行时** | 无已发布的 Runtime trait 适配 crate | web 检索无果(只有 quinn-plus/quinn-boring 等杂项) | miniss 的 `MinissQuicRuntime`(`quic.rs:44-72`)本来就是生态里没人发布过的那块拼图——**而且已经建好测好** |
| gm-quic(纯原生 Rust QUIC) | 原生实现路线的活样本 | GitHub;成熟度远不及 quinn | 原生路线的工期量级:数年 |

**决策:adapter-on-quinn。** 理由按权重:
1. **最难的部分已经完成**。驱动适配(timer→时间轮、spawn→shard、UDP 包装、
   跨 shard 退役预留、owner 绑定、macOS kqueue 绕行)在 `quic.rs` 全部落地,
   `tests/quinn_h3.rs` 5 测试 CI 双平台常跑(审计 §1)。换任何路线都从零开始。
2. **原生实现的成本有量化先例**:compio-quic 复用了 quinn-proto 仍需 3.2k 行
   和 2 年;纯原生(gm-quic)是数年工程。而 A1-A5 全部加起来是 ~1k 行量级(§3)。
3. **monoio-quiche 证明了负先例**:换协议栈(quiche)不省事,反而引入 C 绑定
   与 completion 模型错配,生态最终死掉。
4. **发现 C 的风险不因换库消失**:发现 C 的嫌疑面是 miniss 的每连接
   spawn/drop 生命周期与跨任务唤醒(`performance-baseline-2026-10-30-gcloud.md`
   发现 C.2),不是 quinn 本身。换原生实现同样要在 shard 上 spawn 每连接任务。
   正确动作是**先测量**(步骤零),量出来再定,而不是赌一个重写。

保留的替代路线(推翻条件触发时启用,见 §6):compio-quic 式——保留
quinn-proto 状态机,把连接驱动融合进 endpoint 事件循环(每 endpoint 一个任务,
消除每连接任务),仍然不是"原生协议"。

## 3. A1-A5 最小设计(miniss 语义)

共同约束(AGENTS.md):热路径 shard-local(MUST #3)、跨核只走有界通道
(MUST #4)、只读配置可 Arc(MUST #5)、per-shard 计数后台聚合(MUST #6)。
QUIC 的一个关键事实让这些设计比 TCP 侧简单:**endpoint 在其 owning shard 上
构造,连接驱动经 `MinissQuicRuntime::spawn` 全部落在同一 shard**
(`quic.rs:49-63` + `http_h3_echo.rs` 已记录的约束)——accept 路径天然单 shard,
没有跨核竞争面。

### 步骤零:每连接生命周期微基准(先于一切,规模 **M**,2-3 天)

发现 C 显示 hyper 每连接 spawn/drop 生命周期在 miniss 上塌陷 ~4-20x
(串行化 ~300µs/请求)。QUIC 是**每连接 1 驱动任务 + 每 endpoint 1 驱动任务**
(`connection.rs:66`、`endpoint.rs:153` 的 `runtime.spawn`)——10 万连接 =
10 万 shard 任务(审计 §2c3)。这个成本决定 QUIC-on-miniss 能否上生产,
**必须先测**。

- 产物:`benches/quic_conn_lifecycle.rs`(按 bench 目标惯例,不进 `just check`,
  走显式 recipe;同机 summary.csv 对照)。
- 测四个量(单 shard,--cores 1):
  1. 基线:`spawn_local` 一个立即返回任务的 spawn+poll+drop 成本
     (与既有 `examples/spawn_micro.rs`/`spawn_latency.rs` 数字对齐);
  2. **握手churn**:N 连接顺序 close(0,"")→ 重新连接,每连接
     (spawn 驱动 + TLS 握手 + close + drop)墙钟成本;
  3. **并发churn**:c8/c32 并发上述循环,看是否出现发现 C 型串行化天花板;
  4. **跨任务唤醒**:连接 A 的请求唤醒连接 B 的驱动任务(h3 流回声,
     两连接同 shard),量单次唤醒延迟分布。
- 通过线:并发 churn 每连接成本 < ~100µs 且不随并发数塌陷到发现 C 的
  300µs 串行签名。**失败线:复现 ~300µs/请求签名 → 冻结 QUIC 上生产,
  启用 §6 替代路线**。
- 架构审计:纯 benchmark,不新增共享状态/锁/runtime。

### A1 连接管理(计数/上限/拒绝)——规模 **M**(~250 行 + 测试)

- 每个运行 endpoint 的 shard 持有 shard-local 连接表(`Cell<ConnSlab>`:
  活跃连接 id + `quinn::Connection` 句柄),归 accept 循环任务独占。
- accept 点决策全部本地:对每个 `Incoming` 先查计数,满则 `refuse()`
  (`incoming.rs:44`),可选 `retry()`(`incoming.rs:52`)做防洪——
  retry 顺带补上审计 §5 的覆盖缺口。上限为只读配置 `Arc<QuicLimits>`
  (MUST #5;arc-swap 式替换留接口)。
- 多 endpoint(SO_REUSEPORT 每 shard 一个,`http_h3_echo.rs` 模型)时
  每 shard 各自计数、各自限额(配置除以 shard 数或逐 shard 显式给)——
  **零跨核流量**;全局视野由 A2 的聚合快照给出(控制面,MUST #6)。
- 对外形态:`QuicAcceptor` 包装器(endpoint + limits + 可选 per-accept 回调),
  测试沿用 `tests/quinn_h3.rs:25-31` 的 `MINISS_QUIC_CONNECTIONS` 环境变量语义
  升级为 API。

### A2 stats→metrics 桥——规模 **S/M**(~150 行 + series 表)

- 采样任务:endpoint shard 上 `spawn_local` 的周期任务(时间轮,1s 级间隔),
  从 A1 的连接表逐连接读 `Connection::stats()`、再读 `Endpoint::stats()`,
  **delta 编码**进 `CoreMetricsSnapshot` 新增的 quic 字段
  (`multicore.rs:533` 结构体扩列,与 `snapshot_fields_and_series_stay_in_sync`
  静态断言同型)。
- 导出:在 `metrics.rs:133+` 的 series 表追加 quic 系列
  (`quic_handshakes_accepted`、`quic_udp_tx/rx`、`quic_path_events`、
  `quic_connections_active`(HighWaterMark 合并))——复用现成的
  `CoreMessage::CollectMetrics` 有界通道采样(multicore.rs:1304),
  `SeriesDescriptor::merge_all` 免费拿到 runtime 汇总(metrics.rs:87)。
- 架构:per-shard 计数器 + 热路径外聚合,正合 MUST #6;热路径零新增原子。

### A3 shutdown 排水桥——规模 **M**(~200 行 + 测试)

- 现状:runtime 关停不感知 endpoint;`quic.rs:49-63` 注释承认驱动任务
  准入失败只日志、endpoint 静默死;测试全靠手动 `close`+`wait_idle`。
- 设计:`QuicEndpointGuard`(RAII)。endpoint 在 owning shard 构造时向本
  shard 的 shard-local 注册表(`Cell<Vec<...>`,CoreHandle 上一个槽)登记;
  `Drop` 时(必然在该 shard 或经既有退役通道)投递排水消息。
- `MultiCoreRuntime::shutdown_with_timeout`(`multicore.rs:2664`)前插入
  drain 相位:各 shard 先对自己的 endpoint 执行 `close(err)` → 有界等待
  `wait_idle()`(`endpoint.rs:323`,H3 层用 `client/server Connection::wait_idle`
  + `poll_close`,`h3 client/connection.rs:391,397`——语义与
  `tests/quinn_h3.rs:161` 现有模式一致),超时则强拆,然后才进现有 Shutdown 流程。
- 这同时把 `quic.rs` 的静默驱动丢失竞态变成显式生命周期错误(有界超时可见)。

### A4 示例与文档——规模 **S**(1 天)

- `examples/http_h3_echo.rs`(358 行)**已存在于 wt-http-audit 工作树**,
  含 `--selftest` 内进程 h3 客户端、per-shard endpoint 构造约束注释——
  cherry-pick/move 到主线即可,不必重写。
- DESIGN.md 增加"QUIC 生命周期归你"一节:与 TCP 对比的内核/用户态责任表
  (审计 §0 已给出内容骨架)、内存公式、上限决策点。消灭最大误解面。

### A5 内存预算浮现——规模 **S**(~80 行,多为配置+文档)

- `TransportConfig` 推荐档案:显式收紧 `receive_window`/`stream_receive_window`
  (默认连接级 `VarInt::MAX` 无上界,最坏 200 流 × 1.25 MiB ≈ 250 MiB/连接
  理论上界,审计 §2b3)、datagram 缓冲说明。
- `quic.rs:125` 的 4 MiB socket buffer 常量提为可配置项;文档化
  "8 MiB/endpoint 固定 + 每连接公式"。

## 4. 排序路线图

| 步 | 内容 | 规模 | 产出/退出判据 |
|---|---|---|---|
| **0** | 每连接生命周期微基准(§4 步骤零) | **M**,2-3 天 | go/no-go 证据;失败 → 改融合驱动设计再评估 |
| 1 | A4 示例+文档(cherry-pick http_h3_echo) | S,1 天 | 端到端可跑的 H3 echo,误解面清零 |
| 2 | A5 TransportConfig 档案 + buffer 可配置 | S,1 天 | 内存上界文档化,生产参数有出处 |
| 3 | A2 stats 桥 | S/M,2-3 天 | metrics_export 可见 quic 系列 |
| 4 | A1 QuicAcceptor(含 retry 路径测试) | M,3-4 天 | 上限可配,refuse/retry 有覆盖 |
| 5 | A3 shutdown 桥 | M,3-4 天 | 关停排水有界,驱动丢失显式化 |
| 6 | qlog feature 评估 + h3-datagram(h3-datagram 0.0.2)PoC + gate 压测(同机 baseline) | 各 S/M,按需 | 观测面完整;bench-smoke GREEN |

- **天级**:步骤 0-2(第一周内可全部落地)。**周级**:步骤 3-5(第二周)。
- **最便宜的端到端证明** = 步骤 1+3+4 的组合(http3_echo + 连接上限 +
  stats 桥):三者互相独立、都不依赖步骤零结果,即使步骤零失败也保留为
  诊断设施。
- **风险未知点排序**:① 发现 C 型每连接成本(步骤零直接回答,最高优先);
  ② h3 0.0.x API churn(固定精确版本;compio-quic 全程 0.0.x 跑通生产路径
  是缓释证据);③ macOS kqueue 下高 churn 的 1ms poller 重试循环
  (`quic.rs:471-510`,≤1000 次/秒/endpoint 忙度)在 churn 场景的叠加;
  ④ `receive_window` 默认无上界在生产流量下的真实内存(步骤 2 先行收口)。

## 5. 风险与推翻条件

1. **步骤零复现发现 C 签名**(每连接 close-churn 串行化 ~300µs 级):
   推翻 adapter-on-quinn 的"每连接一任务"形态,转向 compio-quic 式
   quinn-proto + 融合驱动(每 endpoint 一任务)。这是设计变更,**不是**
   转向原生协议实现——原生实现(vendor 自研状态机)在任何测量结果下都
   不入围(工期数年 vs 管理面 ~1k 行)。
2. **h3 0.0.9+ 破坏 server API**:固定 0.0.8/h3-quinn 0.0.10 于 Cargo.lock;
   升级按依赖审计流程单独走。h3 的
   `i-implement-a-third-party-backend-and-opt-into-breaking-changes` feature
   存在说明上游承认 backend 层 churn,适配面在 h3-quinn,不在 miniss。
3. **quinn semver major**:重审 `quic.rs` 适配层(Runtime trait 是 quinn 的
   扩展点,0.11 内已稳定;major 升级按审计流程重跑双平台测试)。
4. **WebTransport/push/datagram 需求提前到来**:h3 0.0.8 无此三者;
   h3-datagram 0.0.2 + 自写 backend 绑定是可行但独立的工作项,不应混入
   本路线图的关键路径。
5. **多 runtime 同进程场景**:metrics 模块已声明两 runtime 会合并成无意义
   序列(metrics.rs:7-8),QUIC 系列沿用同一假设;若未来需要,按 series
   标签拆分,不阻塞本计划。

**决策日志待录**(落地时按 DECISION_LOG 四件套):选择 adapter-on-quinn +
管理面自建 A1-A5;拒绝原生 QUIC 实现(gm-quic 路线)与 quiche 封装路线
(monoio-quiche 先例);证据 = 本文档 §1 源码清单 + §2 先例表 + 步骤零
微基准;推翻条件 = 上述 1-3 条触发。
