# futures::io Traits Implementation Design

## 概述

本文档描述如何为rust-miniss的`AsyncTcpStream`正确实现`futures::io::AsyncRead`和`AsyncWrite` traits，实现与Hyper等HTTP库的兼容性。

## 问题分析

### 当前状态
- ❌ 删除了错误的实现（每次poll都创建新future）
- ❌ 没有正确的状态管理
- ❌ 无法正确处理部分读取/写入

### 核心挑战
1. **状态管理**：futures::io要求poll操作间保持状态
2. **部分操作**：TCP可能部分读取/写入，需要多次poll
3. **异步集成**：需要与io_uring正确集成
4. **性能权衡**：兼容性vs零拷贝

## 设计目标

### 功能目标
- ✅ 正确实现AsyncRead trait
- ✅ 正确实现AsyncWrite trait
- ✅ 与Hyper等库完全兼容
- ✅ 保持现有零拷贝API不变

### 性能目标
- ✅ 合理的内存使用（允许memcpy）
- ✅ 正确的异步行为（无阻塞）
- ✅ 可接受的性能（教育项目标准）

## 架构设计

### 状态管理结构

```rust
pub struct AsyncTcpStream {
    inner: TcpStream,
    // 关键：为每个操作维护状态
    read_state: Option<ReadState>,
    write_state: Option<WriteState>,
}

#[derive(Debug)]
enum ReadState {
    /// 没有进行中的操作
    Idle,
    /// 读取操作正在进行
    InProgress {
        token: IoToken,
        buffer: Vec<u8>,  // 内部缓冲区
    },
    /// 操作已完成，等待poll_read调用
    Completed {
        result: io::Result<usize>,
        data: Vec<u8>,
    },
}

#[derive(Debug)]
enum WriteState {
    /// 没有进行中的操作
    Idle,
    /// 写入操作正在进行
    InProgress {
        token: IoToken,
        data: Vec<u8>,
    },
    /// 操作已完成，等待poll_write调用
    Completed {
        result: io::Result<usize>,
    },
}
```

### 关键设计原则

1. **状态持久化**：操作状态在struct中保持，不在函数中创建
2. **一次操作**：同时只能有一个读取和一个写入操作
3. **正确清理**：操作完成后及时清理状态
4. **Waker注册**：Pending时必须注册waker

## 实现方案

### AsyncRead实现

```rust
impl futures::io::AsyncRead for AsyncTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // 检查当前状态
        match self.read_state {
            ReadState::Idle => {
                // 启动新的读取操作
                self.start_read_operation(cx, buf)
            }
            ReadState::InProgress { ref token, .. } => {
                // 检查操作是否完成
                self.check_read_completion(cx, buf, token)
            }
            ReadState::Completed { ref result, ref data } => {
                // 返回已完成的结果
                self.consume_read_result(result, data, buf)
            }
        }
    }
}
```

### 核心方法实现

#### 启动读取操作
```rust
fn start_read_operation(
    &mut self,
    cx: &mut Context<'_>,
    buf: &mut [u8],
) -> Poll<io::Result<usize>> {
    let state = io_state();

    // 分配内部缓冲区（为了兼容性，比用户buffer稍大）
    let buffer_size = std::cmp::max(buf.len(), 4096);
    let internal_buffer = vec![0u8; buffer_size];

    // 提交io_uring读取操作
    let op = Op::Read {
        fd: self.inner.as_raw_fd(),
        offset: 0,
        len: internal_buffer.len(),
    };

    let token = state.io_backend.submit(op);

    // 更新状态
    self.read_state = Some(ReadState::InProgress {
        token,
        buffer: internal_buffer,
    });

    // 立即检查是否完成（可能同步完成）
    self.check_read_completion(cx, buf, &token)
}
```

#### 检查完成状态
```rust
fn check_read_completion(
    &mut self,
    cx: &mut Context<'_>,
    buf: &mut [u8],
    token: &IoToken,
) -> Poll<io::Result<usize>> {
    let state = io_state();

    // 检查io_uring是否完成操作
    if let Some(completion) = state.completed_io.lock().unwrap().remove(token) {
        // 操作完成，转换状态
        match completion {
            Ok(CompletionKind::Read { bytes_read, data }) => {
                let result = Ok(bytes_read);
                self.read_state = Some(ReadState::Completed {
                    result,
                    data,
                });
                self.consume_read_result(&result, &data, buf)
            }
            Ok(_) => {
                let error = io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Unexpected completion kind"
                );
                self.read_state = Some(ReadState::Completed {
                    result: Err(error),
                    data: Vec::new(),
                });
                Poll::Ready(Err(error))
            }
            Err(e) => {
                let io_error = e.into();
                self.read_state = Some(ReadState::Completed {
                    result: Err(io_error),
                    data: Vec::new(),
                });
                Poll::Ready(Err(io_error))
            }
        }
    } else {
        // 操作仍在进行，注册waker
        state.io_wakers.lock().unwrap().insert(*token, cx.waker().clone());
        Poll::Pending
    }
}
```

#### 消费读取结果
```rust
fn consume_read_result(
    &mut self,
    result: &io::Result<usize>,
    data: &[u8],
    buf: &mut [u8],
) -> Poll<io::Result<usize>> {
    // 清除状态，准备下次操作
    self.read_state = Some(ReadState::Idle);

    match result {
        Ok(bytes_read) => {
            // 将内部buffer数据复制到用户buffer（兼容性权衡）
            let to_copy = std::cmp::min(*bytes_read, buf.len());
            buf[..to_copy].copy_from_slice(&data[..to_copy]);
            Poll::Ready(Ok(to_copy))
        }
        Err(e) => Poll::Ready(Err(*e)),
    }
}
```

### AsyncWrite实现

```rust
impl futures::io::AsyncWrite for AsyncTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.write_state {
            WriteState::Idle => {
                self.start_write_operation(cx, buf)
            }
            WriteState::InProgress { ref token, .. } => {
                self.check_write_completion(cx, token)
            }
            WriteState::Completed { ref result } => {
                self.consume_write_result(result)
            }
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        // TCP的flush通常是no-op
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        // TCP关闭处理
        Poll::Ready(Ok(()))
    }
}
```

## 兼容性层

### 与现有API共存
```rust
// 保持现有的零拷贝API不变
impl AsyncTcpStream {
    /// 零拷贝读取（内部使用）
    pub async fn read(&self) -> io::Result<(usize, Vec<u8>)> {
        // 现有实现保持不变
    }

    /// 零拷贝写入（内部使用）
    pub async fn write(&self, buf: &[u8]) -> io::Result<usize> {
        // 现有实现保持不变
    }
}

// 新增futures::io兼容性
impl futures::io::AsyncRead for AsyncTcpStream {
    // 兼容性实现（需要memcpy）
}

impl futures::io::AsyncWrite for AsyncTcpStream {
    // 兼容性实现（需要memcpy）
}
```

### 用户使用模式
```rust
// 方式1：使用零拷贝API（高性能）
let (bytes, buffer) = stream.read().await?;

// 方式2：使用futures::io（生态兼容）
use futures::io::AsyncReadExt;
let mut buffer = [0u8; 1024];
let bytes = stream.read(&mut buffer).await?;

// 方式3：与Hyper集成
use tokio_util::compat::FuturesAsyncReadCompatExt;
let tokio_stream = stream.compat();
hyper::server::conn::Http::new()
    .serve_connection(tokio_stream, service)
    .await;
```

## 错误处理

### 错误类型转换
```rust
fn convert_completion_to_io_error(completion: Result<CompletionKind, IoError>) -> io::Result<usize> {
    match completion {
        Ok(CompletionKind::Read { bytes_read, .. }) => Ok(bytes_read),
        Ok(CompletionKind::Write { bytes_written }) => Ok(bytes_written),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unexpected completion kind"
        )),
        Err(e) => Err(e.into()),
    }
}
```

### 资源清理
```rust
impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        // 清理正在进行的操作
        if let Some(ReadState::InProgress { token, .. }) = self.read_state {
            // 取消io_uring操作（如果可能）
            let state = io_state();
            state.completed_io.lock().unwrap().remove(&token);
        }

        if let Some(WriteState::InProgress { token, .. }) = self.write_state {
            let state = io_state();
            state.completed_io.lock().unwrap().remove(&token);
        }
    }
}
```

## 性能考虑

### 内存使用
- **内部缓冲区**：读取时分配，写入时复制用户数据
- **状态开销**：每个连接增加约100字节状态
- **memcpy开销**：兼容性权衡，接受性能损失

### 并发特性
- **单连接**：同一时间只能有一个读取和一个写入操作
- **多连接**：每个连接独立状态，完全并发
- **Thread-per-core**：无需锁，无跨核同步

## 实现优先级

### Phase 1: 基础实现
1. 实现状态管理结构
2. 实现AsyncRead的基础功能
3. 实现AsyncWrite的基础功能
4. 基础错误处理

### Phase 2: 完善功能
1. 完善错误处理和边界情况
2. 添加资源清理逻辑
3. 优化内存使用
4. 添加性能监控

### Phase 3: 测试和文档
1. 完整的单元测试覆盖
2. 集成测试（Hyper兼容性）
3. 性能基准测试
4. 用户文档和示例

## 代码量估算

- **核心实现**: ~400行
- **状态管理**: ~150行
- **错误处理**: ~100行
- **测试代码**: ~300行
- **文档**: ~100行

**总计**: ~1050行，完全在项目范围内。

## 注意事项

1. **状态一致性**：确保状态转换的正确性
2. **Waker生命周期**：避免重复注册或忘记注册
3. **错误传播**：正确处理io_uring错误到io::Error的转换
4. **内存安全**：所有unsafe操作必须严格验证
5. **测试覆盖**：每个状态转换都需要测试

这个实现将提供正确的futures::io兼容性，使rust-miniss能够与整个HTTP生态系统集成，同时保持教育项目的简洁性。