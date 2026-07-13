# rust-miniss Roadmap

提交前门禁是 `just test-fast`；Linux 性能实验使用独立的 benchmark 命令，
不把长时间运行的服务塞进默认测试。

## Future / Exploration

| 项目 | 内核需求 | 收益预估 |
|------|---------|---------|
| IOSQE_IO_LINK 链式操作 | 6.0+ | WAL 场景 fsync 吞吐 +18% |
| Provided Buffer Ring (6.12+) | 6.12 | 消除 buffer 分配，混合流量稳定 |
| SQPOLL 模式 | 5.11+ | 消除 flush() syscall，需 root |
| Registered Buffers | 5.7+ | 减少页引脚开销，大 I/O 场景 |
| Multi-shot recv | 6.10 | 减少 recv SQE 数量 |
| DMA-BUF zero-copy recv | 6.16 | 真正零拷贝网络接收 |

## Current verification

- HTTP/1.1、HTTP/2、QUIC/HTTP-3 integration tests pass on macOS/kqueue and
  Linux/io_uring.
- `just test-fast` is the default correctness gate.
- Performance claims require fresh Linux measurements with throughput, p99,
  syscall counts, and host/kernel metadata.
