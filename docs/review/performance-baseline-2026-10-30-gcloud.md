# 跨运行时基准:Cloud Shell 内核 6.18.55(2026-10-29)


> 环境:GCP Cloud Shell,2 vCPU = **1 物理核的 SMT 兄弟**(miniss 正确拒绝
> `--cores 2`),server/wrk 共置无绑核,单 worker。3 轮交错、10 秒/轮、
> ring 对齐(MINISS_URING_ENTRIES=1024 = monoio harness 的 1024)。
> **定性:形状 + 机制归因证据,不是绝对排名**——共享放置下个位数百分比
> 差异不可分辨。原始件:`bench/results/http-matrix-20261029-163437/`。
> 全部 45 格:零 socket/HTTP 错误、干净退出、零进程泄漏。

## 吞吐与延迟(3 轮中位数)

| 场景 | miniss | monoio | compio | tokio | glommio |
|---|---|---|---|---|---|
| ka8 | 68,075 / 105µs / 380µs | 66,764 / 95µs / 4.18ms | 70,239 / 90µs / 4.13ms | **77,879** / 52µs / 181µs | 65,284 / 105µs / 3.86ms |
| ka32 | 77,848 / 199µs / 4.64ms | 70,986 / 190µs / 3.73ms | 72,726 / 184µs / 5.14ms | **79,391** / 146µs / 2.20ms | 68,477 / 206µs / 5.63ms |
| close32 | 77,908 / 196µs / 4.71ms | 70,824 / 189µs / 4.18ms | 72,819 / 184µs / 5.04ms | **79,121** / 160µs / 2.20ms | 68,480 / 206µs / 5.63ms |

- **miniss 对 monoio 全场景 +2.0% ~ +11.2%**;对 compio -3% ~ +7%;
  对 tokio 在 c32 仅 -1.5~-1.9%(ka8 -13%)。这是第一次在干净内核上、
  ring 对齐、3 轮取中位数的对比——2026-07 那份"ka800 并列"的形状结论
  在低连接区间得到修正性支持。
- p99 形状:tokio 全场 ≤2.2ms;三个 uring 家族在 c32 聚在 3.7-5.6ms。
  miniss 在 c8 有 uring 家族最低 p99(380µs),c32 收敛到 ~4.7ms。

## 机制归因(每 10k 请求 CPU 毫秒 / 每请求 syscall)

| 指标 | miniss | monoio | compio | tokio | glommio |
|---|---|---|---|---|---|
| CPU ms/10k req(ka32) | **117.0** | 124.3 | 123.5 | 101.4 | 133.0 |
| io_uring_enter/req | 0.432(**仅 0.125 成功**) | 0.250 | 0.250 | 0 | 0.501 |
| 全 syscall/req | 1.438 | **0.251** | **0.251** | 2.130 | 2.508 |
| RSS(中位) | **191.0 MiB** | 2.6 | 2.7 | 2.9 | 13.6 |

### 发现 A:0.31 enter/请求是空转(新增优化目标)

miniss 的 enter 里只有 29% 产出工作,其余 ~0.31/请求是 **EAGAIN/超时空转**
——与 park 100µs 超时模型一致:单 worker 在 c8 负载下大部分唤醒来自
park 超时而非 CQE(≈21k 次/秒超时唤醒)。这正是文档里长期怀疑的
"park/内核调度方差"贡献 p99 的量化现场。方向:自适应 park 超时 /
完成事件驱动唤醒,把空转 enter 换成 CQE 唤醒。strace 窗口内自旋重的
miniss 掉到 12.4k RPS(monoio 掉得最少)——自旋循环对探针最敏感,
计数方向可信、绝对值不可比。

### 发现 B:RSS 差距 ~70x(已破案,2026-10-30)

**不是预分配。** 复测:idle RSS 8.8MiB——TaskArena 65,536 槽 + 队列 + ring
+ slab 的算术完全成立(见台账 #45 前的估算)。真正原因:**`io_slots` 随
累计 op 数线性无界增长**(每 op 新 IoToken、槽不回收,~200B/请求;实测
8.8→153→301→594 MiB @ 0→0.7M→1.5M→3.0M 累计请求,三线性点)。这是
mio 侧 2d3e6ce 修过的同一缺陷在 uring 侧的漏网(台账 #45)。生产速率下
数小时即 OOM——**这是一级缺陷,与性能无关**。monoio 的 2.6MiB 正是
槽回收正确的对照。发现 A(park 空转)与此无关。

### eBPF 可用性结论(回答"能不能用 eBPF")

Cloud Shell 容器**没有 tracefs/debugfs**(`/sys/kernel/tracing` 为空),
bpftrace 与全部 `syscalls:sys_enter_*` perf 轨点不可用;perf 软件计数器
(task-clock/context-switches)可用(paranoid 降到 1)。**eBPF 路线需要
完整 VM 或裸金属**(GCE 常规实例、或未来 OrbStack 修复后)。本次 syscall
归因由 strace -cf 完成(计数可信、吞吐不可比)。

## OrbStack 健康复跑(2026-10-31):多 worker 塌陷是真实缺陷(台账 #46)

VM-hang 修复(macOS 27.2)后同矩阵重跑:环境确认健康(探针 µs 级,
monoio/compio/tokio 388-403k),但 **miniss 跨 worker 塌陷仍在**——c32
0.58x monoio,p50 2x、p99 4-5x;CPU/万请求 256.8ms vs monoio 56.6ms(4.5x)。
机制实测:busy_rounds/polled ≈17:1,每秒 park 仅 ~8k 次、每 worker 每秒
仅 91ms 在 park——**执行循环在自旋而非阻塞**,自旋地板烧掉每 worker ~1 核。
1 worker 时同循环摊销良好(发现 A 的空转正是其另一面)。结论:污染对照中
"miniss 塌陷"不是(或不只是)VM-hang,而是**多 worker 低单 worker 事件率
拓扑下的真实调度缺陷**(台账 #46),修复方向为自适应阻塞。下面污染-era 的
对照记录保留作为历史,但其"环境伪影"归因已被 #46 修正。

## OrbStack 污染环境对照(形状,2026-10-30;不入 bench/results)

同矩阵在 OrbStack 7.2.12(7 vCPU,已知跨核唤醒 7-300ms)重跑,全部 45 格
零错误零泄漏。**污染签名被完整隔离验证**:干净内核上 miniss 是 uring 家族
第一(c32 对 monoio +8.3~10.0%),同一份代码在污染 VM 上对 monoio 跌到
0.53-0.61x(ka8 0.53x)——7 worker 的跨核 spawn 路径正是被 7-300ms 虚拟
中断击中的路径(1 worker 的 cloud shell 从未触发)。glommio 同样下坠
(0.45-0.69x monoio)。monoio≈compio 的相对关系两环境一致;tokio 两环境
都是 c32 第一或并列。另一个形状发现:c32 p99 的 uring 家族尾巴
(3.7-5.6ms)在 7 真核上不复现(monoio/compio 179-258µs)——干净跑的
发现 A(park 超时空转)更像 1 核争抢下的产物,不是 uring 家族的内在缺陷;
这直接支持"自适应 park 超时"方向的收益预期。两环境差异 = 拓扑(1 vs 7
worker)+ 污染,双变量不可分离,故 orb 数据仍只作形状参考。

## 结论

1. 2026-07 矩阵的"高并发与 monoio 同量级"在干净内核、对齐 ring 后
   **加强为:1 worker 低连接区间 miniss 一致领先 monoio,与 tokio 接近**。
2. 两个新的量化优化目标:park 空转 enter(发现 A)、RSS 异常(发现 B)。
3. 旧矩阵的混杂变量(ring 1024 vs 4096、单轮)已消除;共享放置仍是硬限制,
   排名性结论仍需 ≥4 专核的 GCE 实例。

## hyper 旗舰路径基准(2026-10-31,同机):h1 keep-alive 可用,h2 与 close-churn 塌陷(台账级发现)

> 回答"runtime 集成了 hyper(tokio 生态)就是完整 HTTP 框架,为什么 benchmark
> 只走自写 framer"的问题:本节把 hyper 1.x(`hyper::server::conn::http1/http2`)
> 接到 miniss 上(`examples/http_hyper_echo.rs`、`examples/http_hyper_h2_echo.rs`,
> IO 层为 `AsyncTcpStream` 直接实现的 `hyper::rt::Read/Write`,`src/net.rs:750,794`;
> h2 背景任务走 `MinissHyperExecutor`,`src/hyper_rt.rs`)并跑同一方法论
> (3 轮 × 10s、warmup -t1 -c10 -d3s、`MINISS_URING_ENTRIES=1024`、--cores 1、
> wrk;h2 因 wrk 不支持 h2c 改用 h2load `-m1`,无 close32 对应模式)。
> 原始件:gcloud `~/hyper-bench-20261031-011411/`(h1)与
> `~/hyper-bench-20261031-011848/`(h2,CSV 解析被 VM 重启打断,以 h2load
> 原始 txt 为准);对照 photon 数据取 `~/photon-bench-20261030-161605/results.csv`。

### 吞吐(3 轮中位数,RPS)

| 服务器(同机同日) | h1 ka8 | h1 ka32 | h1 close32 | h2c ka8 (m1) | h2c ka32 (m1) | RSS |
|---|---|---|---|---|---|---|
| miniss 最小 framer(10-29 承诺矩阵) | 68,075 | 77,848 | 77,908 | — | — | 191 MiB(含 #45 缺陷) |
| miniss 最小 framer(10-31 复测,host 漂移校验) | — | 63,860 | 63,398 | — | — | — |
| **hyper on miniss**(本节新增) | **31,131**(p99 680µs) | **35,882**(p99 2.55ms) | **2,789**(p50 10.5ms) | **3,376**(mean 2.3ms) | **3,338**(mean 9.5ms) | h1 ~90 MiB / h2 ~35 MiB |
| photon io_uring(10-30 提交) | 53,179 | 52,968 | 52,913 | — | — | — |
| **hyper on tokio(对照,本节新增)** | **57,517** | **54,774** | **11,993** | 未测 | 未测 | — |

hyper-on-tokio 对照 = `bench/runtime_comparison/hyper_tokio_echo.rs`
(同 hyper http1 路径、同 22B payload、tokio 多线程 runtime `--cores 1`)。

### 发现 C(台账级):hyper 在 miniss 上 h2 与连接churn路径塌陷 ~4-20x,签名是串行化 ~300µs/请求

1. **h1 keep-alive 是健康的低效**:31-36k = 0.54-0.66x hyper-on-tokio、
   0.59-0.68x photon、0.44-0.56x 同日 framer。这是"hyper 每请求做更多事
   (含 date 头 ~35B、HPACK/帧解析)"与 miniss 唤醒路径损耗的叠加,
   量级符合"框架税"。
2. **h1 close-mode 与 h2c 全部压到 ~3.1-3.4k RPS 天花板**,与连接数无关
   (c8 与 c32 同顶格;h1 close c8 探针 avg 2.39ms、h2 c32 mean 9.5ms
   ≈ 32/(1/0.3ms)——单请求 ~300µs 的串行资源)。对照:
   - hyper-on-tokio close32 = 12.0k(hyper 自身的 churn 成本,5.3x 低于其 ka32);
   - miniss framer close32 = 63.4k(同日)。
   即 miniss×hyper 交互缺陷把 hyper 的 churn 成本再放大 ~4x
   (close-mode 0.26x、h2 无 tokio 对照但 3.4k 已低于任何合理框架值)。
   嫌疑面:hyper 每连接的 spawn/drop 生命周期(`spawn_local` 准入/回收)、
   readiness 唤醒跨任务传播、或 io_uring 多 op 序列的完成唤醒;
   framer 单任务长驻循环不触发。**本任务只记录不修复**;修复前,
   benchmark 故事里 hyper 路径的公平数字只有 h1 keep-alive 行。
3. **host 漂移**:同机 10-31 比 10-29 慢 ~18%(framer ka32 77.8k→63.9k、
   tokio-framer 79.4k→62-64k,tokio 同步下移证明是 host 而非 miniss 回退)。
   跨日比较一律用同日对照,本节已按此执行。
4. **h3 无负载工具**:cloud shell 无 h2load 之外的 h3 压测器(ngtcp2 h3load
   未打包),curl 无 --http3。h3 正确性由 `tests/quinn_h3.rs`(5 测试,
   macOS + orb io_uring 全绿)与 `examples/http_h3_echo.rs --selftest`
   (in-process h3 客户端,含 2/7 核自检)覆盖,不进吞吐矩阵。

### 例程的架构审计(AGENTS.md MUST #8)

三个 example 与一个对照 bin 的增量:无新增共享可变状态、无锁、无原子、
无 hot-path runtime 创建(启动时一个 `MultiCoreRuntime`);连接经
SO_REUSEPORT 每 worker 一监听(内核派发模型,MUST #7),accept 与连接
处理全部 `spawn_local` 留在本 shard(无跨核流量);h2 的 hyper 背景任务
经 `MinissHyperExecutor` 在 shard 上 `spawn_local`(hyper_rt 既有代码);
h3 的 quinn endpoint **必须在其 owning shard 上构造**——quinn 的 endpoint
driver 与 connection driver 都经 `MinissQuicRuntime::spawn` 落地,主线程
构造会让两者落到不同 shard 并触发
"QUIC socket owned by shard X, polled from shard Y"(`src/quic.rs`
ensure_owner 硬错误;example 已按 shard 构造并在注释中记录该约束)。
启动路径存在既定模式的阻塞调用(std UDP bind、rcgen 证书生成,setup 期,
与 `http_echo.rs` 的 bind-in-spawn 同型)。
