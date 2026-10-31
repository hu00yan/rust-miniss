# QUIC 服务生命周期层测试方法论(2026-10-31,v2)

> **目的:** 在写任何适配层代码**之前**定死测试方法。这是业主的显式指令
> ("先定好测试方法,把各种边界情况还有实际遇到的问题都确定好怎么测试,
> 然后再写适配层")。
>
> **v2(本版):** 业主否决了 v1 的两点——(1)**测试量**:~2850 行手维护
> 测试超过被测代码(~680 行),维护比不可接受("test maintenance cost ≈
> code cost");(2)**方法**:36 条人脑枚举边界用例不是可持续方法。v2 改为
> **conformance 继承为第一原则**(类比数据库借用 SQLite/SQLLogicTest:数千
> 条上游预维护用例)+ **性质化核心**(生成器 + 不变量 oracle + 强制收缩)+
> **冒烟集 ≤10 条**。madsim 判定(§1)维持不变。
>
> 方法:读码审计(本仓库 + 本地 quinn/h3 源码)+ 上游检索(quic-interop-runner、
> h3spec、h2spec、crates.io)。docs-only 提交,分支
> `feat/quic-service-lifecycle`,基线 `integration/2026-10-31`(ccaf11b)。
> 版本基准(Cargo.lock):quinn 0.11.11 / quinn-proto 0.11.15 / quinn-udp
> 0.5.14 / h3 0.0.8(dev)/ h3-quinn 0.0.10(dev);madsim 上游最新 **0.2.34**
> (crates.io API,2025-10-11;任务卡的"~0.15"不存在);h3spec v0.1.14;
> h2spec 2.6.0。
>
> 输入:`wt-quic-audit/docs/review/quic-resource-audit-2026-10.md`(A1-A5)、
> `wt-quic-plan/docs/review/quic-implementation-plan-2026-10.md`(步骤零、
> <100µs 通过线)、`wt-http-audit/docs/review/performance-baseline-2026-10-30-gcloud.md`
> 发现 C(:123-146)、`docs/review/defect-forensics-2026-10.md`(P1-P5)、
> `src/quic.rs`(796 行)、`tests/common/quiescent.rs`、`tests/shutdown_quiescence.rs`、
> `src/multicore.rs:840-855`(`CoreTerminationSnapshot`)、AGENTS.md gate 定义。

## 0. 两条硬约束(业主 v2 反馈 → 设计约束)

| # | 约束 | 判定标准 |
|---|---|---|
| C1 | **维护比**:"test maintenance cost ≈ code cost"。v1 的 2850 行手维护测试 vs 680 行服务层 = 4.2x,否决 | 我们维护的测试+基础设施总行数 ~1200-1500(≤2.2x,其中纯测试 ~890 ≈ 1.3x);conformance 用例由上游维护,我们 0 行 |
| C2 | **反人脑枚举**:36 条手工边界用例是枚举法,换需求就腐烂 | 每个行为以下列之一承接:上游 conformance 套件(零维护)/ 生成器+不变量(性质)/ ≤10 条冒烟(仅限生成器与套件都够不着的 API 错误契约)。v1 的 36 行全部有新去处(§4.4 追溯表,覆盖零丢失) |

为什么这不是妥协而是更强的防线:v1 的枚举式 T1 恰好重复了 forensics 对
gate 的批评——D22 暴露的不是"用例不够多"而是"结构没有守卫";人脑枚举
的用例在需求演化后静默失配(P1 漂移的测试版)。不变量 + 上游套件断言的是
**恒等式与契约**,不随用例数增长腐烂。

## 1. madsim 判定:自建 quinn-seam 模拟器(维持 v1 不变)

### 1.1 madsim 是什么(证据)

| 问题 | 证据 | 出处 |
|---|---|---|
| 确定性机制 | `RUSTFLAGS="--cfg madsim" cargo test`;madsim-tokio 门面:`#[cfg(not(madsim))] pub use tokio::*; #[cfg(madsim)] pub use self::sim::*;`——**在 tokio API 表面整体替换**,不是逐点拦截 | madsim-tokio/src/lib.rs(GitHub master) |
| 拦截范围 | 模拟:net、time、task::spawn、signal、rand(经 `[patch.crates-io]` 换 getrandom/quanta);**不模拟:fs、process、sync、`io`(含 AsyncFd)——注释原文 "not simulated API","TODO: simulate `fs`"** | 同上 |
| 网络模拟 | `NetSim`(spin::Mutex):in-memory endpoint,投递经 `time.add_timer(latency)`;fault 面 = `clog_node(_in/_out)` / `clog_link` / `reset_node` + `buggify_with_prob(0.1)` 把小延迟放大成 1-5s;**审查可见面没有 per-packet loss/reorder/dup 率旋钮,没有 MTU 旋钮** | madsim/src/sim/net/mod.rs(GitHub master) |
| RNG | `GlobalRng` 全局种子,Network 克隆共享,`gen_range` 按 op 顺序抽取 | 同上 |
| quinn 支持 | **没有。** workspace 成员(madsim、madsim-tokio、madsim-tonic、madsim-tonic-build、madsim-macros、madsim-etcd-client、madsim-rdkafka、madsim-aws-sdk-s3、tonic-example)无一实现 quinn Runtime;README 全文无 quinn | GitHub madsim-rs/madsim |
| 版本 | 0.2.34,2025-10-11 | crates.io API |

### 1.2 quinn 能不能跑在 madsim 上(证据)

1. **quinn 的 tokio 路径在 madsim 世界里断裂。** quinn 默认
   `TokioRuntime::wrap_udp_socket` 调 `tokio::net::UdpSocket::from_std(sock)`
   (quinn-0.11.11/src/runtime/tokio.rs:29-32)——需要真实 fd 的 readiness 桥。
   madsim 下 `tokio::net` = `madsim::net::UdpSocket`(包一个 in-memory
   `Endpoint`,madsim/src/sim/net/udp.rs),**没有真实 fd 进 sim 网络的桥**。
2. **唯一的适配路径 = 亲手写 madsim 版 quinn Runtime**——与自建 quinn-seam
   模拟器工作量同型,还要额外背 `--cfg madsim` 全局编译旗标。
3. **`--cfg madsim` 对本仓库是环境假象放大器(P5)。** miniss 带 tokio 兼容层
   (`src/compat.rs`、`src/hyper_rt.rs`、`tests/tokio_compat_tests.rs`):patch
   tokio→madsim-tokio 会让兼容测试测 shim 而非 tokio 本身(D39 型"测试永远
   测不到声称的对象");不 patch 则 madsim 对新测试收益为零。
4. **决定性缺货:madsim 无法模拟非 tokio 运行时。** miniss 的
   `MultiCoreRuntime` 启动真实 OS worker 线程、自己的时间轮(`SleepFuture`,
   `quic.rs:74-109`)、自己的 IO 后端(kqueue/epoll/io_uring)。被测对象
   (服务生命周期:spawn/drop、retirement 通道、排水)恰好全部住在 madsim
   够不着的那个世界里。

### 1.3 quinn 自己的确定性设施与先例(证据)

| 设施 | 内容 | 对本案的含义 |
|---|---|---|
| quinn-proto 0.11.15 `Pair` 测试架(tests/util.rs:27-41) | 两个 TestEndpoint + 模拟单向 `latency` + `mtu`(超限丢包)+ `congestion_experienced`(ECN)+ `deterministic_packet_numbers(true)`(util.rs:48-55)+ 手动驱动 `drive`/`drive_bounded`/`next_wakeup`(util.rs:122,130,450);tests/mod.rs **92 个 `#[test]`**(v1 按 fn 计数误写 98,修正) | proto 层确定性测试的上游蓝本,且**它本身是可继承资产(§2.1)** |
| quinn 0.11.11 自带测试(src/tests.rs) | 真实 127.0.0.1 UDP + tokio——非 in-memory | quinn 上游没有 in-memory 全栈测试;若需要由我们自建 |
| `Endpoint::new_with_abstract_socket(config, server_config, socket: Arc<dyn AsyncUdpSocket>, runtime: Arc<dyn Runtime>)`(quinn-0.11.11/src/endpoint.rs:133) | 接受任意 socket + runtime;`allow_mtud = !socket.may_fragment()`(endpoint.rs:139);`Runtime::now()` 文档原文 "Allows simulating the flow of time for testing"(runtime.rs:27-29) | **seam 就是官方 API**。SimUdpSocket 直接实现 AsyncUdpSocket(绕开 quinn-udp 0.5.14 的 `UdpSocketState`,后者只吃真实 std socket) |
| compio-quic(同族 thread-per-core 先例) | tests/basic.rs 用**真实回环 UDP**(`Endpoint::client("127.0.0.1:0")`);无确定性模拟 | 同族生态也没有 in-memory 全栈测试——这块本就空缺 |

### 1.4 判定与两方案对比

**判定:自建 quinn-seam 模拟器(v2 修剪到生成器消耗的最小集,~380 行,
§6)。** 改 madsim 的工作量与之同型却买不到关键东西。

| 维度 | 改造 madsim | 自建 quinn-seam sim |
|---|---|---|
| 前期代码 | ~300-450 行适配 + `--cfg madsim` 管道 + tokio shim 版本跟随 | ~380 行(修剪后),零新外部依赖 |
| QUIC 协议语义确定性 | 有(但要自己补 loss/reorder/dup/MTU——madsim net 面没有) | 有,旋钮按生成器需要设计(§6) |
| miniss executor/调度确定性 | **无,且不可达** | 同样**无**——两案共同边界,由性质化核心的 real-flavor + T3/T5 覆盖 |
| tokio 兼容层纯洁性 | 风险:cfg 旗标把 compat 测试换成测 shim(P5) | 无接触 |
| 与生产 seam 的关系 | 新造第三套 runtime 语义 | SimRuntime 与生产 `MinissQuicRuntime` 实现同一 quinn trait 契约(runtime.rs:16-31),trait 漂移在 sim 层即暴露 |
| 失败种子重放 | GlobalRng 有,但全局 cfg 混入无关确定性面 | seed + 收缩后的最小 schedule 完全自有(§4.3) |

**即使 madsim 完美适配,它仍然不覆盖:** ①miniss spawn 顺序/时间轮/retirement
通道的调度行为;②IO 后端矩阵 kqueue/epoll/io_uring 语义(D22 教训:后端必须
真的被编译运行);③T3 微基准——sim 时间是虚拟的,<100µs 通过线是墙钟主张;
④quiescence oracle 需要真实 fd 计数与真实 termination snapshot。

## 2. 第一原则:conformance 继承(新,v2 取代 v1 的 22 条手写 T1 场景)

### 2.1 我们继承什么、欠什么(证据)

**QUIC 传输(RFC 9000/9002)与 H3(RFC 9114/9204)的 conformance 是
quinn 的已验证资产,不是我们的负债。** 证据:

| 资产 | 规模(核实数) | 维护方 | 出处 |
|---|---|---|---|
| quinn-proto 0.11.15 自带确定性测试 | **92 个 `#[test]`**(Pair 架:MTU/ECN/latency/确定性 PN/version negotiate/retry/key update 等) | quinn 上游 | 本地 registry 源码 `grep -c '#\[test\]' tests/mod.rs` = 92;harness tests/util.rs:27-41 |
| quinn 在社区 interop 矩阵注册 | 18 个注册实现之一,role = **both**(镜像 `stammw/quinn-interop:latest`) | 上游 + quinn 维护者 | quic-interop-runner `implementations_quic.json`(master) |
| quic-interop-runner QUIC 用例 | **14 条**(versionnegotiation、handshake、transfer、chacha20、keyupdate、retry、resumption、zerortt、http3、multiconnect、v2、rebind-port、rebind-addr、connectionmigration) | 套件上游 | quic-interop-runner quic.md(master) |
| h3spec | **77 条**(v0.1.14 ChangeLog:"27 new test cases, for 77 total"),覆盖 RFC 9114、RFC 9204(QPACK)、RFC 9000/9002 传输错误 | 套件上游 | kazu-yamamoto/h3spec ChangeLog.md |
| h2spec | **146(HTTP/2,RFC 7540/9113)+ 48(HPACK,RFC 7541)≈ 194 条**(2.6.0) | 套件上游 | summerwind/h2spec(2.6.0;已知误报 issue #141,frame size 4.2) |

**因此我们的义务收窄为三条,且只有这三条:**
(a)**适配器不得违反 quinn 的假设**——`Runtime`/`AsyncUdpSocket`/`UdpPoller`
契约(quinn runtime.rs:16-85)与 `MinissQuicRuntime` 的实现(`quic.rs:44-72`);
(b)**管理面(A1-A5)行为正确**——这是性质化核心的领地(§4);
(c)**我们的 h3 服务在 miniss 上能通过上游套件**——即套件即测,不重写用例。
反过来说:凡 quinn-proto 92 测试与 interop/h3spec/h2spec 已覆盖的协议语义,
我们**不写一条**重复用例——写了反而制造第二份会漂移的真相(P1)。

### 2.2 quic-interop-runner:机制、配方、orb 注意项

**机制(核实):** 每个实现打包成 Docker 镜像,runner 经
`docker-compose.yml` + 伴生项目 quic-network-simulator 提供网络环境
(loss/reorder 类环境在 sim 侧,pcap 落 `sim/`);实现与 runner 全程只经
环境变量 + 挂载目录交互;endpoint 必须尊重 `TESTCASE` 环境变量,**不支持的
用例必须 exit 127**(因此上游加用例不破坏我们);IPv6 用例需要宿主
`sudo modprobe ip6table_filter`;Wireshark 4.5.0+ 仅用于 pcap 分诊。
调用:`python3 run.py -s <server> -c <client> -t <cases>`。

**配方(orb VM,周期性全栈检查,不进 per-commit gate):**

```bash
# 1. 一次性:端点镜像(我们唯一的维护物:~60 行 Dockerfile,非用例)
#    linux 构建 examples/http_h3_echo(计划步骤 1 cherry-pick),
#    entrypoint 尊重 TESTCASE/角色环境变量,不支持的 TESTCASE exit 127。
#    线上矩阵要求 linux/amd64;orb 本地跑 arm64 buildx 镜像即可。
# 2. 本地注册:runner checkout 里给 implementations_quic.json 加 "miniss" 条目
#    (~10 行 JSON,镜像名指向本地 build)——不发 PR 也能本地跑。
# 3. 运行(orb VM 内,docker 已由 OrbStack 提供):
python3 run.py -s miniss -c quic-go -t handshake,transfer,http3,zerortt,resumption,retry,keyupdate,multiconnect
# 4. 退出码 + logs/<server>_<client>/<testcase>/output.txt 为验收物。
```

**orb(OrbStack Linux VM)注意项(逐条可验证):**
1. Docker/compose:OrbStack 自带,满足前置;
2. `ip6table_filter`:OrbStack 定制内核可能不带该模块——采用前先在 orb VM
   验证 `sudo modprobe ip6table_filter`,缺失则 IPv6 相关用例标 skip;
3. 线上矩阵镜像为 linux/amd64,Apple Silicon orb 经 Rosetta 跑 amd64 慢且
   对超时敏感用例(multiconnect 的丢包握手)有 flaky 风险——**本地一律
   arm64 自建镜像**,不追线上跑;
4. network-simulator 容器需要 NET_ADMIN(tc/netem 在 netns 内)——OrbStack
   支持 `--privileged`,验证一次即可;
5. Wireshark 4.5+ 装在 macOS 宿主,只服务分诊,不参与判分。

**维护账:套件的 14 条用例上游维护,我们 0 行;我们维护 1 个 Dockerfile +
1 条本地 JSON 注册 ≈ 70 行(计入 §7 基础设施)。**

### 2.3 h3spec → `examples/http_h3_echo.rs`(~100 行 harness)

h3spec 是**单二进制客户端**:发畸形/边界 H3/QPACK/QUIC 帧,断言服务端以
正确错误码关连接。调用 `h3spec <host> <udp-port>`(如 `h3spec 127.0.0.1 4433`);
输出逐条 Ok/fail。已知约束:TLS 1.3 连接需信任服务端证书——自签证书时把
CA 装进系统信任(orb 测试 VM 镜像内 `update-ca-certificates`;h3spec 无
`-k` 类跳过项的用法无文档,按"装 CA"处理并在 harness 里固化)。

harness(~100 行,shell 或 Rust test 二选一,建议脚本进 `scripts/`):
启动 `http_h3_echo`(bounded readiness wait)→ 生成自签 CA/证书(复用
`tests/quinn_h3.rs:17-24` 的 rcgen 0.13 模式)→ 装 CA → 跑 h3spec 全量
77 条 → 断言 0 fail → 有界停止服务端。**77 条用例零维护;上游 v0.1.15+
新增用例自动纳入。**

### 2.4 h2spec → `examples/http_hyper_h2_echo.rs`(~50 行 harness)

h2spec 2.6.0:`h2spec -h 127.0.0.1 -p <port>`(h2c 明文即可,发现 C 的 h2
例程即 h2c);要求服务端对 GET/POST `/` 回 200 非空——echo 例程满足;
`-k` 跳过证书、`-t` 走 TLS。harness(~50 行):启动
`http_hyper_h2_echo`(该例程现存放于 wt-http-audit 工作树,随计划步骤 1
cherry-pick)→ 有界就绪 → 跑全量 ~194 条 → 断言退出码 → 清理。已知误报
issue #141(frame size 设 `(1<<24)-1`)按上游 issue 追踪,不在我们侧修。

## 3. 测试分层(修订版)

| 层 | 环境 | 覆盖 | gate 位置 | 维护行数 |
|---|---|---|---|---|
| **T1** conformance 继承 | quinn-proto 92 测试(可在本工作树直接 `cargo test -p quinn-proto --locked` 对钉住的 0.11.15 重放——零成本继承)+ h3spec/h2spec harness(§2.3/2.4)+ interop-runner 周期跑(§2.2) | 全部 RFC 协议语义 | quinn-proto:进 `just check` 附加步;h3spec/h2spec:进 check;interop:周期性 | **用例 0 行**;harness ~230 行(一次性) |
| **T2** miniss 真实回环 | 真实 UDP 127.0.0.1;bounded wait;后端矩阵 kqueue / Linux io_uring / Linux epoll(`MINISS_IO_BACKEND`);场景由性质化核心的 **real-flavor 生成器**产出(§4),不再手写场景清单 | 适配器 + 管理面在真实后端上的记账与生命周期 | `just check`;Linux 两后端独立 CI 腿(P2:每台主机恰编译一个后端是 D22 根因,forensics §4) | 计入性质核心 real-flavor |
| **T3** 每连接生命周期微基准 | `benches/quic_conn_lifecycle.rs`,显式 recipe,`--cores 1`;四量:spawn 基线、顺序 churn、并发 churn(c8/c32)、跨任务唤醒 | **步骤零 go/no-go**:并发 churn 每连接 < ~100µs 且不塌陷到发现 C 的 ~300µs 串行签名(performance-baseline-2026-10-30-gcloud.md:123-146) | 显式 bench recipe;**任何性能主张前必须有 T3 数据**(AGENTS.md RED;D44/D45) | ~250 |
| **T4** 性质化核心(v2 主投资) | sim-flavor:单线程 + 虚拟时钟 + SimNetwork,**无 socket/fd/thread/sleep/wall clock**(AGENTS.md test-fast 红线);real-flavor:真实 shard + 真实回环 | 生成器 + 不变量 oracle + 强制收缩(§4) | T4 sim-flavor 不进 test-fast(严禁任何 protocol-sim 进秒级内循环),进入 Tier 2 `just gate` 的 affected 选测,由 src/quic.rs 与 sim 脚手架路径触发;real-flavor 进 `just check` | ~500(含生成器) |
| **T5** gate 断言 | 嵌在 T2/T4 内 | 不变量在每次真实 gate 都跑:termination snapshot 全零 + fd 收支(mirror `park_efficiency_check` 型) | `just check` 每次跑 | 嵌入,0 独立行 |
| **S** 冒烟集 | ≤10 条(§5) | 生成器与套件都够不着的 API 错误契约 | `just check` | ~140 |

**不是 loom:** 生产同步 seam(retirement 通道)尚未抽取,AGENTS.md 规定
此时 `just loom` 必须明确失败,不得用独立 reference model 冒充生产并发证据
——T4 只对 shard-local 单线程策略与调度序列建模,本无数据竞争面。

## 4. 性质化核心(v2 主投资,~500 行含生成器)

### 4.1 生成器(proptest 风格,Schedule 是被 shrink 的值)

```rust
struct Schedule {                    // 全部维度,proptest 逐维 shrink
    connections: 0..=CAP+2,          // 含越界:cap 恰满/溢出
    use_retry: bool,                 // refuse vs stateless-retry 分歧(计划 §3 A1)
    streams_per_conn: 0..=200,
    payload: 0..=1 MiB,              // 含 0 长与 > MTU
    faults: SimFaultSchedule,        // 来自 §6 的旋钮:loss/reorder/dup/delay/partition/mtu
    ops: Vec<Op>,                    // churn 交错:accept|active|drop|idle-advance|shutdown(dup)
    drop_at: Option<DropPoint>,      // mid-handshake / mid-transfer / pre-accept / cross-shard
}
```

### 4.2 不变量 = oracle(v1 的 36 条枚举 → 9 条恒等式)

| ID | 不变量 | 断言形态 | 映射 |
|---|---|---|---|
| INV-1 | **资源收支归零**:任意 schedule 终止后 `ResourceBalanceSnapshot` balanced+empty、`assert_quiescent!` 全零(`tests/common/quiescent.rs:99-106`)、`handle.io_retirement_counts()==(0,0)`、fd 差 ≤ `FD_TOLERANCE`(quiescent.rs:19) | real-flavor 每 schedule 末尾强制;sim-flavor 断言虚拟记账表 | **P3**(D24/D25/D27/D8 类)+ #45/#46 live 不变量复用 |
| INV-2 | **cap 上界**:每个 accept 观测点采样 active ≤ cap;溢出 ⇒ 恰好 refuse(`incoming.rs:44`)/retry(`:52`),且计数差 == 生成的溢出数 | sim+real 采样不变量 | A1(P3) |
| INV-3 | **字节完整性**:生成器写出的每个字节被恰好一次观测到(逐流哈希) | sim+real | D11 类丢失/跨读错序 |
| INV-4 | **指标折叠**:sum(per-shard deltas) == aggregate == 生成器真值(mirror `tests/metrics_aggregation.rs:67,145-147`) | real-flavor | **P3** + P1(series 表漂移由既有静态断言同型守卫) |
| INV-5 | **retirement 通道守恒**:静止时 reserved==queued==0;在途 ≤ 预留容量(`quic.rs:141-153,171-174` 的准入预留不变量) | sim+real | **P3** |
| INV-6 | **有界活性**:任意 schedule 在有界 drain(`shutdown_with_timeout`,含重复 Shutdown 消息注入)内终止且快照全零 | real-flavor | **P4**(D14/D23/D41 类) |
| INV-7 | **内存上界**:不读对端场景,缓冲 ≤ 收紧后的 `receive_window` × 并发流 + endpoint 基数(8 MiB,`quic.rs:125`) | sim-flavor | A5 |
| DIFF-1 | **兄弟路径差分**:同一 Schedule 逐一跑 {socket 路径, poller 路径} × {kqueue, io_uring, epoll} 腿,全部不变量同判——v1 的 8 单元手抄矩阵收缩成 1 条差分性质 | real-flavor,多腿 | **P1**(D15/D12/D13:`quic.rs:279-349` vs `464-566` 的双胞胎;forensics 第 6 项"双后端差分架"的落地) |
| PROBE | **环境有效性**:每腿启动前证明后端真的消费了 completion(D22 签名探测器);T3 前验证空闲 CPU/无杂进程(D44) | 断言,非性质 | **P5**(D44/D45/D22) |

### 4.3 收缩与重放(强制契约)

proptest 对 `Schedule` 逐维 shrink,任何失败必须收缩到最小 schedule;回归用例
由 proptest 自动持久化,且 `MINISS_SIM_SEED=<n>` + prop seed 双种子可仅凭
数字重放(`cargo test quic_lifecycle_props` + 环境变量,失败信息打印
`seed=… step=… pkt=… event=…`)。sim-flavor 的投递序列由唯一 `StdRng` 按
(step, packet_seq) 顺序抽取——同 seed 同测试 = 字节级相同世界。确定性契约:
单线程、时间唯一来源 `SimRuntime::now()`、harness 不暴露真实时钟 API。

### 4.4 v1 → v2 追溯表(覆盖零丢失的机械证明)

| v1 行(36) | v2 去处 |
|---|---|
| A1.1 cap N/N+1 | INV-2(生成器 connections 含 CAP/CAP+1/CAP+2) |
| A1.2 refuse vs retry | quinn-proto 上游 retry 测试 + interop `retry` 用例 + 生成器 `use_retry` 维度(INV-2 判分歧点) |
| A1.3 槽位泄漏(对端握手中途 close) | INV-1 + 生成器 `drop_at=mid-handshake` |
| A1.4 跨 shard drop during drain | INV-1 + INV-6(real-flavor `drop_at=cross-shard`) |
| A1.5 idle 超时回收槽位 | 生成器 `idle-advance` op + INV-2 复检 |
| A1.6 0-RTT 与 refuse | interop `zerortt` + quinn 上游 + INV-2(槽计数) |
| A1.7 accept 回调 panic | S3(冒烟,错误契约) |
| A1.8 多 shard 各自限额 | INV-4(real-flavor 逐 shard)+ INV-2 |
| A2.1-A2.6 stats 桥六条 | INV-4 + 既有 series 静态断言同型(A2.6 非运行时用例)+ `metrics_aggregation.rs:225` 的采样残留由 INV-6 覆盖 |
| A3.1 有界排水 + 强拆 | INV-6 |
| A3.2 驱动丢失显式化 | S2(冒烟,D28/#33) |
| A3.3 endpoint guard 跨 shard Drop | S6(冒烟,复用 `quic.rs:577` 满 inbox 构造) |
| A3.4 wait_idle 唤醒竞态 | 生成器 `shutdown`/`drop` 交错 + INV-6 |
| A3.5 重复 shutdown 消息 | 生成器注入重复消息 + INV-6(D27/#34) |
| A3.6 驱动死亡检测 | S2 合并 |
| A4.1 h3 echo 自检 | S7(保留)+ h3spec/interop 为更强覆盖 |
| A4.2 per-shard 构造约束 | S1(冒烟,`quic.rs:228-247,677-743` 错误契约) |
| A4.3 examples 编译面 | 既有 `just check`,无新增 |
| A5.1 receive_window 收紧 | INV-7(sim)+ S8(real RSS 探针) |
| A5.2 socket buffer 可配置 | S5(冒烟) |
| A5.3 MTU 黑洞/DPLPMTUD | quinn-proto 上游(Pair.mtu,util.rs:34-36)+ 生成器 `faults.mtu` 维度 + intero `transfer` |
| A5.4 anti-amplification | quinn-proto 上游传输层测试 + 生成器 loss 环境复验 |
| A5.5 内存公式文档 | 文档 gate(常量单点化),非测试 |
| 8 单元 P1 差分矩阵 | DIFF-1(1 条差分性质 × 3 后端腿) |

## 5. 冒烟集(≤10 条,只收生成器与套件够不着的契约)

| ID | 内容 | 不可替代的原因 |
|---|---|---|
| S1 | 外来 shard ensure_owner 错误信息契约(`quic.rs:228-247`;现测 `quic.rs:677-743` 拆分) | 错误字符串是 API 契约,生成器不产出"错误文本" |
| S2 | 驱动 spawn 准入失败有界可见(D28/台账 #33;`quic.rs:52-62`) | 需要注入准入失败这一非自然调度态 |
| S3 | accept 回调 panic 后槽位仍释放(RAII 契约)+ cap=0 直接 API 检查 | panic 路径生成器不遍历 |
| S4 | macOS kqueue 1ms poller 重试收敛(`quic.rs:474-510`,busy ≤1000/s/endpoint) | cfg(macos) 专属,Linux CI 腿永远够不着 |
| S5 | socket buffer 配置化读回(getsockopt == 配置值,`quic.rs:125,182-197`) | 单点配置契约 |
| S6 | endpoint guard 跨 shard Drop 走退役通道(满 inbox 压力构造,复用 `quic.rs:577`) | 该构造本身是精密手工活(压满 inbox 的阻塞任务),不宜进生成器 |
| S7 | `http_h3_echo --selftest` 2/7 核自检(A4.1) | 例程自含检查,h3spec 不覆盖 in-process 自检 |
| S8 | receive_window 真实 RSS 探针(收紧档案下非读对端内存有界) | sim 断言虚拟账,真实 RSS 只能实测(与 INV-7 互补) |
| S9 | 后端功能探针:epoll/io_uring/kqueue 腿启动前完成一次 completion 消费 | D22 疫苗,属环境断言(P5/PROBE 的机械载体) |
| S10 | `MINISS_QUIC_CONNECTIONS` 环境语义 → A1 API 平价(`tests/quinn_h3.rs:25-31` 升级) | 迁移期双形态兼容契约 |

**硬顶 10 条**:新增冒烟必须先证明"生成器维度/上游用例覆盖不到",否则按
RED 处理(防止冒烟集在维护中重新长回枚举法)。

## 6. 故障注入配置(v2 修剪:只保留生成器消耗的旋钮)

```rust
pub struct SimFaultSchedule {          // 只服务 §4.1 生成器,不再追求通用模拟器
    pub seed: u64,                     // 唯一 RNG:StdRng::seed_from_u64
    pub loss: f64,                     // 每方向每包
    pub reorder: Option<Duration>,     // 投递时刻 + U(0,jitter)
    pub dup: f64,
    pub delay: (Duration, Duration),   // (base, jitter)
    pub partition: Vec<(usize, usize)>,// (at_step, 方向对):迁移/rebind/排水
    pub mtu: Option<usize>,            // >mtu 丢弃;先例 Pair.mtu(quinn-proto util.rs:34-36)
}
```

- v1 移除:`buggify`、`ecn`(生成器当前维度用不到;Pair 先例已记录,需要时
  加回是 +20 行)。madsim 对应能力(clog/reset/buggify)本就不含 loss/reorder/
  dup/MTU(§1.1),比较结论不变。
- 行数:v1 估 ~670±150 → **~380**(SimNetwork 路由+RNG ~180、SimUdpSocket
  ~90、SimRuntime/SimTimer ~90、schedule 抽取 ~20;配置/重放并入生成器代码)。

## 7. 规模预算(v2,满足 C1)

**上游维护(conformance 用例,我们 0 行):**

| 套件 | 用例数(核实) | 我们的维护行数 |
|---|---|---|
| quinn-proto 0.11.15 内嵌测试 | 92 | 0(`cargo test -p quinn-proto --locked` 直接重放) |
| quic-interop-runner | 14 | 0(端点打包 Dockerfile+JSON 注册 ~70 行计我们侧基础设施) |
| h3spec 0.1.14 | 77 | 0 |
| h2spec 2.6.0 | 194(146+48) | 0 |
| **合计** | **~377 条** | **0** |

**我们维护的行数:**

| 项 | 行数 | 备注 |
|---|---|---|
| sim 脚手架(§6 修剪) | ~380 | 共享基础设施,性质核心与未来管线复用 |
| 性质化核心(生成器 + 9 条 oracle,sim+real 两 flavor) | ~500 | 主投资;v1 的 T2 场景清单(~800)与 T1 枚举(~1200)并入其中 |
| conformance harness(h3spec 100 + h2spec 50 + interop 打包/配方 80) | ~230 | 一次性;套件升级自动受益 |
| 冒烟集 ≤10 | ~140 | 硬顶 |
| T3 微基准 | ~250 | 不变(业主 E 项) |
| **合计** | **~1500(区间 1200-1500)** | |
| 对照:服务层(A1-A5) | ~680 | QuicAcceptor 250 / stats 桥 150 / shutdown 桥 200 / 配置档案 80 |

**维护比:纯测试 890 行 ≈ 1.3x 服务层;加共享基础设施 2.2x——落在业主
"test maintenance cost ≈ code cost" 的量级内(v1 为 4.2x,否决项已消除);
同时 ~377 条上游用例 + quinn 上游 CI 以 0 维护成本入场。**

## 8. 执行顺序(v2)

| 步 | 内容 | 前置门 |
|---|---|---|
| 0 | 例程 cherry-pick(计划步骤 1:`http_h3_echo`、`http_hyper_h2_echo`)——conformance 套件的靶子 | 纯增例程,零 src/ 改动 |
| 1 | **性质核心 sim-flavor + sim 脚手架 + 冒烟 S1/S2/S3/S9 先行**,gate 第一个适配层提交(A1 QuicAcceptor) | 新测试以普通 cargo test 目标落盘(不改 justfile/gate 接线);T4 sim-flavor 不进 test-fast,由 Tier 2 `just gate` 按 src/quic.rs 与 sim 脚手架路径 affected 选测;diff-audit allowed_files;arch-audit |
| 2 | `cargo test -p quinn-proto --locked` + h3spec/h2spec harness 进 `just check`(对 cherry-pick 后的例程) | 92 + 77 + 194 条继承资产对钉住版本生效 |
| 3 | real-flavor 性质 + 后端三腿 + 冒烟 S4-S8、S10,gate A2 stats 桥 / A3 shutdown 桥 | epoll 腿复活(D22 疫苗)先行 |
| 4 | **T3 微基准先于任何性能主张**(<100µs 通过线 / ~300µs 冻结线) | bench recipe 显式 + PROBE 环境校验(D44/D45) |
| 5 | interop-runner 在 orb VM 周期跑(周级,arm64 自建镜像) | §2.2 注意项逐条验证(ip6table_filter、privileged) |

## 9. 开放问题(记录,不阻塞)

1. ~~T4 sim-flavor 进 test-fast 需改 justfile:310-316 的显式清单~~ 已裁决
   (owner verdict,gate-hierarchy-2026-10.md §2.4):T4 sim-flavor 不进
   test-fast(严禁任何 protocol-sim 进秒级内循环),进入 Tier 2 `just gate`
   的 affected 选测,由 src/quic.rs 与 sim 脚手架路径触发;不改 justfile。
2. h3spec 的 TLS 信任路径(无 `-k` 用法文档)需在 orb 测试 VM 镜像里固化
   CA 安装;若后续版本提供跳过项则简化。
3. interop 端点镜像的 `TESTCASE` 协议实现(exit 127 语义)是本方案里唯一
   的"上游接口面"工作,~60 行 Dockerfile + entrypoint;若上游 json 接受
   miniss 注册(可选 PR),本地补丁即撤。
4. macOS poller 重试路径的 busy 计数探针(S4)可能需一行 cfg(test) 钩子;
   如需登记 ARCHITECTURE_EXCEPTIONS 在 A1 提交时一并办理。
5. sim 的 TLS 握手用 rcgen 0.13 + rustls-ring:quinn-proto 用
   `deterministic_packet_numbers` + 固定 CID 解决可重放面;T4 对 TLS 层的
   主张仅限"握手成败/0-RTT 接受与否",不主张字节级重放(记录边界)。
