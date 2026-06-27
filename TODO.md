# rust-miniss Task Tracker

> 每个任务用唯一 ID 标记。修改后必须通过 `just gate` 方可提交。
> 违反 MUST/MUST NOT 的任务直接标记 RED → 丢弃，记录到 `docs/FAILED_ATTEMPTS.md`。

## 任务卡模板

```yaml
---
id: <唯一 ID>
status: TODO | IN_PROGRESS | GREEN | YELLOW | RED
goal: 一句话目标
allowed_files:
  - 允许修改的文件路径
forbidden_patterns:
  - 禁止引入的代码模式
test_commands:
  - cargo test
benchmark_commands:
  - 性能测量命令
acceptance:
  - 通过标准
review_questions:
  - 审查时重点关注的问题
---
```

## Future / Exploration

| 项目 | 内核需求 | 收益预估 |
|------|---------|---------|
| IOSQE_IO_LINK 链式操作 | 6.0+ | WAL 场景 fsync 吞吐 +18% |
| Provided Buffer Ring (6.12+) | 6.12 | 消除 buffer 分配，混合流量稳定 |
| SQPOLL 模式 | 5.11+ | 消除 flush() syscall，需 root |
| Registered Buffers | 5.7+ | 减少页引脚开销，大 I/O 场景 |
| Multi-shot recv | 6.10 | 减少 recv SQE 数量 |
| DMA-BUF zero-copy recv | 6.16 | 真正零拷贝网络接收 |

## Known Architectural Issues

| Issue | Impact | Fix |
|-------|--------|-----|
| memcpy in Read/Write | ~3-4x on echo | Rent 路径修复（`read_rent`/`write_rent`） |
| No registered files | ~5-10% | Post-accept `IORING_REGISTER_FILES` |
| No multi-shot accept | Per-connect syscall | `IORING_OP_ACCEPT_MULTI` |
| No SQPOLL | Per-flush syscall | `IORING_SETUP_SQPOLL`（需 root） |
| No registered buffers | Per-I/O page pin | `IORING_REGISTER_BUFFERS` |
