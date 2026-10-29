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

## 硬件前提

任何性能决策的测试环境必须是干净内核:
- **可用**:`gcloud cloud-shell ssh --authorize-session`(内核 6.18.55+,已验证
  跨核唤醒 38µs;但注意 Cloud Shell 实例仅 2 核,吞吐基准数字偏小);
- **禁止作为性能依据**:OrbStack VM(内核 7.2.12 的跨 vCPU 唤醒 7-300ms,
  见 `docs/review/runtime-safety-findings.md` #44 之后的 wake-probe 记录),
  只用于正确性验证。
