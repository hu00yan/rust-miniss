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

### 发现 B:RSS 差距 ~70x(待查)

191MiB(monoio 2.6MiB)远超"每分片 8-9MiB"的估算。候选:--all-features
构建的协议栈静态段、quinn/rustls 结构、Buffer 池峰值、或 TaskArena 之外
另有未计入的预分配。需要一次独立的 footprint 归因(排除法:逐特性构建)。
注意 c32 p99 收敛(4.7ms)与内存占用之间没有已知因果——不要把两件事
混为一谈。

### eBPF 可用性结论(回答"能不能用 eBPF")

Cloud Shell 容器**没有 tracefs/debugfs**(`/sys/kernel/tracing` 为空),
bpftrace 与全部 `syscalls:sys_enter_*` perf 轨点不可用;perf 软件计数器
(task-clock/context-switches)可用(paranoid 降到 1)。**eBPF 路线需要
完整 VM 或裸金属**(GCE 常规实例、或未来 OrbStack 修复后)。本次 syscall
归因由 strace -cf 完成(计数可信、吞吐不可比)。

## 结论

1. 2026-07 矩阵的"高并发与 monoio 同量级"在干净内核、对齐 ring 后
   **加强为:1 worker 低连接区间 miniss 一致领先 monoio,与 tokio 接近**。
2. 两个新的量化优化目标:park 空转 enter(发现 A)、RSS 异常(发现 B)。
3. 旧矩阵的混杂变量(ring 1024 vs 4096、单轮)已消除;共享放置仍是硬限制,
   排名性结论仍需 ≥4 专核的 GCE 实例。
