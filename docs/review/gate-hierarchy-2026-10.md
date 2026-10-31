# Gate 层级整理与受影响目标选择(2026-10-31,DRAFT 供业主评审)

> **目的:** 回答业主 2026-10-31 的三条裁定:(1) `just test-fast` 收窄为"拦下最
> 愚蠢错误"的秒级内循环,任何 protocol-sim 型测试不得进入;(2) 提交门加重,并
> **自动**包含与改动子系统匹配的后端/平台/协议测试腿——该选择能力今天不存在;
> (3) 审计 justfile 里 32 条测试/门配方的责任混乱与重复并修复设计。
>
> **结论先行:** 盘点发现 **5 处结构性重复**与 **1 个零引用孤儿配方**
> (`check-backends`——defect-forensics Tier-1 第 1 项的 P2 杀手,自 `13fcca1`
> 加入起没有任何 check/gate/CI 引用它);合同(AGENTS.md)与实现漂移 **5 条**,
> 其中"默认开发与 CI 先跑 `just test-fast`"在 CI 五个 job 里一个都没跑。
> 修复设计为三层:**Tier 1 `fast`(秒级)/ Tier 2 `gate`(分钟级,新增
> `affected` 受影响目标选择)/ Tier 3 `nightly`(周期全量)**。受影响选择的
> 机械载体是 `scripts/test-affected.sh` + `scripts/test-affected-mapping.yaml`
> (本分支 DRAFT,已实测匹配/去重/拒绝空选择三条路径)。对 forensics 五个根因
> 模式的覆盖估计:P2 大部分转为本地拦截,P1/P3/P4 部分(提交时同跑双后端腿),
> P5 与跨子系统语义交互**不可拦**(§6 诚实边界——这是 integration-merge ritual
> 仍然必要的原因)。
>
> 分支 `refactor/gate-hierarchy`(基线 = `integration/2026-10-31` ccaf11b,
> GREEN 树)。本文件、`scripts/test-affected.sh`、映射表、justfile 草案编辑与
> DECISION_LOG 草案行均以 **REVIEWABLE PROPOSAL** 提交;业主逐条批准后才改
> AGENTS.md 与 CI,不批准即整体丢弃。

## 0. 业主裁定(本设计的输入)

| # | 裁定 | 设计后果 |
|---|------|---------|
| V1 | `just test-fast` 保持轻量(~秒级),只拦最愚蠢错误;**任何 protocol-sim 型测试不得进入** | Tier 1 只留 fmt + ignored 扫描 + arch-audit + `--lib` 单测 + 纯性质模型;`runtime_regressions`(host 后端 socket I/O)迁出 Tier 1 |
| V2 | 提交门加重,且自动包含与改动匹配的测试——特别是触及子系统的后端与平台腿(touch `src/io/uring.rs` → io_uring 腿;`mio_poll.rs` → epoll+kqueue 腿;`multicore.rs` → lifecycle/shutdown 套件;等等) | Tier 2 新增 `just affected`:git diff → 映射表 → 具体测试命令(后端 × 平台);`just gate` 串入 |
| V3 | justfile 测试配方责任混乱、可能重复——审计并修复 | §1 全量盘点,§1.5 重复量化,§1.6 孤儿/死配方,§1.7 合同漂移 |

## 1. 盘点与分类(基线 ccaf11b)

### 1.1 justfile 32 条配方

| 配方 | 用途 | 调用方 | 重叠/问题 | 判定 |
|------|------|--------|-----------|------|
| `check` | fmt+ignored 扫描+clippy+cargo check+test-regular | dev、gate | 不含 arch-audit(与 test-fast 责任倒挂,§1.7-D4) | **keep**,补 check-backends/docs-audit/unsafe-audit |
| `check-backends` | cfg(io_backend) 双后端编译矩阵 | **零引用** | P2 杀手(Tier-1 item 1,`13fcca1` 引入)却从未被 check/gate/test-full/CI 任何一个调用;CI 三条 OS 腿各只编译自己的单后端 | **keep,接进 check**(孤儿是本审计最大单点发现) |
| `docs-audit` | README 链接与基线行核对 | 仅 CI linux-epoll 单腿 | 三条 OS 腿只有一条跑;gate 不跑 | **keep,进 check** |
| `arch-audit` | 架构红线 grep(src/tests/bench/examples) | test-fast、gate、CI×3 | 在 fast 有、在 check 没有;合同写 "benches/" 实扫 "bench/"(§1.7-D5) | keep |
| `diff-audit` | 任务卡 allowed_files 越界 | gate(带 TASK) | — | keep |
| `bench-smoke` | 1 核 5s HTTP 冒烟 vs 同机 baseline | gate | AGENTS.md 已声明需显式 BASELINE;±40% 仪器(DECISION_LOG 2026-10-17 行) | keep,位置不动 |
| `review` / `review-diff` / `review-adversarial` | echo 提示(stub,不调任何 agent) | 手动 | 三条互相指路;review-diff 只打印 diff+清单 | **降级**:合成一条 `review`(打印两个模式的说明)或删,业主裁 |
| `gate` | check+arch+unsafe+diff+bench-smoke | dev 提交门 | 缺 affected(V2) | **keep+affected** |
| `bench-nop` | 跑 bench_nop 例程 | 零引用 | 手动工具 | keep(或删,业主裁) |
| `bench-http` | bench_http.sh 手动全参 | 零外部引用(bench-smoke 直呼脚本) | 与 bench-smoke 同脚本不同形状 | keep(手动) |
| `bench-multicore` | 多 worker park 效率门(台账 #46) | 手动 | park_efficiency_check.pl 已内嵌 bench_http.sh:411 多 worker 路径 | keep(Tier 3/专项;gate 内基准只有 bench-smoke 与它) |
| `bench-matrix` / `bench-topology` / `bench-uring-flags` / `bench-verdict` | 显式多轮矩阵/拓扑/flag A/B/机械判分 | 手动 | 互不重复,自声明"不进任何 gate" | keep(手动,Tier 3 工具) |
| `test` | ignored 扫描+test-regular | 零外部引用 | 是 check 的真子集,第三入口 | **删**(或并入 check 注释) |
| `test-regular` | 全量常规测试(host 后端,`--test-threads=1`) | check、test | `--skip` 10 行清单与 test-all-features **逐字重复**(§1.5-#3) | keep,共享清单(本分支已并) |
| `stress` | 30s 压力+关闭回收 | CI linux 腿、test-full | linux-safety 重复 1 个用例×3 轮;CI per-push + nightly 各一遍(§1.5-#5) | keep(Tier 3) |
| `test-all-features` | fmt+ignored+clippy+全量 test(--all-features) | 仅 test-feature-matrix | 被调用时 CI 腿已跑过 fmt/clippy/test → **每腿双跑**(§1.5-#2) | keep,去 fmt/clippy(本分支已改) |
| `test-feature-matrix` | 4 个 feature 组合 check + test-all-features | CI×3、test-full | 与 check-backends **正交不重复**(cargo feature vs io_backend cfg),但属编译面矩阵非行为测试 | keep(Tier 3) |
| `test-full` | check+feature-matrix+differential+loom+stress+linux-safety+miri | 文档引用(README_CN.md:165、DESIGN.md:727、DECISION_LOG 2026-07-14 行) | 名字暗示"完整"实为周期聚合器 | **重定位 Tier 3**,别名 `nightly`(本分支已加) |
| `test-differential` | runtime_safety_models | test-full | 与 test-fast 的同一 `--test` 重复入口 | keep(Tier 3) |
| `test-fast` | fmt+ignored+arch+`--lib`+models+**runtime_regressions** | dev;**CI 不跑**(§1.7-D1) | check 的子集+arch-audit;runtime_regressions 含 socket I/O(accept multishot 等 15 测)——V1 下失去秒级资格 | **重定义 = `fast`**(本分支已改,旧名别名) |
| `unsafe-audit` | SAFETY 注释门(clippy undocumented_unsafe + # Safety 扫描) | gate | CI 0/5 job 跑 | keep,进 check |
| `fmt` / `clippy` | 便利单步 | 手动 | check 子集 | keep |
| `miri-safety` | 8 具名 TLS/waker/buffer Miri 测试 | CI miri 腿、test-full | 与 loom 同属"生产 seam 未抽取则失败"纪律 | keep(Tier 3) |
| `no-ignored-tests` | ignored 扫描别名 | 手动 | 各 gate 已内嵌 | keep |
| `loom` | 3 个生产链接模型 | CI loom 腿、test-full | AGENTS.md:生产同步 seam 未抽取时必须失败 | keep(Tier 3) |
| `linux-safety` | 具名 Linux/io_uring 回归(28 条 exact) | CI linux 腿、test-full | 与 stress 重叠 1 用例×3 轮;与 runtime_regressions 是同二进制的具名子集(§1.5-#5) | keep(Tier 3) |

### 1.2 scripts/(7 个)

| 脚本 | 被谁调 | 判定 |
|------|--------|------|
| `audit_architecture.sh` | arch-audit、test-fast、CI×3 | keep |
| `check_no_ignored_tests.sh` | check、test、stress、test-all-features、test-fast、test-feature-matrix、CI | keep(每个 gate 各扫一遍,成本 ~1s,可接受) |
| `check_prometheus_metrics.sh` | **零引用**(validation-pipeline-gaps-2026-10.md §3 引为手动检查,需 Docker) | keep(手动,登记进 Tier 3 附录) |
| `check_readme_references.py` | docs-audit | keep |
| `check_unsafe_safety.sh` | unsafe-audit | keep |
| `linux_safety.sh` | linux-safety | keep |
| `run_exact_test.sh` | loom、miri-safety、linux-safety | keep |
| `test-affected.sh`(本分支新增) | affected、gate | **DRAFT,§3** |

### 1.3 bench/scripts/(7 个)

| 脚本 | 被谁调 | 判定 |
|------|--------|------|
| `bench_http.sh` | bench-smoke、bench-http、bench-multicore(**gate 承载**) | keep |
| `park_efficiency_check.pl` | bench_http.sh:411 多 worker 路径(**gate 承载**,#46 门) | keep |
| `compare_http_baseline.pl` | bench-smoke(**gate 承载**) | keep |
| `bench_http_matrix.sh` / `bench_http_topology.sh` | bench-matrix / bench-topology(手动) | keep |
| `bench_uring_flags.sh` | bench-uring-flags(手动) | keep |
| `bench_verdict.pl` | bench-verdict(手动) | keep |
| `bench_hyper.sh` | **不存在**(本审计怀疑清单提及;核实:分支上无此文件,hyper 面由 tests/hyper_http1/2.rs 覆盖,无 bench 脚本) | 记录在案,无需动作 |

### 1.4 CI 五个 job 与 recipe 的关系

| job | 步骤 | 发现 |
|-----|------|------|
| macos kqueue | fmt / clippy / check / test / feature-off test / feature-matrix / ignored / arch | **内联复刻** recipe 而非调用(旗标还不同:全部 `--all-features`,本地 check 是默认 feature);feature-matrix 内部再跑一遍 fmt+clippy+全量 test |
| linux-epoll | 同上 + docs-audit,`--test-threads=1` | 唯一跑 docs-audit 的腿;`CARGO_TARGET_DIR=target/epoll` 与本设计 affected 的 epoll 腿一致 |
| linux io_uring | 同上 + linux-safety + stress | 内核 6.12 硬门正确;stress 在 per-push(§4-CI-delta 建议移 nightly) |
| loom | just loom | 直调 recipe,干净 |
| miri | just miri-safety | 直调 recipe,干净 |

**CI 不跑**:`just check`、`just test-fast`、`just gate`、bench-smoke、diff-audit、unsafe-audit——提交门在 CI 没有对应物,CI 是一套平行实现(§1.7-D1/D2 的直接来源)。

### 1.5 重复量化(前 5)

| # | 重复 | 位置 | 代价 |
|---|------|------|------|
| 1 | **check-backends 孤儿**:P2 编译矩阵配方零调用 | justfile:26 vs `grep -r check-backends`(仅自身);CI 三腿各编译单后端 | P2 防线的一半(本地编译面)从未生效 |
| 2 | **CI 每腿双跑静态步**:3 条 OS 腿内联 fmt/clippy/test 后,`just test-feature-matrix` → `test-all-features` 把 fmt+clippy+全量 test 再跑一遍 | ci.yml:16-26/44-54/106-116 + justfile:242-256(基线行号) | 每腿约多付一次 clippy + 一次全量测试的 CI 分钟;三腿 ×3 |
| 3 | **`--skip` 清单双副本**:test-regular 与 test-all-features 逐字维护同一份 10 行 stress 跳过清单 | justfile:215-226 vs 246-256(基线) | gate 系统内部的 P1 漂移面(漏改一份=两个 gate 对 stress 的边界不同) |
| 4 | **test-fast 与 check 责任倒挂**:arch-audit 在 fast 不在 check;runtime_regressions(socket I/O)在 fast 使其非秒级;而 CI 根本不跑 test-fast | justfile:310-316 vs 12-19;ci.yml 全文 | 违反 AGENTS.md "默认开发与 CI 先跑 just test-fast";fast 循环被 I/O 测试拖到分钟级 |
| 5 | **stress × linux-safety 用例重叠**:`test_multicore_udp_receive_close_race_reclaims_state` 在 stress(30s 参数)与 linux-safety(64 socket×3 轮)双跑;stress 在 CI per-push 与 test-full 再双跑 | justfile:230-237、355-374;ci.yml:122-124 | 同一义务两处维护,参数漂移无告警 |

次要重复:runtime_safety_models 双入口(test-fast 与 test-differential);docs-audit 仅 1/3 腿;unsafe-audit 0/5 job;ignored 扫描每 CI job 内嵌+显式共 2 次(成本可忽略,不列)。

### 1.6 死配方/孤儿/入口收敛

- 零引用配方:`check-backends`(孤儿——**接回**)、`test`(删)、`bench-nop`/`bench-http`(手动工具,保留)、`review-diff`/`review-adversarial`(echo stub,建议并入 `review`)。
- `no-ignored-tests`/`fmt`/`clippy`:手动便利,保留。

### 1.7 合同(AGENTS.md)与实现漂移 5 条

| # | 合同说 | 实际是 |
|---|--------|--------|
| D1 | "默认开发与 CI 先跑 `just test-fast`" | CI 五个 job 没有一个跑 test-fast |
| D2 | gate 定义 "`just check`:fmt + clippy + `cargo test --tests --examples`" | 实际 check 还含 ignored 扫描、`cargo check --all-targets --all-features`、test-regular(含 --lib 与 examples) |
| D3 | test-fast 定义 "fmt + 普通单测 + 64-case bounded property models" | 实际还含 `--test runtime_regressions`(host 后端 socket I/O;红线只写在 property models 上,留了漏洞) |
| D4 | arch-audit 定义未提 test-fast 归属 | arch-audit 实际只在 fast/gate/CI,不在 check——提交门主步反而没有架构扫描 |
| D5 | arch-audit 扫 "src/、tests/、benches/" | 实扫 src/tests/bench/examples(D21 修复后的事实,合同未跟上) |

## 2. 三层设计

| 层 | 入口 | 职责(一句话) | 预算 | 所有人 |
|----|------|---------------|------|--------|
| Tier 1 | `just fast`(`test-fast` 别名) | 拦最愚蠢错误:格式、ignored 测试、架构红线 grep、单测/静态断言、纯性质模型 | 热缓存 ≤30s(冷构建吃 cargo 增量) | 开发者本人,每次保存/提交前 |
| Tier 2 | `just gate TASK=… BASELINE=… [BASE=…]` | 提交门:check(静态+编译矩阵+全量常规测试)+ **affected(受影响目标选择)** + arch/unsafe/docs 审计 + diff-audit + bench-smoke | 热缓存 ≤10 分钟;CI 后端腿各自计时 | 提交者;Decide GREEN 的机械前置 |
| Tier 3 | `just nightly`(`test-full` 别名)+ 显式 bench/quic 配方 | 周期全量:feature 矩阵、differential、loom、stress、linux-safety、miri;QUIC interop/h3spec/h2spec;T3 微基准;跨运行时矩阵 | 30-90 分钟 | nightly CI schedule + 合并集成前手动 |

### 2.1 Tier 1 `fast`(V1 的机械化)

**放进**:fmt、`check_no_ignored_tests.sh`、`audit_architecture.sh`(三者秒级,后者把 D20 型 type-alias 藏锁这类"愚蠢错误"挡在编译前)、`cargo test --lib`(单测+静态断言)、`cargo test --test runtime_safety_models`(纯性质模型,无 socket/fd/thread/sleep/wall-clock)。

**不进**(负清单,写入合同):**Tier 1 任何测试二进制不得使用 socket、fd、thread、sleep 或 wall clock**——把旧合同只约束 property models 的红线扩到整个 Tier 1。因此 `runtime_regressions`(bounded I/O,含 accept multishot 套件)迁 Tier 2:它本来就是 test-regular 的成员,Tier 2 每次都跑;开发者改 io/net 后由 `affected` 立即单跑该二进制(§3),无覆盖损失。

**不进**:clippy(全 target 编译,非秒级)、一切 protocol-sim 型测试(V1)、feature 矩阵。

### 2.2 Tier 2 `gate`(V2 的机械化)

组成(顺序即失败顺序):`check`(现在含 check-backends/docs-audit/unsafe-audit)→ `affected` → `arch-audit` → `unsafe-audit` → `diff-audit TASK` → `bench-smoke BASELINE`。返回码契约不变(0/1/2/3)。

对比旧 gate 的三处加重:后端编译矩阵进主门(P2 的本地半边);docs/unsafe 审计进主门(原 0/5 job 或 1/3 腿);affected 把"改动子系统的专属腿"变成机械义务(原来完全没有这个能力——linux-safety 的 28 条具名回归只能整包跑或靠人记)。

### 2.3 Tier 3 `nightly`

组成不变(§1.1 test-full 行),角色重定位:不是每提交义务。触发:CI schedule(nightly)+ 合并集成前手动。结果记录:CI artifacts + `docs/review/runtime-safety-findings.md`(安全类)/ `performance-baseline-*.md`(性能类)。stress 从 CI per-push linux 腿移到 nightly 是一条待批 delta(§4-CI-3)——linux-safety 的具名回归仍在 per-push 腿,生命周期义务不缩。

### 2.4 QUIC 方法论 T1-T5 的落位(V1 裁定Applied)

`feat/quic-service-lifecycle` 的 quic-test-methodology-2026-10.md §3 表格提出了 gate 位置;按本设计落位如下:

| QUIC 层 | 方法论文档原提议 | **本设计落位** | 触发 |
|---------|-----------------|----------------|------|
| T1 quinn-proto 92 测试重放 | "进 `just check` 附加步" | **Tier 2 affected**:`Cargo.lock`/`Cargo.toml`/`src/quic.rs` 规则挂 `cargo test -p quinn-proto --locked` 腿(映射表 §3 的 Cargo.toml 规则已留注释位) | 依赖或 QUIC 面变更 |
| T1 h3spec/h2spec harness | "进 check" | **Tier 3**(需例程 cherry-pick + CA 环境;harness 就绪后可升 Tier 2,由映射表挂腿) | nightly / 手动 |
| T2 miniss 真实回环三后端腿 | "`just check`;Linux 两后端独立 CI 腿" | **Tier 2 affected**:`src/quic.rs` → quinn_h3 host+epoll 腿(映射表已含)+ test-regular | src/quic.rs、quinn_h3.rs 变更 |
| T3 每连接生命周期微基准 | "显式 bench recipe" | **Tier 3 显式配方**(不变);任何性能主张前必须有 T3 数据(AGENTS.md RED 纪律) | 手动,主张前强制 |
| T4 性质核心 sim-flavor | "进 `just test-fast`"(§3 表格 T4 行、§8 步骤 1、§9 开放问题 1) | **Tier 2 affected**——**V1 明确否决 test-fast 收编 protocol-sim**;映射表 `src/quic.rs` 规则已留 "T4 sim-flavor 就绪后追加 tests/quic_lifecycle_props 腿" 注释位 | sim 脚手架/生成器路径变更 |
| T4 real-flavor | "进 `just check`" | **Tier 2**(test-regular 或映射腿) | 同上 |
| T5 gate 不变量 | "嵌在 T2/T4 内,`just check` 每次跑" | **不变**:嵌入宿主层,随 Tier 2 跑 | — |
| interop-runner | "周期性全栈检查,不进 per-commit gate" | **Tier 3 周期**(orb VM,arm64 自建镜像) | 周级 |

**该文档需要的一行修订**(业主批准本设计后由该分支顺手改):§3 表格 T4 行 "sim-flavor 进 `just test-fast`" → "**sim-flavor 进 Tier 2 提交门的受影响选择(`src/quic.rs`/sim 脚手架路径触发);业主 2026-10-31 裁定 test-fast 不收编任何 protocol-sim 测试(gate-hierarchy-2026-10.md §2.4)**"。同表 T1 行 "进 `just check` 附加步" → "进 Tier 2 affected(`Cargo.lock`/`src/quic.rs` 触发)"。

## 3. 受影响目标选择机制(V2 的机械化)

### 3.1 形态

- `scripts/test-affected-mapping.yaml`(DRAFT):source-path 前缀 → (测试二进制, 后端, 平台) → 具体 cargo/env 命令。规则 = `match` 前缀列表 + `legs`(platform: linux/macos/any;backend: io_uring/epoll/kqueue/host/both)。**最长前缀命中**,每文件贡献其命中规则的全部 legs,跨文件去重。兜底规则 `""` 必须存在。
- `scripts/test-affected.sh`(DRAFT):`[--dry-run] [BASE_REF]`,BASE 缺省 `git merge-base HEAD origin/main`,任务卡工作流传 baseline 提交。后端可用性镜像 build.rs:67-91(io_uring/epoll 仅 Linux,kqueue 仅 macOS);具名后端设 `MINISS_IO_BACKEND` 并按 CI 惯例给 epoll 独立 `CARGO_TARGET_DIR=target/epoll`(切换后端无需 cargo clean,DECISION_LOG 2026-10-17 行)。跑完全部腿再汇总失败(失败清单一次给全,不首败即停)。
- **绝不静默为空**:选中 0 条命令 → 退出码 2 + 明细;未匹配路径被兜底规则接住并告警;唯一豁免开关 `MINISS_AFFECTED_ALLOW_EMPTY=1` 显式写明。本分支已实测:匹配/去重正确(uring/mio/quic/multicore/tests/docs 混合清单在 macos 与 linux 两平台各产出正确腿集)、真实执行路径 GREEN、空 diff 以退出码 2 拒绝。
- `just affected base=""` 与 `just gate task="" baseline="" base=""` 已串入(本分支 justfile 草案)。

### 3.2 映射表 DRAFT 的子系统 → 腿(节选,全文见文件)

| 触碰 | 自动腿 |
|------|--------|
| `src/io/uring.rs`、`src/io/buf_ring.rs` | Linux:io_uring `--lib io::uring` + runtime_regressions/vectored_io/test_fast_poll;**epoll 编译检查**(改动不得让另一后端失去编译——P2) |
| `src/io/mio_poll.rs` | Linux:epoll 回归腿;macOS:kqueue 回归腿(D10/D12/D13/D23 的平台腿) |
| `src/io/mod.rs`、`src/io/future.rs`、`src/fs.rs` | 双后端同套件(runtime_regressions + async_file_tests) |
| `src/multicore.rs`、`src/waker.rs`、`src/executor.rs`、`src/cpu.rs`、`src/task.rs`、`src/cancellation.rs`、`src/timer/` | **lifecycle+shutdown 套件双后端**(multicore_tests + shutdown_quiescence;#40/#42/D25/D27 类) |
| `src/http.rs`、`src/net.rs`、`src/hyper_rt.rs`、`src/compat.rs`、`src/buffer.rs` | integration_http + hyper_http1/2 host;Linux 另跑 epoll integration_http |
| `src/quic.rs`、`src/tls.rs` | quinn_h3(--all-features)host + Linux epoll(T2 落位) |
| `Cargo.toml`/`Cargo.lock`、`build.rs`/`justfile`/`.github/`/`scripts/`/`bench/` | everything:check-backends + test-regular |
| `tests/shutdown_quiescence.rs` 等 | 直接跑该二进制(部分登记);`tests/` 兜底 = 整 host 套件 |
| `docs/`、README、AGENTS.md | docs-audit |
| 其他任何路径(兜底 `""`) | docs-audit + 告警(宁可重跑不可漏跑;业主认为需要真腿就加规则) |

### 3.3 覆盖评估 vs defect-forensics-2026-10.md 的根因模式

| 模式 | 成员(数) | 受影响选择能否本地拦 | 依据 |
|------|-----------|---------------------|------|
| **P2 未编译/未测回退** | D16/D17-D19/D22 前提/d3904d3/D39(6) | **大部分能** | 触 `src/io/uring.rs` → epoll 编译检查;触 `mio_poll.rs` → epoll 测试腿;macOS-only 测试在 macOS 腿当次必跑(D39 的"PR 自带 macOS 测试永不过"变成机械触发);check-backends 进 check 双保险。剩余:build.rs 探针逻辑本身(<6.12 分支)仍需 PATH shim 类专项验证(DECISION_LOG 2026-10-17 行的做法,不入映射) |
| **P1 兄弟路径漂移** | D15/D12/D13/D18/D31/D32 等(8) | **部分** | 双后端同套件在提交时同跑,行为漂移当次暴露;但 D15 型"双 drain 路径语义差分"需要 forensics 第 6 项的确定性差分架,不在本设计范围 |
| **P3 记账/所有权不对称** | D24/D25/D26b/D27/D29/D8 等(8) | **部分** | shutdown_quiescence 已在 test-regular(Tier 2 每次跑);multicore/waker/executor 变更额外触发 lifecycle 双后端。**活体收支不变量(forensics 第 3 项)仍未实现,本设计不替代它** |
| **P4 取消/遗弃竞态** | D23/D24/D32/D28/D41(5+2) | **部分** | 同 P3 腿;Loom 维持 AGENTS.md 纪律(生产 seam 未抽取必须失败) |
| **P5 测试环境假象** | D44/D45/D41-D43/D14(8) | **不能** | 泄漏进程、污染基线、±40% 仪器需要的是 PROBE/环境有效性断言(#46 的 park_efficiency_check 是正确形态),与测试选择正交;bench-smoke 的 baseline 纪律照旧 |
| 流程(D37-D39 连锁) | PR #3 四 gate 未跑被合并 | **能** | gate 一条命令串全部门 + affected;不再依赖注记自觉 |

## 4. 合同修订清单(AGENTS.md,逐条待业主批准)

1. **Gate Definitions → `just test-fast`**:定义改为 Tier 1(fmt + ignored 扫描 + arch-audit + `--lib` + 纯性质模型);**新增负清单:Tier 1 任何测试不得使用 socket/fd/thread/sleep/wall-clock**;`runtime_regressions` 迁 Tier 2。理由:V1;关闭 D3 漂移的红线漏洞。
2. **Gate Definitions → `just check`**:补 check-backends、docs-audit、unsafe-audit 三步进定义。理由:三者现无家或单点(§1.5-#1、§1.4)。
3. **Gate Definitions → `just gate`**:序列改为 check → **affected** → arch-audit → unsafe-audit → diff-audit → bench-smoke;affected 的 BASE 缺省 merge-base origin/main,任务卡必须传 baseline。
4. **新增:`scripts/test-affected-mapping.yaml` 是 gate 契约的一部分**;修改映射表的提交必须附 `scripts/test-affected.sh --dry-run <BASE>` 输出自证未缩覆盖(本分支映射表头已写明)。
5. **Gate Definitions → `just test-full`**:重定位为 Tier 3 周期入口,别名 `nightly`;组成不变。
6. **arch-audit 描述**:扫描目录 "src/、tests/、benches/" 修正为 "src/、tests/、bench/、examples/"(D5)。
7. **CI 章节(给业主选)**:(a) 三条 OS 腿改调 `just check` + 各自 backend env,消除内联复刻与双跑(§1.5-#2);(b) stress 从 per-push linux 腿移 nightly schedule,linux-safety 留 per-push;(c) docs-audit/unsafe-audit 不再单腿。**本分支不改 ci.yml**(超出授权文件清单;批准后另卡执行)。
8. **`just review`**:三条 stub 合并或删除(§1.6)。

## 5. justfile 草案编辑(本分支已提交,业主审 diff 而非散文)

| hunk | 内容 | 理由 |
|------|------|------|
| 头注 | 三层模型 + 返回码不变 | 评审入口 |
| `STRESS_SKIPS :=` | 10 行跳过清单提为唯一变量,test-regular/test-all-features 引用 | 杀重复 #3 |
| `check` | +check-backends +docs-audit +unsafe-audit | §4-2 |
| `affected base=""` | 新配方,调 test-affected.sh | §3 |
| `gate` | 插 affected 步,返回码契约不变 | §4-3 |
| `fast` + `test-fast: fast` | Tier 1 重定义,旧名别名 | V1 |
| `test-all-features` | 去 fmt/clippy(调用方已有) | 杀重复 #2 |
| `nightly` + `test-full: nightly` | Tier 3 重定位 | §4-5 |
| 其余 24 条配方 | 一字未动 | 最小 diff |

## 6. 诚实边界

1. **跨子系统语义交互,affected 拦不住。** 映射表按路径前缀工作;它看不见语义。integration dry-run 的 dead-arm 泄漏类(一个子系统的改动只在另一个子系统的负载形态下显形——#46 的"多 worker 加入被单 worker 调优的循环"正是这种涌现交互)不可能由文件前缀规则触发。这就是 **integration-merge ritual 仍然必要**的原因:Tier 3 全量 + 合并集成树上的整树验证是网格的最后一层,affected 只是让提交门"知道改了什么",不替代"知道改动意味着什么"。
2. **映射表错误的代价 = false green。** 该跑的腿没配 → 腿静默不跑 → gate 绿但覆盖缩水。缓解四层:兜底规则保证未登记路径不静默;改映射表必须附 dry-run 自证(§4-4);映射表纳入 diff-audit 的 allowed_files 纪律;Tier 3 周期全量兜底。但缓解不等于消除——**这是把"测试选择"从人的记忆搬进一份需要像代码一样被 review 的表**,表错了 gate 就诚实地说谎。业主批准 §4-4 即接受此代价结构。
3. **affected 不缩减 CI 义务。** CI 腿照旧全量;affected 是本地提前暴露,不是 CI 缩减方案(§4-7 是 CI 自身的去重,与 affected 无关)。
4. **P5 与活体不变量不在本设计射程**(§3.3):forensics 第 3 项(活体收支 oracle)与 PROBE 类断言仍需独立卡片;本设计只是保证现有 oracle(shutdown_quiescence)与现有腿被"该跑就跑"。
5. **预算是目标不是硬保证**:Tier 2 的 10 分钟按当前测试量估算(test-regular 在 6.12+ host 全绿 ~26-39s/次,DECISION_LOG 2026-10-17 行;affected 增量腿 2-8 分钟);若映射命中 everything 规则(build.rs/CI 变更)会更久,这是设计意图而非回归。

## 7. 交付物清单(本分支)

| 文件 | 性质 |
|------|------|
| `docs/review/gate-hierarchy-2026-10.md`(本文) | 设计提案 |
| `scripts/test-affected-mapping.yaml` | DRAFT 映射表(契约候选) |
| `scripts/test-affected.sh` | DRAFT 脚本(已实测:匹配/去重/平台过滤/真实执行/空选择拒绝) |
| `justfile` 草案编辑 | 8 个 hunk(§5) |
| `docs/DECISION_LOG.md` 追加一行 | 决策草案(选择/拒绝/证据/推翻条件) |

未动:`.github/workflows/ci.yml`、`AGENTS.md`(§4 是给业主的修订清单,批准后另卡)、其余 24 条配方、所有 bench 脚本。
