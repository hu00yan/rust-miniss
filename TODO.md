# 性能修复清单

> 做完一条删一条，不回退。

## ✅ 已完成（本会话）

### 编译修复

- [x] **uring.rs 语法损坏修复** — `Op::Read` arm 混入 `data: data` (属于 Write)，导致 Linux 编译失败
- [x] **`_data` → `data` 命名修复** — `PendingOp::Write` 和 `WriteFile` 的字段名在 submit/poll_complete 中不统一，3 处 `_data` 应改为 `data`
- [x] **重复 `Op::WriteFile` arm** — 删掉被 patch 损坏后重复的 WriteFile 分支
- [x] **缺失 `Op::Write` arm** — 被 patch 的 `Write`→`WriteFile` 替换错误吃掉了，补回
- [x] **io-uring 0.7.10 API 兼容** — `Builder::default()` → `IoUring::builder()`; `&mut *ring.submission()` → `ring.submission()`; `remove()` → `try_remove()`; 补 `use std::time::Duration`
- [x] **dead_code 警告** — `PendingOp::WriteFile.data` 从未读取; `PendingOp::ReadVectored.iovecs` 从未读取; `PendingOp::WriteVectored.{buffers,iovecs}` 从未读取

### Rent 路径修复（性能）

- [x] **WriteRent 不返回 buffer** — 完成时返回 `Buffer::empty()` 而非真实的 buffer，调用方拿到的 buffer 永远是空的，rent 语义失效。修复：返回 `PendingOp::Write.data`
- [x] **ReadRent/Write/WriteFile/WriteRent 多余 clone** — submit 时 Op 字段里 `data.clone()` / `buffer.clone()` 完全浪费，Op 字段的 data 从未被使用。修复：使用 `Buffer::empty()` 占位

### CpuIoState 优化

- [x] **HashMap → Vec** — `io_wakers`, `completed_io`, `pending_tasks` 从 `FxHashMap<IoToken, V>` 改为 `Vec<Option<V>>`。IoToken 是 slab index，用 Vec 直接下标访问 O(1) 无 hash，无 cache miss

### 之前的修复（本阶段）

- [x] **IoToken 重建 bug** — `poll_complete()` 用 `IoToken::new()` 而非 `IoToken::from_id(token_id)`，所有 completion 静默丢弃
- [x] **Slab** — `pending_ops` 用 `slab::Slab<PendingOp>`，CQE `user_data` 直接是 slab index，O(1) 定位
- [x] **批量 SQE 提交** — `flush()` 在事件循环末尾统一 `ring.submit()`，不在 submit() 里每次 syscall
- [x] **io_uring 优化 flags** — `IORING_SETUP_SINGLE_ISSUER | DEFER_TASKRUN | COOP_TASKRUN`
- [x] **SQE 批处理阈值** — SQ >75% 或 >128 条时主动 `flush()` 避免紧急提交
- [x] **PollAdd + multishot** — `IORING_OP_POLL_ADD` 支持，multishot 用 `cqe.more()` 检测持续 CQE
- [x] **Waker: Rc + raw pointer** — 无 Arc，无 Mutex，thread-local queue

### 本次会话新增修复

- [x] **Write EAGAIN → POLLOUT 重试** — TCP socket 写缓冲区满时返回 -EAGAIN，之前视为错误断开连接。修复：在 `poll_complete` 内部自动提交 POLLOUT，可写时重试 Write。通过 `WriteRetryWaiting` completion 让 `poll_io` 迁移 waker/task。**1024 连接吞吐从 62K 提升到 267K（4.3x）**
- [x] **UdpRecv/UdpSend/ReadVectored/WriteVectored 多余 buffer clone 消除** — 使用 `Buffer::empty()` / `Vec::new()` 占位，避免每次 I/O 的 memcpy
- [x] **bench_nop 修复** — 之前测的是 task spawn 而非 io_uring NOP。现在通过 `Op::Nop` + `IoFuture` 走真实 io_uring NOP 路径
- [x] **CQE 收集提前** — 将 CQE 收集到 `Vec<Entry>` 中再处理，避免 `ring.submission()` 与 `ring.completion()` 的借用冲突（`IoUring` 的 shared borrow 不允许多次访问）

## P1 — 待做（按优先级）

- [ ] **注册文件描述符（IORING_REGISTER_FILES）** — 减少 fd 表查找开销
- [ ] **Multi-shot accept** — 一个 SQE 持续产生新连接 CQE，无需每次 accept 重新 arm
- [ ] **多线程 wrk benchmark** — 4 worker 线程压单核，测事件循环在高负载下的稳定性
- [ ] **FxHashMap → 原地 task 存取** — 消除 remove + insert 两个 hash 操作
- [ ] **bench_nop 改进** — 单任务批量提交 NOP，测纯 io_uring 路径吞吐（目标 10M+）
