# io_uring 特性开关的决策框架

> 2026-10-29。回答一个问题:**一个 io_uring 特性,是查资料就确定打开,还是必须轻量测试后再打开?**
> 结论先行:**资料定先验与排序,同机 A/B 定决策——两步都要,且已有一次反例证明只靠资料会翻车。**

## 为什么不能只靠资料

`DEFER_TASKRUN` 在文献里是"推荐模式"(arXiv 2512.04859 明确这么说),但我们的实测:
- OrbStack 内核 7.2.12:开启后吞吐 40 万 → 2.6-2.8 万(≈-94%),伴随超时;
- 另一台内核:`No suitable IO backend available`,setup 直接失败。

同一个 flag,资料说好,两台机器两种死法。原因:taskrun 语义依赖内核调度行为,
不同内核/虚拟化下表现完全不同。**任何 setup flag 的默认值都必须来自目标运行
环境的 A/B,资料只决定"值得花测试预算"的排序。**

## 两类特性,两条流程

### A 类:setup flag(不用写代码,只改 ring 创建参数)

流程:资料筛选 → `just bench-uring-flags` 同机配对 A/B(机制计数器齐全)→
默认值 + 失败回退。半天的测试量。

### B 类:需要实现代码的特性(multishot recv / buf_ring / fixed file / send_zc)

资料决定"做不做、先做谁"(收益先验 + 实现复杂度),实现时必须带 `MINISS_URING_*`
布尔开关(默认关),然后走 A 类的同一套 A/B 决定"是否进默认"。

## 现有证据与当前决策(2026-10)

| 特性 | 类型 | 资料先验 | 我们的实测 | 当前决策 |
|---|---|---|---|---|
| `COOP_TASKRUN` | A | — | +12.0% RPS / p99 6.70→5.24ms(2026-07);生产默认 A/B 配对中位 +3.34% | **默认开**,失败自动回退 |
| `SINGLE_ISSUER` | A | — | 比 COOP 差(+8.3% vs +12.0%) | 关;仅作 DEFER_TASKRUN 的前置再评估 |
| `DEFER_TASKRUN` | A | arXiv:推荐模式 | OrbStack 上 -94%;另一内核 setup 失败 | **开关保留,默认关**;在干净内核(gcloud 6.18+)重测后重定 |
| `SQPOLL` | A | arXiv:单线程存储 +32%,但 ~30µs 唤醒延迟 | OrbStack 800 连接 p99 优于 baseline,尾延迟仍不及 monoio/compio | **开关保留,不是拒绝**——见下方重新定性 |
| multishot recv | B | arXiv:小消息下最高效 | 未实现 | **下一个实现目标**,对标 compio `sys/op/multishot/iour.rs` |
| provided buf_ring | B | 注册类对存储 +11%(固定 buffer 单独 4-6%) | 未实现 | multishot recv 之后;对标 compio `sys/buffer_pool/iour.rs`(5.19+ 硬要求) |
| fixed file | B | 单独 4-6% | 未实现 | 低优先;若做,buf_ring 一起(共享注册路径) |
| `SEND_ZC` | B | 小消息(<1KiB)下比普通 send 更差 | 未实现 | **搁置**;22 字节 echo 负载明确吃亏 |

## SQPOLL 重新定性(2026-10-29,用户决定)

此前记录把 SQPOLL 当作"拒绝"是不准确的。正确定性是:**已实现、有布尔开关
(`MINISS_URING_SQPOLL`)、未在目标类硬件上验证**。关键约束是内核收割线程容量:
每个开启 SQPOLL 的 ring 占一个专属内核线程,10 核机器开 10 个 ring 就要用掉
全部核的一半预算,继续开会性能暴跌——这是"用一部分换另一部分"的交换,不是
纯收益。解决路径(未实现):`IORING_SETUP_ATTACH_WQ`(内核 5.6+)可让多个
ring 共享一个 SQPOLL 线程池,大核数机器上若启用 SQPOLL 应连它一起评估。

## B 类特性的内核源码定论(2026-10-29,torvalds/master io_uring/net.c + kbuf.c)

不依赖跑分,内核实现直接给出三个确定结论:

1. **multishot recv 与 provided buffer ring 是一个原子特性,不可拆分。**
   `io_recvmsg_prep()` 对 `IORING_RECV_MULTISHOT` 强制要求
   `REQ_F_BUFFER_SELECT`,否则 prep 阶段直接 `-EINVAL`——没有 buf ring,
   multishot recv 无法存在。实现顺序从"先 recv 后 buf_ring"改为
   **单分支原子交付**(`feat/multishot-recv-buf-ring`)。
2. **机制收益确定:每请求对 op 数 2 → ~1。** 单发 recv 每次完整走
   submit → io_recv → CQE;multishot 在 `io_recv()` 内 `retry_multishot`
   循环:每轮从 buffer group 重新选 buffer、`io_req_post_cqe()` 直接
   post CQE,不重走 SQE 提交(arm 一次收 N 次)。echo 场景每请求对
   原本 1 recv + 1 send 两个 op,recv 并入长驻 arm 后 ≈1.x 个。
   端到端 RPS/p99 增益取决于 op 开销占比,这一层必须真机 A/B——但
   eBPF 计数(io_uring_submit_sqe tracepoint 按 opcode 计数)可以先于
   任何性能数字验证"op 减半"确实发生。
3. **buf ring 选择成本 O(1) 零分配**:`io_ring_buffer_select()` 是掩码
   索引弹出 + acquire 读生产者尾,无内核分配(legacy ProvideBuffers 是
   链表 + 每 buffer kmalloc);`IOU_PBUF_RING_INC`(6.14+)支持部分消费,
   小请求不再整块占用 4KiB buffer——正对 22 字节 echo 负载的内存放大问题。
4. **术语修正**:此前文档把两类注册混称"registered buffer"。我们的 recv
   路径需要的是 **PBUF_RING(provided buffer ring,5.19+)**;而
   `IORING_REGISTER_BUFFERS`(REGISTERED 固定数据 buffer,READ_FIXED/
   WRITE_FIXED,5.1+)是另一个东西,arXiv 的"+11% 存储"是后者的数据,
   与 recv 路径无关,引用时要分开。recvzc 存在但强制 multishot 且依赖
   网卡 zcrx 基础设施(`io_zcrx_ifq`),不适用,维持搁置。

## setup flag 的内核级冲突矩阵(2026-10-29 起由代码强制)

"全打开"在 io_uring 是不可能的——部分 flag 互斥,内核直接拒绝整个 ring。
`src/io/uring.rs` 的 `resolve_taskrun_flags()` 现在持有这张表,非法组合自动
落到最近合法子集并 warn,而不是起不来或静默错配:

| 组合 | 内核语义 | 运行时处理 |
|---|---|---|
| COOP_TASKRUN + DEFER_TASKRUN | 互斥(两种 taskrun 模式) | DEFER 胜出,自动去 COOP |
| DEFER_TASKRUN 无 SINGLE_ISSUER | EINVAL(硬性前置) | 自动补 SINGLE_ISSUER |
| SQPOLL + DEFER_TASKRUN | 内核拒绝 | 忽略 DEFER,warn,SQPOLL 照跑 |
| 其余组合 | 自由组合 | 一开关一实验 |

`just bench-uring-flags` 的默认 flagset 已包含 `defer_pair`(SINGLE_ISSUER+
DEFER_TASKRUN)这一合法组合;gcloud 干净内核上的重测结果将决定 DEFER 是否
进默认(OrbStack 数据不作数,见下节)。

## Photon 案例:生产级 C++ 代码库的默认层级(2026-10-31,源码核实 HEAD 5641e27)

PhotonLibOS(字节跳动,io_uring 里程最长的生产代码库之一)的默认集回答了
"该开什么"的问题——它的层级不是对 io_uring 的热情,而是**对用户的一道纪律**:

- **io_uring 本身双重门控**(构建 flag + 显式 init),**epoll 仍是默认引擎**;
- **自动开启的只有两类**:内核版本门控、省 syscall、语义不可见——
  COOP_TASKRUN(≥5.19 自动)与 fixed-file 注册(≥5.5 自动,稀疏表 10000);
- **一切改变时序或线程行为的都是显式 opt-in**:SQPOLL(flag,SQ_AFF 被硬禁,
  COOP 冲突见其 issue #803)、IOPOLL、send_zc(独立 socket flavor);
- **DEFER_TASKRUN 从不请求**——代码里只作为 EINVAL 时剥离的对象出现,
  与我们的 -94% 实测互证;
- **registered buffers 与 buffer ring 完全缺席**;multishot 只用于不可能
  有害的 poll/eventfd;issue #784 维护者理由:psync recv + uring poll
  已达 memcpy 速度,"completion 路径上加任何东西都是先付成本后谈收益";
- **EINVAL 回退阶梯 + 版本探针处处皆是**(pre-5.15 wait bug、COOP×SQPOLL
  冲突 el8 6.4.12 等,每个都有代码注释引用内核提交)。

**竞品生态事实**:除 monoio/glommio/compio(io_uring-first)外,主流生态
默认不用 io_uring——Go 2019 年起拒绝(golang/go#31908),asio 实验性 opt-in,
netty 4.2 毕业但 epoll 仍是默认,libuv 因 100% CPU bug 默认关闭文件 io_uring
(libuv#4044),Python 核心缺席。"全打开"的生产先例不存在。

**对 miniss 的映射:现有姿态与 Photon 层级几乎逐条吻合**(COOP 默认开 ✓、
SQPOLL/DEFER opt-in ✓、send_zc 搁置 ✓、回退阶梯 ✓、multishot recv+buf_ring
默认关 ✓)。唯一的新增候选:**fixed-file 注册**(Photon 自动开,我们未实现)
——收益中等、实现中等,列为低优先候选;不改变现有默认集。

## 生态共识:八个运行时的默认集(2026-10-31,第二半调研,含 kimojio 源码核实)

 Photon 之外,补齐 kimojio(Azure,2025-11,架构上与 miniss 最近的兄弟)/
glommio / tokio-uring / ntex-neon(已归档)/ libxev 的源码级核实:

- **共识默认集惊人保守**:全部八个运行时的默认 = 纯 opcode + 零或近零
  setup flag + 阻塞 submit_and_wait。7/8 默认无 SQPOLL、无 DEFER_TASKRUN;
  6/8 无 COOP_TASKRUN;5/8 无 registered buffers;**没有任何运行时把
  buf ring / multishot recv / SEND_ZC 作为默认**。
- **"全打开"不存在,而且结构性不可能**:COOP/DEFER 互斥,DEFER 要求
  SINGLE_ISSUER 且与 SQPOLL 冲突,内核对 SQPOLL ring 清除 SINGLE_ISSUER。
  实测上限 = 一种 taskrun 模式 **或** SQPOLL **或** IOPOLL,三者不叠加。
- **内核风险史支持保守默认**:2023-2026 的每一波稳定树修复——DEFER 停顿
  (6.7.x/6.8.x + liburing #1645 无界 taskwork 积压)、multishot recv 的
  buffer 回收回归(6.1.y 至 6.13)、PBUF_RING 生命周期/ENOBUFS、SEND_ZC
  通知/生命周期(6.9-6.12 滴灌式修复)、SQPOLL parking/sharing/teardown——
  **恰好落在生产运行时全部 opt-in 或缺席的那部分特性集里**;零 flag 纯
  opcode 默认集几乎没有回归史。
- **分歧由负载解释而非口味**:glommio 与 kimojio 都常备一条 IOPOLL 副环,
  因为它们是存储导向(可假设 NVMe poll 队列)——纯网络运行时无人这么做。
  kimojio 的用户态 BusyPoll(默认 **Never**,即阻塞)与 libxev 的拒绝
  IORING_TIMEOUT_UPDATE(压低内核地板)是同一权衡的两个方向:可预测延迟。
  libxev 是全调研中最明确"为压内核地板而拒绝特性"的案例。
- **Apache Iggy**(2026-02 生产迁移博客)选 compio 用纯 opcode +
  IOSQE_IO_LINK、零 flag,并称 registered buffers 对普通人是"够不着的天花板"
  ——又一例生产 thread-per-core 的保守姿态;glommio 被其评为
  "unmaintained, too opinionated"(最后发版 2024-03,无官方声明)。

**对 miniss 默认集的最终定论**:现有姿态(COOP 默认开 + 其余全 opt-in)
已获八运行时共识背书——COOP 比其中 6/8 激进,但有自有实测(+12%/+3.34%)
且语义不可见,保留。multishot recv + buf_ring 维持默认关(共识如此,且
内核风险史里 multishot recv 回归贯穿 6.1.y→6.13,在目标内核 A/B 前不进
默认)。#46 修复方向获得 kimojio 佐证:其 BusyPoll 默认 Never = 默认阻塞,
忙轮询是显式选项——与本运行时的自适应阻塞方案同构。

## 硬件前提

任何性能决策的测试环境必须是干净内核:
- **可用**:`gcloud cloud-shell ssh --authorize-session`(内核 6.18.55+,已验证
  跨核唤醒 38µs;但注意 Cloud Shell 实例仅 2 核,吞吐基准数字偏小);
- **禁止作为性能依据**:OrbStack VM(内核 7.2.12 的跨 vCPU 唤醒 7-300ms,
  见 `docs/review/runtime-safety-findings.md` #44 之后的 wake-probe 记录),
  只用于正确性验证。
