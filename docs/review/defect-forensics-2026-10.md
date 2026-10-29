# Defect-History Forensics: rust-miniss(2026-10-30)

> 方法:全量 235 条 commit message 逐条阅读;缺陷台账 44 条;`FAILED_ATTEMPTS.md`
> (仅模板,零真实条目——RED 路径从未被走过,这本身是数据点);DECISION_LOG 27 行;
> validation-pipeline-gaps;任务卡;justfile / audit_architecture.sh / ci.yml;
> 对 lease/retirement 机制做定点源码核实。91/235 提交(39%)带 fix 前缀。
>
> 出处修正:多处文档把 epoll-drain 修复引为 `e77bd70`,该哈希在当前历史不存在
> (rebase 伪影);现哈希为 `f5304fb`。

## 1. 缺陷清单(有书面证据者)

"应拦下的 gate"指当时存在的 gate 或其直接前身。

| ID | 简名 | 类别 | 引入 | 发现 | 应拦未拦的原因 |
|----|------|------|------|------|----------------|
| D1 | io_uring 读 use-after-free | resource-lifecycle | `ea6df73`(2025-08-25)前 | 手工验证 | 当时无任何 gate(justfile 时代始于 2026-06-27 `dcb610f`) |
| D2 | 取消测试挂死,根因"难寻",测试被 `#[ignore]` | cancellation | 同前 | 挂死 | 无;且是流程失败——ignored 测试后来被禁(`9841466`) |
| D3 | shutdown 内存序(Relaxed→Acquire) | concurrency | `9c0fa3a`(2025-08-30)前 | 审计 | 无 loom/内存序 gate(当时和现在) |
| D4 | UDP io_uring send 改同步 send | design-workaround | `4e446cf`(2025-09-01) | 现场兼容失败 | 有意从缺陷路径撤退 |
| D5 | tick 循环阻塞 recv 致定时器饿死 | scheduling | `bec8766`(2026-06-11)前 | 现场 | test-regular 跑 executor 测试但不带负载 |
| D6 | 不支持的 io_uring op 走 `todo!()` panic | mechanics | 同前 | 现场 | 无后端穷尽性测试 |
| D7 | `spawn()` 每任务一个 OS 线程 | design | `dd8e4d7` 前 | 现场 panic | 本应是编译期/类型错误——无 API 守卫 |
| D8 | `IntoRawFd` 泄漏 IO token | resource-lifecycle | 同前 | 清理审计 | 无 token 收支不变量 |
| D9 | SignalHandler 回调从不触发 | mechanics | 同前 | 清理审计 | 集成缺口 |
| D10 | kqueue 后端被删,macOS 别名到 epoll 实现;3 个缺口隐藏(park 自旋 100% CPU;ReadRent/WriteRent Unsupported;inline fsync 阻塞 shard,违反 MUST-NOT-#7) | coverage/sibling-drift | `e43e54b`(2026-06-11),暴露于 `ba98de6`(2026-10-15) | review batch | CI 曾**故意**删掉 macOS+epoll 腿(`e1fea75`,2025-08-10);macOS gate 停跑 14 个月 |
| D11 | HTTP 管道请求跨读丢失 | parser | `e778ceb`(2026-07-13)前 | 现场 | parser 性质测试后来才加(`9e76372`,台账 #20) |
| D12/D13 | owner-completion 路由 + kqueue PollAdd 投递漂移 | sibling-drift | `976ba0e`/`050445e`(2026-07-13)前 | 协议集成(手工,台账 #11) | 无差分后端测试;证据是手工的 |
| D14 | 12 小时孤儿测试进程(`671fd7d`,台账 #12) | test-lifecycle | 同前 | 进程猎捕 | panic-unwind 阻塞 shutdown;测试清理无 bounded-join |
| D15 | io_uring 终态清理在 fused 与 Vec drain 双路径重复且**已漂移**(SAFETY 注释丢失、模式绑定不同) | sibling-drift | `e8b7fbd` 前;台账 **#5** 预言 | 重构评审 | 无测试同时穿越两条 drain 路径(`e8b7fbd` 自己承认) |
| D16 | 重构 `e8b7fbd` 让 Linux **整体编不过** | coverage | `e8b7fbd`(2026-10-15) | 下个会话(`7ec8942`) | src/io/uring.rs 是 cfg(linux+io_uring);macOS 的 just check/clippy 从不编译它 |
| D17-D19 | D10 的 macOS 缺口 | coverage/sibling | `e43e54b` | `ba98de6` | 同 D10 |
| D20 | JoinHandle 藏着 `Arc<parking_lot::Mutex>` 逃过 arch-audit | gate-mechanics | `ba98de6` 前 | `ba98de6` 扩大扫描 | 模式表匹配 `Arc<Mutex` 但没有 parking_lot 拼写 |
| D21 | arch-audit 扫 `benches/` 而基准在 `bench/`——整条基准路径未被审计 | gate-mechanics | `ba98de6` 前 | `ba98de6` | — |
| **D22** | **Linux+epoll 从不消费 completion**:`process_io_completions` 按 `target_os="linux"` 选 fused 路径,但只有 io_uring 实现 fused;trait 默认什么也不消费 → epoll 全套挂死 4-5/6 次;一个 worker 自旋 **19h21m / ~18 CPU 小时**(台账 **#41、#42**) | coverage(+潜在 mechanics) | 自 process_io_completions 写下即潜伏;2026-10-16 引爆 | `f5304fb`,任务卡 epoll-pending-op-not-drained | 2026-10-16 前没有任何主机编译过 Linux+epoll:CI 删了 epoll(`e1fea75`),build.rs 在 6.12 下 panic 使 `io_backend="epoll"` 在 Linux 上是死代码(`d3904d3`:"活分支从未被编译器看过") |
| D23 | 非 owner 线程 Drop 静默跳过取消(epoll 把 closed fd 移出兴趣列表 → op 永不 resolve → shutdown join 挂) | design(所有权) | `f5304fb` 前 | `f5304fb` | 无 gate 在 Linux/epoll 上跑过跨线程 drop(与 D22 同一覆盖洞);模型里根本没有跨线程取消传输 |
| **D24** | 被遗弃的 readiness 注册永久持有 fd lease;`close(2)` 永不发;writev 护栏挂 33-60%(台账 **#43**) | design(所有权)+时序 | 设计有意为之:`4e25b39`(rm-07,2026-07-11) | Arc::strong_count 探针;`b0a31f2` | 测试 flaky(33-60%),在 just check 路径里反复重跑变绿——"每个 lease 都释放"没有确定性 oracle;"lease 只由 terminal CQE 释放"规则无强制 |
| **D25** | parked-shard 关闭死锁:`pending_operation_count` 把 shard 自己的 parking poll 计入;cancel 只是入队 AsyncCancel;slot 要等 CQE 才走;循环每轮再 arm 新 poll → 不可满足的退出谓词,99.5% CPU 自旋(台账 **#40**) | design(记账模型) | `arm_wakeup_poll` + drain 退出谓词,`6ffc0fc` 前 | 两次 live gdb 捕获;`6ffc0fc` | 间歇性(修复前 2/20),默认路径里但时序依赖;无活体 quiescence oracle(台账 #7 明说 snapshot 是"post-join 证据,非活体 oracle") |
| D26 | Mio 生命周期四缺口:(a) 定位文件 I/O 共享 OFD offset;(b) file job 活过 shutdown 记账;(c) mio token/slot 向量无界增长;(d) UDP `Poll::register` 失败留下 pending 状态 | mixed | `2d3e6ce` 前 | 主动评审(`678a207`/`2d3e6ce`,任务卡 mio-lifecycle-correctness) | 主动评审发现——没有 gate 覆盖;validation-gaps 文档逐条记为覆盖缺口 |
| D27 | 重复 `CoreMessage::Shutdown` 残留 inbox(台账 **#34**) | accounting | `9e76372` 前 | termination-snapshot 矩阵 | snapshot 本身(`9e76372`)是发现者——oracle 晚于缺陷 |
| D28 | QUIC adapter 在 admission 关闭后调用 → 1024 连接 `expect` abort(台账 **#33**) | mechanics + test env | `9e76372` 时代压力测试前 | 压力运行 | 压力测试在默认 gate 外;adapter 契约无测试 |
| D29 | 指标:scrape 饿死周期时钟;`CollectMetrics` 残留终止中的 inbox(无守卫 6/6 失败) | accounting | `e36af16` | 自身测试 | admission-ticket 契约对新消息类型无机械强制 |
| D30 | Task-key ABA;generation 回绕 → ~4.3e9 次 spawn 后**永久静默 spawn 饥饿**(`de8f3f8` ABA + `14ff7d0`) | design(identity) | `de8f3f8` 前 | 客户端复现 + 评审 | 边界条件任何现实测试都够不着;只有 fail-closed 设计能关死 |
| D31/D32 | 被取消的 accept 留下缓存 token;迟到的成功 accept CQE 留下描述符;completion connect 发起两次 | mechanics | `de8f3f8`(PR #3)前 | PR 自带回归 | 正是台账 **#5** 的预言("single-close handling can drift")在 accept 路径应验 |
| D33 | 流写 SIGPIPE | mechanics | `de8f3f8` 前 | PR 回归 | 平台默认信号此前无测试 |
| D34 | SlotMeta 别名:共享+可变引用重叠 | memory-safety | `de8f3f8` 前 | PR 评审 | Miri 集(8 个具名测试)不碰这段代码 |
| D35 | Mio 重新注册失败吞错、泄漏状态 | mechanics | `678a207` 前 | 评审 | 故障注入 seam 只存在于写过的测试处 |
| D36 | io_uring `Send` 打管道 → `ENOTSOCK` | mechanics | `de8f3f8`(其修复自身的 bug) | 后续回归 | SIGPIPE 修复(D33)选 opcode 时没分类描述符 |
| D37 | HTTP header 重扫 O(n²)(8MiB 请求 ≈8GiB 扫描) | perf | `de8f3f8` | `a242a2f` | 性质模型管正确性,不管复杂度 |
| D38 | 每次提交一次 `SO_TYPE` getsockopt | perf | `de8f3f8` 余波 | `419d9fd` | 同 D37 |
| D39 | PR #3 的 macOS SIGPIPE 测试在任何 macOS 上**永远过不了**(peer drop 后 EINVAL)——PR 自己的注记承认 macOS 检查未跑(台账 **#44**) | test-mechanics + coverage | `de8f3f8` | `f3347d8` | 合并纪律:PR 带着明确未跑的 fmt/clippy/macOS/stress 被合并 |
| D40 | taskrun flag 组合出非法内核组合 → 启动失败 | mechanics | `76adbfb` 前 | 现场("No suitable IO backend available") | setup flag 无约束矩阵测试(2026-10-30 已由 `resolve_taskrun_flags` 关死) |
| D41-D43 | 测试构造竞态:`dropping_owner_socket_drains_pending_receive`(#41)、escaped-waker 定长 tick 断言(#21)、QUIC 64 连接过早关客户端(#14) | test-lifecycle | 各处 | 台账调查 | flaky 测试重跑到绿为止;无构造评审 gate |
| D44 | 泄漏 worker 占核时测的基线:记录 165,764 rps,真实 289,730(~43% 误差)(`d3904d3`,台账 **#42** 的后果) | environment-falsehood | 2026-10-16 窗口 | 空闲 VM 重测 | bench-smoke 无环境有效性检查(空闲 CPU、无杂进程) |
| D45 | bench-smoke 在目标主机上是 ±40% 的仪器却强制 ±5% 的主张(`d3904d3`) | gate-calibration | 设计即如此 | 实测 | 单 5 秒样本对 5% 阈值 |

## 2. 根因模式挖掘

| 模式 | 数量(现场缺陷) | 成员 | 结构性? |
|------|------|------|---------|
| **P1 兄弟路径漂移** | **8** | D15(fused/Vec,台账 #5)、D12、D13、D18(macOS rent ops)、D31/D32(accept 终态路径)、`fb18d0c`(mio UDP `data.clone()` vs io_uring move——"同一协议按后端行为不同")、D22 的 OS-vs-backend 派发谓词 | **结构性**。迄今的缓解都是事后合并(`e8b7fbd` 把双 drain 合进 `reconstruct_completion`;`66bd34d` 五个 readiness wait 合进 `poll_readiness`;`b4ba8de` 删死 op)。但每个新 op 都重新加每后端 arm,合并是被动的不是预防的 |
| **P2 未编译/未测回退** | **6** | D22(+D16、D17-D19、`d3904d3` 的从未编译分支:`src/cpu.rs:57,74`、`src/multicore.rs:1752`、`tests/fallback_buffer_init.rs:7`)、D39 | **结构性,且当前已回退。** 后端选择是编译期 cfg;每台主机/gate 恰好编译一个后端。Linux-epoll CI 任务在 `678a207` 加入,**同日**被 `520b89d` 删除;当前 ci.yml(macOS kqueue、loom、miri、Linux io_uring)没有 epoll 腿 |
| **P3 记账/所有权不对称** | **8** | D24(#43)、D25(#40)、D26b、D31(缓存 token)、D27(#34)、D29、D8、rm-12 的 `890b819` | **结构性——这就是"为什么 bug 不断"的答案。** 每个资源的获取路径与释放路径在不同函数、常在不同文件(`DescriptorLease` 只在 CQE drain 路径释放:src/io/uring.rs 426/513/1005/1668;token 在 poll_readiness 清;fd 由 Arc drop 关;pending_ops 在 drain 退休)。没有任何类型把它们绑在一起。迄今每个修复都是在失败现场**手工配对**(retire_abandoned_registrations、retire_wakeup_poll+reap 窗口、CoreExitGuard)——每个新资源或新交互都重新推导一遍配对 |
| **P4 取消/遗弃竞态** | **5**(+2 主动) | D23、D24(迟到 CQE 面)、D32、D28、D41 | **半结构性。** D24/D23 是 P3 的遗弃面。注意:没有一个数据竞争——Loom(建模竞态)不可能抓到 #40/#41/#43;它们是逻辑/所有权 bug |
| **P5 测试环境假象** | **8** | D44、D45、D41-D43、D14(#12)、#39,外加两次**蓄意**收窄 `e1fea75`(2025-08-10 删 macOS+epoll)与 `520b89d` | **结构性。** 19h 自旋(#42)悄悄作废了窗口内全部 Linux 时序证据——包括 gate 随后对比的那条基线(D44)。污染环境不只是藏缺陷,它**制造了假回归信号** |

流程横切发现:(i) PR #3(`de8f3f8`)带着明确未跑的 fmt/clippy/macOS/stress 被合并——其 fallout 单独产生四个后续提交(`a242a2f`、`419d9fd`、`f3347d8`、`69ba4fd`);(ii) D20/D21 说明审计脚本自己也要被审计;(iii) FAILED_ATTEMPTS.md 只有模板(TBD),Decide 流程的 RED 路径从未被走过。

**"为什么这类问题在 epoll 支持加入后出现"的直接答案**(时间线推断,证据强支持):epoll 支持 2025-08 **宣布**、2026-06 为 macOS **别名**(`e43e54b`),但直到 2026-10-16 才**第一次在 Linux 目标上被构建运行**——因为 CI 删了它(`e1fea75`)、build.rs 在 6.12 下 panic(`d3904d3` 记录)。缺陷不是 epoll 加入后积累的——它们是在它不可构建期间埋下的,并在它首次编译的一天内全部引爆(D22/D23 发现于 2026-10-16,`ba98de6` 重命名后端次日)。而生命周期**机制**(completion 作用域的 `DescriptorLease`、pending_ops 记账)2026-07-11 为 **io_uring only** 设计(`4e25b39` rm-07:"fallback 后端留在旧的正确性优先路径"),之后被类比扩展——第二个后端最终跑起来时,遇到的是一套它不共享释放规则的机制。

## 3. 生命周期类:设计失误 / 机制失误 / 覆盖失误

| 缺陷 | 判定 | 依据 |
|------|------|------|
| D22(epoll 不消费 completion) | **覆盖失误**(带潜在机制触发——`target_os` 派发谓词错了,但只在无 gate 编译的 cfg 组合里承重) | `d3904d3`;`f5304fb`;任务卡根因(2) |
| D23(跨线程 drop 跳过取消) | **设计失误**——所有权模型没有跨线程取消传输;修复加了新模型元素(`CoreHandle` + `CoreMessage::CancelIoToken`),不只是代码 | `f5304fb`,任务卡根因(1) |
| D24(遗弃注册 lease,#43) | **设计失误**——"lease 只由 terminal CQE 释放"规则(2026-07-11/2026-10-17 两行决策,考虑并否决了 per-arm dup、提交作用域 lease、drop 内同步 cancel)允许存在**零所有者**的 armed 注册。修复(`b0a31f2`)补的是驱动退出路径;残余——永不完成的被遗弃注册仍持有 lease 直到 runtime drop——记录在提交里。洞是缓解不是关闭;决策日志自己的推翻条件("真实负载反复遗弃永不完成的注册")处于激活状态 | `b0a31f2`,台账 #43,DECISION_LOG 2026-10-17 |
| D25(parking poll 死锁,#40) | **设计失误(记账)**——`pending_operation_count` 混淆"内核欠我们 CQE"与"数据面工作未完成";shard 自己的控制 poll 被算进它自己正在 arm 的退出谓词。修复改的是关闭协议(drain 中不再 re-arm;同一轮 retire),即模型层 | `6ffc0fc`,台账 #40 |
| D26a/b(file offset、file job vs shutdown) | **设计失误**(池语义无 per-op 位置;在途池 job 不在 pending-work 模型里) | `2d3e6ce`,validation-gaps 表 |
| D26c/d、D31/D32、D34、D35 | **机制失误**——所有权模型正确,终态路径代码错。正被合并(`e8b7fbd`、`66bd34d`、`b4ba8de`)与终态 CQE 纪律(`de8f3f8`)永久关闭 | 如引用 |

结论:epoll 后的生命周期缺陷中,**3 个设计失误(D23/D24/D25,加 D26a/b)、4-5 个机制失误、1 个纯覆盖失误(D22)**。这决定预防手段:覆盖失误需要后端矩阵被编译运行(便宜、机械);机制失误需要路径合并 + 终态回归(进行中);设计失误需要**改所有权模型本身**——在失败现场打点修复正是这类问题反复重入的原因。台账 **#5** 在 epoll 集群之前就写下了("终态清理在 completion 路径间重复……single-close 处理会漂移")并精确预言了 D31/D32——台账看到了结构,但没有人对结构动手。

## 4. 为什么现有 gate 漏掉了每个模式

- **P2**:每个 gate 在每台主机只编译一个后端(编译期 cfg)。`test-feature-matrix` 变的是 cargo feature 不是后端。`MINISS_IO_BACKEND` 直到 `d3904d3`(2026-10-17)才在 main 存在;唯一 CI epoll 腿活了约 2 小时(`678a207`→`520b89d`)。任何静态 gate(arch-audit 是 grep)都看不见 cfg 级可达性。
- **P1**:`just test-differential` 只覆盖 TaskArena 模型;justfile 自己的注释承认"operation/kernel 协议在确定性 fake backend 出现前仍分裂在纯模型与后端回归测试之间"。
- **P3**:全零 termination-snapshot oracle 存在(`9e76372`)但是 post-join 且只在 just stress/linux-safety 里断言;`just test-regular` 显式 --skip 五个 shutdown-reclaims 测试(justfile 186-190 行)。不存在任何**活体**获取/释放平衡断言——证明 #43 的探针是临时的且已删。
- **P4**:Loom 覆盖三个任务级模型(cancel-waiter、sleep-gate、waker-deactivate)——不碰后端注册/取消 seam;按 AGENTS.md,`just loom` 在生产 seam 抽出前必须失败。Miri 覆盖 8 个具名测试,没有一个碰后端取消。
- **P5**:bench-smoke 单 5 秒样本强制 5% 主张而实测 ±40%(`d3904d3`);无测试二进制看门狗(泄漏进程跑了 19h);flaky 测试重跑到绿——诚实记录(#39、#40)但机械上无界。
- **流程**:Decide 规则(GREEN 要求 just check)没有强制臂;`de8f3f8` 四个 gate 未跑被合并并产生 D37-D39 连锁。

## 5. 超越 justfile 的流水线提案(按 预防力÷成本 排序)

**第一梯队——先建(小时到天;直接消灭覆盖类):**

1. **后端编译矩阵**。Linux 上 `cargo check --all-targets --all-features` + clippy 在 `MINISS_IO_BACKEND={io_uring,epoll}` 两种 cfg 下跑,macOS 跑 kqueue cfg,进 `just check`(Linux 腿)与 CI。消灭:P2 全部编译面(D16、D22 前提、`d3904d3` 死分支)永久。成本:小时。误报:无。
2. **恢复 Linux-epoll CI 腿**。任务原样存在于 `git show 678a207:.github/workflows/ci.yml`(`linux-epoll`、`MINISS_IO_BACKEND: epoll`、独立 target dir)。原样复活。消灭:P2 运行面——D22 本可提前 2 天在第一次 CI 失败,省掉 18 CPU 小时。成本:分钟。注意 `linux-safety` 脚本已正确跳过 <6.12 内核,此任务无需内核升级。
3. **活体资源收支不变量(debug/test 构建)**。每后端对 `descriptor_leases`、`pending_ops`、slot 元数据的获取/释放计数器;在每次 drain/park 边界断言 `released == acquired − in_flight`,shutdown 全零;发布进 `RuntimeTerminationSnapshot`。消灭:P3 的检测——D24/D25/D26b 从 33-60% flaky 变成同秒确定性失败。成本:数天。误报:必须建模合法在途集(不变量是收支方程不是"永远为零")。这正是台账 #7 明说不存在的那只**活体 quiescence oracle**。
4. **`assert_quiescent!` 宏 + fd 计数差,提升进 `test-regular`**。一个宏:断言全零 termination snapshot + 进程 fd 计数差(Linux `/proc/self/fd`,macOS libproc),成为每个构造 runtime 的测试的强制最后一行;把一个 loaded-shutdown 和一个 lease-retention 测试从 just stress 移进 test-regular,让 oracle 每次跑。消灭:P3 逃逸、P5 的泄漏 fd 污染。成本:1-2 天。
5. **合并纪律强制**。必需 CI 检查 = 全后端 cfg 的 `just check`;验证注记写着"clippy 没跑成"的 PR 按规则判 RED。成本:零代码。直接针对 D37-D39 连锁。

**第二梯队——数周;消灭漂移与错误路径类:**

6. **双后端差分测试架**。一个测试二进制在同一调用里对两个后端跑相同的 op 脚本(accept/connect/read/write/writev/UDP send-rent/fsync/cancel/abandon/loaded-shutdown),比对结果**和** quiescence snapshot。消灭:P1 全部于编写时——任一后端终态路径漂移,配对断言立即失败。成本:1-2 周;需要 justfile 已点名的确定性 fake-backend seam。误报:需一张 per-op 容差表(mio file-write buffer identity 等,`fb05baa`)。
7. **处处可用的故障注入 seam**。submit/register/cancel 失败经 test-only hook 可注入(io_uring 已有 EBADF 注入 `9e76372`;mio 有 UDP 注册失败 `2d3e6ce`——泛化)。消灭:D35/D26d/rm-09 错误路径清理类。validation-gaps 文档已建议,仍未完成。成本:中。
8. **取消路径覆盖 gate**。llvm-cov 跑具名 cancel/abandon 测试集;ECANCELED arm、retire pass、终态清理 arm 未执行即失败。消灭:P4 盲区。成本:中;注意 cfg 门控误报。

**第三梯队——高成本、增量价值窄(推迟或试点):**

9. **cargo-fuzz on src/http.rs**。消灭 parser 类(D11、framing、bare-LF、D37 的前身)。性质模型已存在(台账 #20/#29/#38),边际预防中等但真实——剩下的 bug 是**跨读的有状态 framing**,fuzz 很快能打中。成本:低-中。
10. **RAII/typestate 注册 API**。让遗弃不可表示:submit 返回 guard,Drop 即 cancel-and-record;readiness arm 在 token slot 里持有 guard。结构性消灭 P3/P4——唯一真正灭绝该类的方案。成本:数周 + 热路径风险;DECISION_LOG 2026-10-17 推迟过并写了明确推翻条件,而 #43 的残余正是那个条件。建议试点:只做 readiness arm(它们不携带 payload——`b0a31f2` 自己指出的),completion 作用域 lease 保留给 payload op。
11. **真实注册 seam 上的 Loom / eBPF fd-trace / LSAN-valgrind**。推迟。Loom 建模竞态;生命周期缺陷(#40/#41/#43)是单线程逻辑/所有权 bug,Loom 看不见。eBPF 追踪被第 3+4 项以更低成本覆盖。LSAN 会漏掉 #43(fd 被合法 Arc 持有——不是堆泄漏)。

**推荐最小集:** 第 1、2、5 项立即做(小时-天,零风险),然后第 3、4 项(数天,把生命周期类从 flaky 挂死变成确定性失败),然后第 6 项(P1 的结构性解药)。第 10 项是唯一让该类**灭绝**而非被检测的提案——按决策日志自己的推翻条款,当 #43 残余第一次咬到真实负载时排设计卡。

**给所有者的一句话:** 这类 bug 不断出现,是因为资源的获取/释放配对靠**约定**维持在大约 5 类 driver × 2 个后端 × N 条 completion 路径上——迄今每个修复都在失败现场手工配对(D24 的 retire pass、D25 的同轮 retire、D23 的取消邮箱),而没有把配对挪进类型或不变量;同时唯一能拦住覆盖半边的那条 gate(Linux-epoll CI 腿)存在了两个小时(`678a207`→`520b89d`)就被删了。
