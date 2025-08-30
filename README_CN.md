# rust-miniss

[![CI](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/ci.yml)
[![Docs](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/docs.yml)
[![Benchmarks](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/benchmarks.yml)
[![Nightly](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/nextest-nightly.yml)
[![Sanitizers](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/memory-sanitizers.yml)
[![Diff Coverage](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/diff-coverage.yml)
[![Typos](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml/badge.svg)](https://github.com/hu00yan/rust-miniss/actions/workflows/typos.yml)
[![Container Image](https://img.shields.io/badge/ghcr.io-rust--miniss-blue?logo=docker)](https://github.com/hu00yan/rust-miniss/pkgs/container/rust-miniss)

[miniss](https://github.com/qqiangwu/miniss) 的 Rust 实现 - [Seastar](https://github.com/scylladb/seastar) 框架的简化版本。

## 概览

rust-miniss 是一个最小化的异步运行时，展示了高性能服务器设计原理：

- **无共享架构**：每个 CPU 核心独立运行
- **无锁通信**：通过 SPSC 队列进行跨 CPU 消息传递
- **零拷贝 I/O**：在 Linux 上使用 io-uring
- **自定义 Future**：深入理解异步内部机制

## 快速开始

### 开发环境设置
```bash
# 安装开发工具
make install-tools

# 快速检查（格式化 + clippy）
make quick

# 完整检查（格式化 + clippy + 测试 + 构建）
make check

# 完整测试套件，包含内存检测器
make full
```

### 开发工作流
```bash
# 1. 编写代码
# 2. 自动格式化
make fmt-fix

# 3. 快速验证
make quick

# 4. 运行测试
make test

# 5. 提交前检查
make pre-commit
```

### CI 故障排除
| CI 任务 | 本地命令 |
|--------|---------------|
| 快速检查 | `make quick` |
| 测试套件 | `make check` |
| ASan+LSan | `make asan` |
| TSan | `make tsan` |
| Miri | `make miri` |

## HTTP Echo 示例和基准测试

- 示例：`examples/http_echo.rs`（HTTP/1.1 最小化回显服务器）
- 运行方法：
  - 构建发布版：`cargo build --release --examples`
  - 启动服务器：`./target/release/examples/http_echo --addr 127.0.0.1:8080`
  - 使用 wrk 进行基准测试：`wrk -t4 -c256 -d30s http://127.0.0.1:8080/`
- 结果（macOS，本地主机）：
  - 请求数/秒：21978.09
  - p99 延迟：N/A（此次运行的 wrk 输出格式未暴露 p99）
- 更多详情：查看 docs/benchmarks/http.md

## 目标

1. **教育性**：学习异步运行时的内部工作原理
2. **性能**：针对特定用例实现近乎最优的性能
3. **简洁性**：保持代码库小巧且易于理解（~2000 行）

## 架构

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│   CPU 0     │     │   CPU 1     │     │   CPU 2     │
│  ┌───────┐  │     │  ┌───────┐  │     │  ┌───────┐  │
│  │Tasks  │  │────▶│  │Tasks  │  │────▶│  │Tasks  │  │
│  └───────┘  │◀────│  └───────┘  │◀────│  └───────┘  │
│  ┌───────┐  │     │  ┌───────┐  │     │  ┌───────┐  │
│  │IO Ring│  │     │  │IO Ring│  │     │  │IO Ring│  │
│  └───────┘  │     │  └───────┘  │     │  └───────┘  │
└─────────────┘     └─────────────┘     └─────────────┘
     SPSC Queue          SPSC Queue
```

## 构建

```bash
# 在支持 io-uring 的 Linux 系统上（推荐，性能最佳）
cargo build --features io_uring

# macOS/其他平台（回退模式）
cargo build
```


## 示例

[![HTTP benchmark](https://img.shields.io/badge/http%20echo-benchmark-blue)](docs/benchmarks/http.md)

- HTTP Echo：examples/http_echo.rs（参见上述基准测试）

### 作为轻量级运行时/网络库使用

- 添加到项目：此 crate 提供了最小化运行时和计时器工具；API 参见 docs/API_REFERENCE.md
- 单线程运行时：使用 Runtime::new().block_on(fut) 来驱动异步代码
- 计时器：使用 timer::sleep、timer::timeout 和 timer::Interval 进行调度
- 多核支持（特性门控）：使用 `multicore` 特性在多个 CPU 执行器间分发任务
- 网络：使用 HTTP echo 示例作为简单基于 TCP 协议的模板
- 平台说明：为获得最高 I/O 性能，建议在 Linux 上使用 `io-uring`（例如在 macOS 上的 OrbStack 支持的容器中）

### 基本用法

```rust
use rust_miniss::Runtime;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        println!("Hello from rust-miniss!");
    });
}
```

### 计时器工具

运行时提供了几种用于异步定时操作的计时器工具：

```rust
use rust_miniss::{timer, Runtime};
use std::time::Duration;

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // 睡眠 1 秒
        timer::sleep(Duration::from_secs(1)).await;
        println!("Slept for 1 second");
        
        // 为操作应用超时
        let result = timer::timeout(Duration::from_secs(2), async {
            timer::sleep(Duration::from_millis(500)).await;
            "Operation completed"
        }).await;
        
        match result {
            Ok(value) => println!("Success: {}", value),
            Err(_) => println!("Operation timed out"),
        }
        
        // 创建周期性间隔
        let mut interval = timer::Interval::new(Duration::from_millis(200));
        for i in 0..3 {
            interval.tick().await;
            println!("Tick {}", i + 1);
        }
    });
}
```

### 周期性任务

生成定期运行的任务：

```rust
use rust_miniss::{task, timer, Runtime};
use std::time::Duration;
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        
        // 生成周期性任务
        let handle = task::spawn_periodic(Duration::from_millis(100), move || {
            let counter = counter_clone.clone();
            async move {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                println!("Periodic task executed: {}", count + 1);
            }
        }).unwrap();
        
        // 让它运行一段时间
        timer::sleep(Duration::from_millis(550)).await;
        
        // 取消周期性任务
        handle.cancel().unwrap();
        println!("Final count: {}", counter.load(Ordering::SeqCst));
    });
}
```

### 通过信号优雅关闭

处理系统信号以实现应用程序的优雅关闭：

```rust
#[cfg(feature = "signal")]
use rust_miniss::{Runtime, timer, signal};
use std::time::Duration;

#[cfg(feature = "signal")]
fn main() {
    let runtime = Runtime::new();
    runtime.block_on(async {
        // 设置信号处理以实现优雅关闭
        let shutdown_signal = signal::wait_for_signal(&["SIGTERM", "SIGINT"]);
        
        // 主要应用程序逻辑
        let main_task = async {
            let mut counter = 0;
            loop {
                timer::sleep(Duration::from_millis(500)).await;
                counter += 1;
                println!("Working... iteration {}", counter);
                
                // 模拟完成一些工作
                if counter >= 20 {
                    println!("Work completed naturally");
                    break;
                }
            }
        };
        
        // 等待主任务完成或收到关闭信号
        tokio::select! {
            _ = main_task => {
                println!("Main task completed successfully");
            }
            signal = shutdown_signal => {
                println!("Received signal: {:?}, shutting down gracefully...", signal);
                
                // 执行清理操作
                println!("Cleaning up resources...");
                timer::sleep(Duration::from_millis(100)).await;
                
                // 关闭连接，刷新数据等
                println!("Cleanup completed, exiting");
            }
        }
    });
}

#[cfg(not(feature = "signal"))]
fn main() {
    println!("Signal handling example requires the 'signal' feature");
    println!("Run with: cargo run --features signal --example graceful_shutdown");
}
```

## Docker/OrbStack 开发

此项目包含用于开发的 Docker 容器。要清理 Docker/OrbStack 资源并释放磁盘空间，请使用提供的清理脚本：

```bash
./cleanup.sh
```

此脚本将：
- 显示当前磁盘使用情况
- 移除已停止的容器
- 移除悬挂的镜像
- 可选择性地移除未使用的卷（需要确认）
- 移除未使用的网络
- 显示最终磁盘使用情况

### 手动清理命令

对于手动清理，您可以单独运行这些命令：

```bash
# 列出当前对象
docker ps -a
docker images -a
docker volume ls
docker network ls

# 移除特定容器和镜像
docker rm <container_id>
docker rmi <image_id>

# 清理未使用的资源
docker container prune -f    # 移除已停止的容器
docker image prune -f        # 移除悬挂的镜像
docker volume prune -f       # 移除未使用的卷
docker network prune -f      # 移除未使用的网络

# 检查磁盘使用情况
docker system df
```

有关 Docker 最佳实践的更多信息，请参见 [OrbStack 文档](https://docs.orbstack.dev/)。

## 状态

这是一个学习项目，仍在开发中。路线图请参见 [DEVELOPMENT_PLAN.md](DEVELOPMENT_PLAN.md)。

- 项目目的和使用说明：请参见 [docs/USAGE_AND_POSITIONING.md](docs/USAGE_AND_POSITIONING.md)

## 许可证

MIT OR Apache-2.0