# CI 错误总结 - feat/http-mvp 分支

## 1. Docs workflow 失败 (ID: 16858021635)

**错误类型**: Rust工具链配置错误

**问题**: rustup 尝试安装 `rustdoc` component 失败
```
error: component 'rustdoc' for target 'x86_64-unknown-linux-gnu' is unavailable for download for channel 'stable'
```

**原因**: `rustdoc` 组件在新版本的 Rust 中已被弃用/移除

**修复建议**: 
- 移除 `.github/workflows/docs.yml` 中的 `components: rustdoc` 
- `rustdoc` 已经内置在标准 Rust 安装中，无需单独安装

---

## 2. Typos workflow 失败 (ID: 16858021638)

**错误类型**: TOML 配置文件语法错误

**问题**: `.typos.toml` 文件中的正则表达式转义错误
```
TOML parse error at line 4, column 13
4 | "http://[\\w\\./-]+",
  |             ^
missing escaped value, expected `b`, `f`, `n`, `r`, `\\`, `"`, `u`, `U`
```

**原因**: TOML 中反斜杠需要双重转义

**修复建议**:
- 将 `.typos.toml` 第4行的 `"http://[\\w\\./-]+"` 改为 `"http://[\\\\w\\\\./-]+"`

---

## 3. Codespell workflow 失败 (ID: 16858021636)

**错误类型**: codespell 配置错误

**问题**: 未知的内置词典
```
ERROR: Unknown builtin dictionary: usage-typos
```

**原因**: `.codespellrc` 配置文件中引用了不存在的词典

**修复建议**:
- 检查并修复 `.codespellrc` 文件中的 `builtin` 设置
- 移除 `usage-typos` 或替换为有效的词典名

---

## 4. CI Build Matrix 失败 (ID: 16858021626)

**错误类型**: 代码格式化问题 + 编译错误

### 4.1 格式化问题
- 多个文件的代码格式不符合 `cargo fmt` 标准
- 涉及文件: `bench_compare/http_echo_tokio.rs`, `examples/comprehensive_demo.rs`, `src/cpu.rs` 等

### 4.2 编译错误
- `tests/task_cancellation_tests.rs` 缺少必要的 import:
  ```rust
  // 需要添加:
  use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
  use std::time::Duration;
  ```

**修复建议**:
1. 运行 `cargo fmt` 修复格式化问题
2. 在 `tests/task_cancellation_tests.rs` 顶部添加缺失的导入

---

## 修复优先级

1. **高优先级**: 修复编译错误 (缺少imports)
2. **中优先级**: 修复配置文件 (typos.toml, codespellrc)  
3. **低优先级**: 运行 cargo fmt 修复格式化

## 快速修复命令

```bash
# 1. 修复格式化
cargo fmt --all

# 2. 修复imports (手动编辑 tests/task_cancellation_tests.rs)
# 在文件顶部添加:
# use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
# use std::time::Duration;

# 3. 修复 typos 配置 (编辑 .typos.toml)
# 4. 修复 codespell 配置 (编辑 .codespellrc)
# 5. 修复 docs workflow (编辑 .github/workflows/docs.yml)
```
