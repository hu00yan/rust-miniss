# miniss vs Photon(PhotonLibOS)完整对照基准:cloud shell 1 核 + OrbStack 4/7 核(2026-10-31)

> 回答"和 Photon 比到底差多少、差在哪"的任务卡。Photon 侧用的是**它自己的
> benchmark 代码**:`examples/perf/http/http-server.cpp`(官方 HTTP perf
> server,commit `5641e27`,release 构建)+ 我们按它自家
> `examples/perf/net-perf.cpp` 的多 vCPU 模式加的两个开关
> (`--vcpus N` 每核一线程各自 `photon::init` + 同端口 SO_REUSEPORT 监听、
> `--event_engine epoll|iouring`、`--socket tcp|iouring`);客户端沿用
> miniss 矩阵的中立 wrk(偏差见下)。miniss 侧四个服务器:最小 framer
> (`http_echo`)、hyper h1(`http_hyper_echo`)、hyper h2c
> (`http_hyper_h2_echo`)、hyper-on-tokio 对照,外加 monoio(仅 orb)。
> 方法论与已提交矩阵一致:3 轮 × 10s、每轮顺序轮换(interleaved
> order-swapped)、每次测量前 warmup `-t1 -c10 -d3s`、
> `MINISS_URING_ENTRIES=1024`、全部格子零 socket/HTTP 错误、零进程泄漏、
> 服务器有界等待退出(SIGTERM → 200ms → KILL 兜底)。
> 原始件:`bench/results/photon-matrix-20261031/`(两场地 CSV+raw tgz)。

## 方法(venue / cores / 拓扑 / 线程映射)

| venue | 核数 | kernel | 说明 |
|---|---|---|---|
| GCP Cloud Shell | 1(2 vCPU SMT) | 6.18.55+ | 干净 1 核场地,同 10-29/10-31 基线 |
| OrbStack VM | 4、7 | 7.2.12-orbstack | VM,绝对数只作形状证据;自同日对照 |

- server 与 wrk 同机共置、不绑核(与基线一致);miniss `--cores N` =
  N worker 线程 + 每 worker SO_REUSEPORT 监听(内核派发);photon
  `--vcpus N` = N OS 线程、每线程独立 `photon::init` 事件引擎 + 同端口
  SO_REUSEPORT(与 miniss 同为内核派发模型);tokio = `worker_threads(N)`;
  monoio = 其自带多线程模型。photon 官方单 vCPU 拓扑(`--vcpus 1`)即
  上游 example 原样。
- 场景:ka8(`-t1 -c8` keep-alive)、ka32(`-t2 -c32`)、close32
  (`-t2 -c32` + `Connection: close`);h2c 用 h2load `-m1`(wrk 不支持
  h2c,h2load 无 close 模式),photon 不支持 h2c 故无对照行。
- 响应体对齐:photon 官方 server 默认 4096B 静态 body;矩阵里统一
  `--body_size 22`,与 miniss 各 echo 的 22B body 相同(头部集合仍有
  小差异:photon 走完整 http 框架带 Server/Date 头,miniss framer 是
  104B 固定响应——framework-vs-framework 的公平对照是
  miniss-hyper-h1 vs photon,framer 行是"手写解析器"上界)。
- 每格 (场地 × 核数) 附 photon **官方拓扑**复现:官方 server(4096B)
  + 官方 `http-perf-client --threads 4`(4 个协程同步往返),取 server
  端 qps 日志中位数(官方 client 被 timeout 截断,无汇总行)。

### 与上游的偏差(全部登记)
1. `PHOTON_ENABLE_LIBCURL=OFF`:vendored 静态 curl 在
   BUILD_DEPENDENCIES 下链接序损坏(curl 在 ssl/zlib 之后);被测路径
   (http server/handler、tcp/uring socket、native client)不用 curl。
2. photon `-Werror` 放宽为警告(orb 的 gcc 版本更新,upstream CI 未覆盖)。
3. `http-server-mc.cpp` 为新文件(不改上游文件):qps 计数器改 atomic
   (多 vcpu 下多线程递增);显式指定引擎时必须补 `| INIT_EVENT_SIGNAL`
   (上游默认值自带该位;缺失时 `block_all_signal` 吞掉 SIGTERM,
   进程无法优雅退出——实测踩坑后修复)。
4. photon server 端 SO_REUSEPORT(单 vcpu 上游不设)——多 vcpu 同端口
   模型所需,对 1 vcpu 行为无影响。
5. wrk 版本:cloud shell = 自建 4.2.0(`~/bin/wrk`,与 10-29 基线同源),
   orb = apt 4.1.0;h2load 1.59(cloud shell)/1.6x(orb)。

## 结果表(3 轮中位数,rps / p50 / p99)

### cloud shell,1 核

| 场景 | miniss framer | miniss hyper-h1 | hyper-tokio | photon epoll | photon uring | photon uring-sock |
|---|---|---|---|---|---|---|
| ka8 | **61,254** / 64µs / 2.0ms | 51,911 / 142µs / 3.4ms | 58,217 / 130µs / 4.0ms | 40,404 / 186µs / 4.1ms | 42,863 / 161µs / 4.0ms | 46,599 / 104µs / 2.9ms |
| ka32 | **62,055** / 195µs / 2.3ms | 49,705 / 595µs / 5.2ms | 59,599 / 271µs / 5.2ms | 38,237 / 782µs / 5.4ms | 41,396 / 678µs / 5.5ms | 48,693 / 251µs / 2.9ms |
| close32 | **62,276** / 163µs / 2.3ms | **3,134** / 9.5ms / 15.1ms | 12,891 / 1.8ms / 4.6ms | 37,886 / 796µs / 5.6ms | 40,007 / 695µs / 5.5ms | 48,872 / 256µs / 3.1ms |
| h2c ka8(m1) | — | 3,619(mean 2.16ms) | — | — | — | — |
| h2c ka32(m1) | — | 3,592(mean 8.77ms) | — | — | — | — |

RSS 中位:framer 46.0 MiB、hyper-h1 39.7 MiB、photon 6.4-8.0 MiB、
tokio 3.4 MiB(miniss 含未修的 #45 io_slots 无界增长)。

### OrbStack VM,4 核

| 场景 | miniss framer | miniss hyper-h1 | hyper-tokio | monoio | photon epoll | photon uring | photon uring-sock |
|---|---|---|---|---|---|---|---|
| ka8 | 164,406 / 35µs / 2.3ms | **167,790** / 33µs / 0.68ms | **238,667** / 24µs / 0.62ms | 140,099 / 43µs / 0.89ms | 129,924 / 49µs / 1.2ms | 132,763 / 50µs / 0.45ms | 115,126 / 52µs / 3.4ms |
| ka32 | 340,560 / 56µs / 11.5ms* | 339,424 / 55µs / 2.1ms | 292,316 / 91µs / 2.5ms | 345,472 / 65µs / 3.5ms | **353,408** / 64µs / 1.4ms | 316,814 / 71µs / 2.5ms | 291,332 / 77µs / 0.92ms |
| close32 | **338,769** / 56µs / 2.5ms | **29,326** / 870µs / 6.5ms | 79,576 / 240µs / 2.9ms | 356,524 / 64µs / 2.5ms | 318,979 / 69µs / 2.8ms | 279,778 / 77µs / 7.4ms | 244,328 / 90µs / 5.7ms |
| h2c ka8(m1) | — | 23,954(mean ~250µs) | — | — | — | — | — |
| h2c ka32(m1) | — | 26,212(mean ~880µs) | — | — | — | — | — |

\* r2 单轮 p99 100ms(VM 抖动;三轮 346k/311k/341k,rps 中位未受影响)。
ka8 行所有服务器都压在 8 连接 × RTT 的客户端上限附近(tokio 239k 封顶),
多核下 ka8 是 client-bound 场景,判别力看 ka32/close32。

### OrbStack VM,7 核

| 场景 | miniss framer | miniss hyper-h1 | hyper-tokio | monoio | photon epoll | photon uring | photon uring-sock |
|---|---|---|---|---|---|---|---|
| ka8 | 166,492 / 36µs / 1.9ms | 153,730 / 38µs / 1.0ms | **251,082** / 24µs / 0.36ms | 146,281 / 44µs / 0.53ms | 128,762 / 51µs / 0.31ms | 129,055 / 51µs / 0.40ms | 118,911 / 53µs / 0.64ms |
| ka32 | 335,091 / 61µs / 2.6ms | 332,783 / 62µs / 2.5ms | 309,870 / 87µs / 1.5ms | **390,129** / 62µs / 0.71ms | 347,141 / 65µs / 0.64ms | 344,344 / 69µs / 0.99ms | 322,925 / 72µs / 0.79ms |
| close32 | **341,137** / 59µs / 2.4ms | **37,092** / 692µs / 7.6ms | 79,324 / 246µs / 2.5ms | **380,507** / 62µs / 2.2ms | 335,220 / 67µs / 3.5ms | 340,928 / 70µs / 0.94ms | 323,470 / 72µs / 0.96ms |
| h2c ka8(m1) | — | 30,341(mean ~200µs) | — | — | — | — | — |
| h2c ka32(m1) | — | 45,313(mean ~550µs) | — | — | — | — | — |

RSS 中位:framer 133→143 MiB(c4→c7)、hyper-h1 123→134 MiB、
monoio 2.6→3.1 MiB、tokio 3.1→3.5 MiB、photon 6.2→17.8 MiB(uring 变体
随 vcpu 涨,ring/vcpu)。

## 百分比结论(正数 = miniss 领先)

**(a) vs photon-uring-sock**(c1 上 photon 最强形态):

| 场景 | c1 framer | c1 hyper-h1 | c4 framer | c4 hyper-h1 | c7 framer | c7 hyper-h1 |
|---|---|---|---|---|---|---|
| ka8 | +31.5% | +11.4% | +42.8% | +45.7% | +40.0% | +29.3% |
| ka32 | +27.4% | +2.1% | +16.9% | +16.5% | +3.8% | +3.1% |
| close32 | +27.4% | **-93.6%** | +38.7% | **-88.0%** | +5.5% | **-88.5%** |

**(b) vs photon 三变体中当日最强**(c4/c7 的 ka32/close32 上 photon-epoll
或 photon-uring 更强,用最强的算才公平):

| 场景 | c1(vs uring-sock) | c4(vs epoll) | c7(vs epoll/uring) |
|---|---|---|---|
| ka8 framer | +31.5% | +26.5% | +29.3% |
| ka32 framer | +27.4% | **-3.6%** | **-3.5%** |
| close32 framer | +27.4% | +6.2% | +0.1% |
| ka8 hyper-h1 | +11.4% | +29.2% | +19.4% |
| ka32 hyper-h1 | +2.1% | -4.0% | -4.1% |
| close32 hyper-h1 | -93.6% | -90.8% | -89.1% |

一句话:**1 核上 miniss(framer 与 hyper-ka)全面领先 photon
+2%~+31%;多核高连接(k a32)打平(±4%);多核低连接仍是 miniss 领先
(+19%~+46%);close-churn 上 miniss-framer 依旧领先(0~+39%),而
miniss×hyper 塌到 photon 的 ~10%**(见发现 C)。

photon 官方拓扑(自家 client,4 协程同步往返,4096B):c1 = 29.5k(epoll)
/ 29.8k(iouring) qps,c4 = 60k,c7 = 67-68k——该拓扑 client 侧自限,
只作"官方代码可跑通"的基线注记。

## 发现 C 在多核下的形状(hyper×miniss 交互缺陷,台账级)

1. **塌陷仍在,且不是固定 3.4k 天花板,而是"每 worker 串行化"**:
   h1 close32:3,134(c1)→ 29,326(c4)→ 37,092(c7);按 worker 均摊
   3.1k → 7.3k → 5.3k,即单 worker 串行成本从 ~300µs/req 降到
   ~140-190µs/req,但仍只有 framer 同格的 **0.05x-0.11x**
   (c1 3.1k vs 62.3k,c7 37.1k vs 341k)。
2. **h2c 同形状**:3.6k(c1)→ 24.0-26.2k(c4)→ 30.3-45.3k(c7),
   ka32 反而比 ka8 快(c7 45.3k vs 30.3k,更多并发流摊薄了串行点),
   同样是"随核数放大但 per-worker 顶格"的签名。
3. **与 hyper 自身对照**:hyper-on-tokio close32 = 12.9k(c1)/
   79.6k(c4)/ 79.3k(c7),miniss×hyper 是它的
   0.24x → 0.37x → 0.47x——放大倍数随核数收窄(4.1x → 2.7x → 2.1x),
   即 #46 修复 + 多 worker 分摊了部分串行,但**交互缺陷没有消失**。
4. **keep-alive 路径已康复**:c4/c7 上 hyper-h1 ka32 = 339k/333k ≈
   framer(341k/335k),ka8 甚至全场第二(167.8k,超 monoio 140k、
   photon 全变体 115-133k)。对比 10-31 基线(修复前 c1 hyper-ka 只有
   31-36k,0.54-0.66x tokio),今天 c1 hyper-ka = 49.7-51.9k
   (0.83-0.89x tokio):**#46 自适应阻塞让 hyper 集成的 ka 路径从
   "明显低效"变成"与 framer 同档"**,剩余差距集中在 churn/h2c 的
   连接生命周期路径(spawn/drop、readiness 传播、多 op 完成唤醒)。

## Photon 用了什么我们没用到的手法(读源码所得,按可操作性排序)

1. **io_uring SQE 统一在事件循环 drain 点批量提交**(`io/iouring-wrapper.cpp:214,479`):
   单个 op 只 prep SQE,`io_uring_submit` 集中在 `wait_and_fire_events`
   每轮一次,一次 enter 带走全部排队 SQE;另提供 `eager_submit` 模式
   (提交后立刻 `reap_events()` 内联收割,完成则 `thread_yield()` 短路,
   完全不进睡眠)。对照 miniss 实测 0.432 enter/req 且 0.125 有效
   (基线发现 A 的空转 enter)——把 submit 移到 drain 点是消掉空转
   enter 的直接蓝图。
2. **CQE→eventfd→级联唤醒,零自旋零 park 超时**(同文件 `wait_for_events`):
   uring 引擎用 eventfd 接 CQE,等待协程 `thread_sleep(-1)` 挂起,由
   reaper 显式唤醒(`wait_and_fire_events` → per-thread fire event)。
   没有 100µs park 超时、没有 busy-poll;#46 修的是"空转时去睡",
   photon 的做法是"根本不醒来直到 CQE"。miniss 的下一步:完成事件
   直驱唤醒 + 可选 SQPOLL(`IORING_SETUP_SQPOLL` 在 photon 是一个
   参数,非默认)。
3. **registered files / fixed-file socket**(`net::new_iouring_tcp_server`,
   实测日志 `register_files is enabled`):固定 fd 集合减每次 op 的 fd
   映射;photon 在 uring-sock 变体上拿到 c1 最强 HTTP 数字
   (46.6-48.9k)。miniss 的 uring backend 尚未用 register_files。
4. **带超时 op 的 linked-timeout + cancel 协议,等待上下文全栈分配**
   (`iouring-wrapper.cpp:255-335`):每个可超时 op 用
   `IOSQE_IO_LINK` 的 link_timeout,CQE 与超时两个完成源在栈上
   ioCtx 会合,reaper 显式 drain 后才返回;无堆分配、无悬垂。miniss 的
   IoToken 池(#45 修复前无界增长)可以对照这个"栈上会合 + 显式 drain"
   模型简化。
5. **多 vcpu = 每线程独立引擎 + 显式迁移**:`net-perf.cpp` 的 server 端
   就是每 OS 线程一个 `photon::init`(与 miniss 的 thread-per-core 同
   构);跨核路径交给 `WorkPool` + `thread_migrate`(最新提交 #1721 还在
   调它的 yield 参数)——即 photon 也认为跨核 handoff 是热点,用显式
   迁移语义而非隐式窃取。photon 默认**不开**栈池
   (`STACK_ALLOCATOR_DEFAULT`=malloc),pooled 栈是可选项,不是它
   快的原因。

## 结论与后续工作方向

1. **调度器基本盘(1 核 + 多核 ka/churn 吞吐)已经不输 photon**:
   framer 对 photon 全变体 +0~+39%,ka32 高连接打平(±4%),ka8 低连接
   领先 +26~+46%;#46 之后 orb 多 worker 不再塌陷(monoio 345-390k
   同档)。性能故事的主叙事可以从"追平一线"转为"hyper 集成层"。
2. **下一个最大的石头仍是发现 C**:hyper×miniss 的连接生命周期串行化
   (~140-300µs/req,随核数放大但 per-worker 恒在,ka32 并发可摊薄)。
   修复面按嫌疑排序:每连接 spawn/drop 生命周期(`spawn_local` 准入/
   回收)、readiness 跨任务传播、单连接多 op(read+write)的完成唤醒
   批处理——photon 的"drain 点批量 submit + CQE 显式唤醒"同时覆盖
   后两条,是现成参考实现。
3. **发现 A(空转 enter)的量化收益预期**:photon 的批量 submit 模型
   把 enter 数压到 ~1/循环;若 miniss 仿照, ka 路径的 enter/req 有望从
   0.43 降到 ~0.1,直接兑现为 c1 场景的 p99(2-5.5ms → photon-sock 的
   2.9-3.1ms 档)与低连接吞吐。
4. **RSS 不是借口**:miniss 46-143 MiB vs photon 6-18 MiB(本树未含
   #45 io_slots 回收修复,方向已验证)。photon 的栈分配默认都不池化,
   其小 RSS 来自 op 上下文栈分配 + 无每-op 堆 token。
5. h3 无负载工具(两场地都没有 h2load 之外的 h3 压测器),与基线文档
   结论一致;正确性由 quinn 测试与 example 自检覆盖。

## 例程与脚本的架构审计(AGENTS.md MUST #8)

本分支新增文件:`bench/photon/http-server-mc.cpp`(C++,构建进
PhotonLibOS checkout,不在本 crate)、`bench/photon/setup_photon.sh`、
`bench/photon/bench_photon_matrix.sh`、`bench/photon/summarize.py`
(benchmark 编排与汇总工具,运行在仓库外/离线)。Rust 热路径零变更:
无新增共享可变状态、无锁、无原子、无 runtime 创建、无跨核流量;本分支
的 Rust 侧增量 = #46 自适应阻塞(`src/multicore.rs`,已在
fix/multiworker-adaptive-blocking 审计)+ hyper echo examples 与
hyper_tokio 对照(已在 bench/hyper-http-audit 审计,SO_REUSEPORT 每
worker 一监听、accept 与连接处理 spawn_local 留在本 shard)。
benchmark 运行期确实存在的阻塞调用(进程 spawn、kill/wait)全部在
编排脚本里,不在被测进程内。
