# 控制面 metrics sampler 对数据面的干扰:机制解读 + orb 7 核测量(2026-10-31)

> 回答 owner 的问题:"那一条 sampler 线程会不会从 8 个 worker 嘴里抢 CPU、
> 扰动数据面任务,能否避免?monoio 的 one-runtime-per-thread 根本没有这条
> 线程。" 本文分两半:**机制**按 91e3496 的代码逐行核对(只读,不改 src/),
> **测量**在 OrbStack VM(7 vCPU,`Cpus_allowed_list 0-6`)上跑 7 worker
> framer(`http_echo`)ka32(`-t2 -c32`),3 轮 × 10s 顺序轮换 interleaved、
> 每格前 warmup `-t1 -c10 -d3s`、`MINISS_URING_ENTRIES=1024`、co-located 不
> 绑核(与 bench_http.sh/基线一致)、全程零 socket/HTTP 错误、零进程泄漏、
> 服务器有界退出(SIGTERM→2s→KILL 兜底)。per-thread CPU/上下文切换取自
> `/proc/<pid>/task/*/stat|status` 测量窗前后差分。原始件在仓库外
> `wt-metrics-probe-tools/results/metrics-probe-results/`,脚本
> `wt-metrics-probe-tools/metrics_probe_bench.sh`(benchmark 编排工具,不在
> crate 内)。3 个配置:A = bench 默认(`MINISS_METRICS_INTERVAL_MS=1000`,
> 聚合线程 + example 日志线程各 1Hz)、B = `MINISS_METRICS_INTERVAL_MS=1`
> (两线程都 1000Hz,knob 允许的最坏档)、C = env 不设(只有无条件启动的
> 聚合线程,1Hz)。

## 一、机制(代码证据,file:line)

### (a) 稳态线程清单:不是 8+1,是 N+1(+example 可选 1)

- worker:`MultiCoreRuntime::new_with_sampler` 每 CPU plan 起一条
  `miniss-core-N`(`src/multicore.rs:1896-1968`),Linux 上
  `sched_setaffinity` 钉死到选定 CPU(`src/multicore.rs:1188-1215`,在
  `run()` 开头生效 `:946`)。orb 上 affinity 0-6 → **7 worker**。
- 聚合线程:同一次 `new` 里无条件 `thread::Builder::spawn`,名字
  `miniss-metrics`,**普通 OS 线程、无 executor/reactor/任务队列**,跑
  `Aggregator::run`(`src/multicore.rs:1998-2030, 1827-1835`;
  `src/metrics.rs:696-725`),不绑核。
- example 日志线程:`examples/http_echo.rs:235-258` 在
  `MINISS_METRICS_INTERVAL_MS` 设了的时候再起一条同名 `miniss-metrics`,
  每间隔调一次 `metrics_snapshot(1s)` 直通 fanout(不走聚合线程缓存)。
- main:起完 accept loop 后 `block_on(pending::<()>())` 永久停泊
  (`examples/http_echo.rs:323`),实测 CPU 0.00%。
- 内核侧:SQPOLL 是显式 opt-in(`MINISS_URING_SQPOLL`,
  `src/io/uring.rs:746-759`),默认关,无内核 SQ 线程;稳态进程内
  /proc 实测恰 10 条线程(A)/ 9 条(C),与上表一一对应,无意外线程。

**结论:bench 默认配置 = 7 worker + 2 sampler + 1 停泊 main = 10 条用户
线程对 7 个 vCPU;owner 机器 8 worker + 1 sampler + 1 main = 10 条对
8 核。**

### (b) 计数器如何到达 sampler:消息,不是直接读,也不是共享槽

周期时钟(默认 1s,`src/config.rs:64-78`)到期后
`MetricsFanout::sample`(`src/metrics.rs:580-612`):

1. 在与 spawn 共享的 admission 计数器上拿一张票(共享
   `Arc<AtomicUsize>` 的 CAS,`src/multicore.rs:189-210`);
2. 对每 shard 在**同一条 4096 容量 crossbeam 有界 inbox** 上
   `try_send(CoreMessage::CollectMetrics{reply})`,满了就放弃本轮
   (`src/metrics.rs:584-591`;容量 `src/config.rs:12`);
3. 每 shard 一个唤醒:`threads[core].unpark()` + `CoreWakeup::notify()`
   (`src/metrics.rs:614-617`);
4. worker 在消息 drain 点处理(`src/multicore.rs:1325-1328`):把
   shard-local 的 `CoreMetrics`(**普通 u64 字段,无原子**,
   `src/multicore.rs:623-656`)按值复制成 `Copy` 的
   `CoreMetricsSnapshot`(`:797-834`,快照 34 个字段),经**每次 pass
   新建的** `std::sync::mpsc` 回发;
5. sampler 收满 7 份(250ms 总 deadline,`src/metrics.rs:32`)、排序、
   merge 进 `MetricsView`,view 是聚合线程的**线程本地普通变量**
   (`src/metrics.rs:634-640`)。

writer = sampler(inbox、gate、eventfd),reader = sampler(reply);
worker 只在收到消息时才被要求做一次复制。scrape 默认不打扰 worker:
`MetricsCommand::View` 只回缓存(`src/metrics.rs:706-708`,
`src/multicore.rs:2776-2779`);`Refresh` 才强制 pass(`:709-713`)。
但**周期时钟驱动的 pass 不看有没有人 scrape**——没人看也每秒全量
fan-out 一次。DECISION_LOG 2026-10-15 行(§ aggregator owned by the
runtime)与 2026-10-18 行(provider-neutral metrics integration)记录的
就是这套设计。

### (c) 路径上的锁/智能指针:数据面零锁,控制面两条 mpsc

- 采样路径上没有任何 `Mutex`/`RwLock`/`Rc`。同步原语只有:
  crossbeam 有界队列(lock-free)、admission `Arc<AtomicUsize>`(与
  spawn 路径共享,是全进程采样路径上唯一可能被数据面写到的原子——
  仅在 spawn 同时发生时竞争同一 cache line)、sleep-gate
  `AtomicU8`(`src/multicore.rs:100-146`)、每次 pass 一个
  `std::sync::mpsc` reply(每 scrape/每 pass 新建,不属于任何 worker)。
- `parking_lot::Mutex` 只保护 join handles(`src/multicore.rs:1664`),
  采样路径不经过。
- `Arc<MetricsFanout>`/`Arc<CoreWakeup>` 启动后只读;Drop 先停聚合线程
  再 join worker、关 eventfd,顺序有注释约束
  (`src/multicore.rs:3025-3065`)。

### (d) sampler 会不会唤醒 worker / 在 worker 上跑代码

**会——这正是它唯一的跨线程动作,也恰好是"委托 + 唤醒"本身**:

- 每 pass 向每 shard 发 1 条消息(委托)并给 2 个唤醒信号:
  `unpark()` + `CoreWakeup::notify()`。后者只有 worker 已置
  `SLEEPING` 才写 eventfd(`WorkerSleepGate::notify` 返回
  `current & SLEEPING != 0`,`src/multicore.rs:127-141, 248-253`);
  worker 在跑就只做一次 gate CAS 置 `WAKE_REQUESTED`,worker 下次
  `begin_sleep` 时自己吸收掉(`:107-125`),eventfd 都不写。
- sampler 从不直接读 worker 内存、从不在 worker 线程上执行代码;
  worker 执行的只有被委托的 `CollectMetrics` 处理(一次 ~300B 按值
  复制 + 一次 mpsc send)。
- **例外即答案**:它不能"跑"worker 的代码,但它能让 worker **在
  sampler 的时钟上**多跑一段——被 park 中打断(独占轮空一轮事件循环
  + 唤醒税),或繁忙时占用一个 message-drain 档位(繁忙时 inbox 轮询
  间隔最多 15 tick,`src/multicore.rs:1009-1026`)。这是 PULL 形态的
  固有代价:回答的成本由 worker 付,时机由控制面定。

### (e) 可配置性:有频率、无开关

`MINISS_METRICS_INTERVAL_MS` 启动时读一次(默认 1000;`0`/非法值回落
1000,即**最小 1ms、无法关闭**,`src/config.rs:71-78`)。聚合线程
无条件随 `MultiCoreRuntime::new` 启动(`src/metrics.rs:3-5` 的模块注释
明说),没有"不采样"的配置;example 日志线程随同一 env 出现/消失。
所以"完全关掉"不可测,本文用 C(单线程 1Hz)作下界、B(1kHz)作
上界夹逼。

## 二、对照 owner 原则的判定

原则:"跨线程流量只应该是任务委托 + 唤醒;控制面指标应该由数据面
active-push,控制面侧用便宜的 mutex/Rc"。

- **形态上合规**:现在的机制确实只有"消息委托 + 唤醒",没有直接读
  worker 的原子、没有共享可变快照槽、没有锁过界;owner 担心的
  "sampler 轮询 worker 发布的原子"并不存在(那是 2026-10-15 拒掉的
  备选之一)。
- **方向上不合规**:流量是控制面定时 PULL,不是数据面 loop-boundary
  PUSH。三个越界点:(1) pass 由 sampler 自己的钟驱动,与有没有读者
  无关,worker 被迫陪跑;(2) 回答成本(复制 + 回发)落在 worker,
  时机不受 worker 控制;(3) 每 pass 每 shard 最多 3 个跨核信号
  (inbox CAS、gate CAS、eventfd/unpark)。sampler 不绑核,它偷的
  CPU 由调度器随缘分配。

## 三、测量(orb 7 vCPU,7 worker,ka32,framer)

### 表 1:吞吐与延迟(3 轮 interleaved,中位数;每轮 10s)

| 配置 | r1 | r2 | r3 | **中位 rps** | p50 中位 | p99 中位 |
|---|---:|---:|---:|---:|---:|---:|
| A 默认(2 线程 × 1Hz) | 284,042 | 248,393 | 260,614 | **260,614** | 74µs | 1.71ms |
| B 1ms(2 线程 × 1kHz) | 290,086 | 257,311 | 260,670 | **260,670** | 74µs | 1.90ms |
| C 单聚合线程 1Hz | 295,837 | 259,147 | 250,761 | **259,147** | 74µs | 1.95ms |

- 三配置 rps 中位极差 **0.6%**;轮间环境漂移(284k→248k,±9%)比配置
  间差大一个数量级。同轮配对差:最大 +4.3%(r2 C vs A),方向不固定。
- p99 中位 B/C 比 A 高 11%/14%,但无剂量响应——C(比 A 还少一条线程、
  同为 1Hz)反而最差,轮内 p99 抖动(1.26-3.02ms)与 2026-10-15 基线
  的结论一致("host 无法在这个粒度分辨 p99 差异")。不能归因于采样。
- 对比 ±40% 仪器噪声底线(`docs/review/gate-hierarchy-2026-10.md:46`,
  引 DECISION_LOG 2026-10-17):所有配置差在噪声带内 **1-2 个数量级**。

### 表 2:per-thread CPU(/proc stat 差分,10s 窗,占单核 %)

| 配置 | worker 合计(7 条) | sampler 合计 | main | 进程合计 | worker 非自愿切换/10s |
|---|---:|---:|---:|---:|---:|
| A | 221.7 / 223.8 / 219.7 | **0.00**(两线程均 0 tick) | 0.00 | ≈222 | 880 / 6,081 / 4,343 |
| B | 229.3 / 219.3 / 223.3 | **7.5 / 8.7 / 8.9**(每条 2.9-5.7) | 0.00 | 232-238 | 16,509 / 24,607 / 20,939 |
| C | 221.7 / 219.3 / 217.4 | 0.00(单线程 0 tick) | 0.00 | ≈219 | 1,434 / 6,837 / 7,106 |

- **默认 cadence:sampler CPU 低于测量分辨率**(0 tick = 每线程每 10s
  <10ms,<0.1% 核;两线程合计 <0.2%)。
- **worker CPU 份额不因 sampler 活动下降**:B 的 sampler 吃掉 7.5-8.9%
  核的同时,worker 份额没有系统性下滑(中位 221.7 → 223.3),被偷的
  是闲置余量/wrk 份额,不是 7 个钉死 worker 的预算——7 worker 只用了
  7 核的 ~32%。
- **极限 cadence 的信号在调度器里,不在吞吐里**:B 下 worker 非自愿
  切换 3-12× 同轮 A(每 worker 每 s 约 2000 条 CollectMetrics 的唤醒/
  抢占痕迹),但 ka32 吞吐与 p50 纹丝不动。sampler 线程自身 vcs:
  A/C ≈ 34-50 次/10s(每 pass 一次阻塞 recv + 计时器),B ≈ 2.8-3.0 万
  次/10s。
- worker 侧单条消息成本:低于 /proc tick 分辨率;上界推断——B 下
  worker vcs 总量未超出 A/C 波动带,说明繁忙段消息到达时 gate CAS
  吸收了唤醒(eventfd 大多没写,与 (d) 的代码判断一致)。

### 每个 pass 的成本与理论界

B 档反推:每轮 ~2000 pass(2 线程 × 1kHz × 10s),sampler 合计 CPU
750-890ms/轮 → **每 pass(7 shard 全量 fan-out + 收集 + merge)
0.37-0.45ms 控制面 CPU(两线程合计)**,即每 shard 摊 ~55-64µs。折算
默认 1Hz(A,2 条线程 × 1 pass/s):≈0.8ms/s ≈ **0.08% 核**——与 0 tick
实测相容。worker 侧即使按 B 的 2000 msg/s/worker 也无可见 CPU 增量
(≈ ≲1µs/条)。**默认配置的理论最坏界:采样系统总 CPU <0.1% 核,
worker 每秒被多打断 2 次(聚合 + 日志线程各 1 次)。** owner 机器
8 worker 外推:每 pass 控制面成本再 +1/7 shard(≈+14% 的 0.08%),
每 worker 的打断频率不变——oversubscription 从 10 线程/7 核变成
10 线程/8 核,只更宽松。

**"完全关闭"没有 knob,未测**(按任务约束不加开关);C 是最近似的
下界,A−C 的吞吐差 +0.6% 即"第二条线程全量存在"的上界信号。

## 四、判定与可选项

**判定:默认配置下干扰低于测量分辨率,更远低于 ±40% 噪声带,ka32 上
不构成材料问题;真实成本是结构性的(worker 在控制面时钟上陪跑),
不是数量级的。** 若要兑现 owner 的 PUSH 原则,按性价比排序:

1. **scrape-only / 去 periodic(最小改动,先做这个)**:删掉
   `Aggregator::run` 的 `RecvTimeoutError::Timeout` 分支里的周期
   sample(`src/metrics.rs:718-721`),`View` 无样本时回
   `NotSampled` 或触发一次有界 `Refresh`(后者已存在,`:709-713`)。
   控制面流量从"每秒固定 2 pass"变"每 scrape 一 pass"(Prometheus
   典型 15-60s 一拉,流量反而下降 15-60×),worker 只在有人看时被打扰,
   **零新增数据面工作**。代价:scrape 多一个 pass 往返(≤250ms 有界);
   需要重新表述 `View` 的首次语义(DECISION_LOG 2026-10-15 行的
   推翻条件本来就预留了"scrape 延迟"这条)。
2. **loop-boundary push 进单写者槽(结构性解,树内有先例)**:worker
   每 N 个事件循环 turn 把 `CoreMetricsSnapshot` 副本发布进自己的
   `CachePadded` 单写者槽(先例:`publish_ready_hint` +
   `core_ready_hints`,`src/multicore.rs:1052-1056, 1669-1671`,同一
   形状已过审计),sampler/scraper 只读,零消息、零唤醒、零 eventfd。
   代价:(i) 每 turn ~300B 复制是数据面工作(虽小,按 MUST #6 需要
   注释与审计);(ii) Copy 结构跨字段撕裂需要 seqlock/双缓冲或容忍
   (监控语义可容忍);(iii) `max_*` 高水位与 gauge 变成 turn 边界值;
   (iv) sampler 读原子 = 跨核 cache line 读,但方向是"worker 已推过、
   sampler 共享读",不再有 dirty-line 迁移到 worker。
   与 1 组合 = "1-in-N 自发布 + scrape 时读",worker 端 N 可取
   每 turn 一次到每 64 turn 一次。
3. **绑核**:8 核 8 worker 无空核,pinning 只决定 sampler 从**哪一个**
   worker 偷,把随机干扰变成对单个 worker 的确定性税;orb 上 10 线程
   对 7 核同理。不单独解决问题,可与 1/2 叠加用于隔离(比如钉到
   main 停泊所在的核)。
4. **合并 cadence**:把默认 1s 提到 scrape 量级,与 1 等价但保守
   (保留低频心跳,live/ready 判断仍有新鲜样本)。
5. **monoio 极简端**:monoio 无内建 sampler/聚合线程,指标全靠应用层
   自理——那是"没有功能"的零成本,不是"有功能的零成本"。miniss 的
   运行时级指标是产品差异(docs/METRICS.md、README 矩阵),不必拆;
   走 1(或 1+2)后,控制面在数据面眼里退化成"只在被读时才出现的
   一条消息",已满足"委托 + 唤醒"的字面与精神。

## 五、测量 vs 外推(诚实边界)

- **实测**:orb VM(7.2.12-orbstack,aarch64,7 vCPU)7 worker、ka32、
  framer、3 配置 × 3 轮 × 10s interleaved;per-thread CPU/ctxt 差分;
  线程清单;A−C 与 B−A 配对差。
- **未测**:"完全关闭采样"(无 knob,按约束不加);owner 的 8 物理
  核 + 8 worker;非 ka32 场景(ka8/close800 等空闲占比更高的形状下,
  唤醒打断 park 的相对权重会更大,本文数字不外推到那些形状);
  ftrace/perf 级的 per-唤醒观测;worker 单条消息的精确成本
  (低于 /proc tick 分辨率,只有上界)。
- **外推**:默认 cadence 的 0.08% 核 = B 档实测 per-pass 成本 ÷ 1000
  的线性折算;8 worker 的 +1/7 shard 线性外推;VM vCPU 非物理核、
  当日宿主负载 3.75-8.56,绝对数只作同场自比,不作与历史表的横比
  (与 photon-benchmark-2026-10-31 的场地纪律相同)。

## 六、架构审计(AGENTS.md MUST #8)

本分支相对 91e3496 仅新增本文档;**src/、tests/、benches/ 零变更**,
无新增共享状态、锁、原子、runtime 创建、阻塞调用或跨核流量。测量在
仓库外脚本中进行,服务器/客户端有界退出,复检 `pgrep`/`ss` 无残留
进程、无监听残留。唯一的运行期阻塞(进程 spawn、kill/wait、/proc 读取)
全部在编排脚本里,不在被测进程内。
