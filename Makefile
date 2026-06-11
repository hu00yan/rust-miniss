# Makefile for miniss development workflow
# 确保提交前运行必要检查，避免CI失败

.PHONY: help check quick full fmt clippy test build clean install-tools

# 默认目标：显示帮助
help:
	@echo "miniss 开发工具"
	@echo ""
	@echo "常用命令:"
	@echo "  make check      - 快速检查 (fmt + clippy + test)"
	@echo "  make quick      - 超快检查 (fmt + clippy)"
	@echo ""
	@echo "单独任务:"
	@echo "  make fmt        - 代码格式化"
	@echo "  make clippy     - Clippy 检查"
	@echo "  make test       - 运行测试"
	@echo "  make build      - 编译项目"
	@echo ""
	@echo "工具:"
	@echo "  make install-tools - 安装必要工具"
	@echo "  make clean         - 清理构建产物"

# 安装必要工具
install-tools:
	@echo "安装开发工具..."
	rustup toolchain install nightly
	rustup component add rustfmt clippy --toolchain stable
	rustup component add rust-src llvm-tools-preview miri --toolchain nightly
	cargo install cargo-nextest

# 格式化
fmt:
	@echo "检查代码格式..."
	cargo fmt --all -- --check

# Clippy 检查
clippy:
	@echo "运行 Clippy..."
	cargo clippy --all-targets -- -D warnings

# 超快检查 (提交前必须通过)
quick: fmt clippy
	@echo "✅ 快速检查完成"

# 基础测试
test:
	@echo "运行测试套件..."
	cargo test --all-targets

# 构建检查
build:
	@echo "编译检查..."
	cargo build --all-targets
	cargo build --release

# 标准检查 (fmt + clippy + test + build)
check: quick test build
	@echo "✅ 标准检查完成"

# 清理
clean:
	@echo "清理构建产物..."
	cargo clean

# 开发模式 - 监视文件变化并运行快速检查
watch:
	@echo "监视模式 (需要安装 cargo-watch)..."
	@if ! command -v cargo-watch >/dev/null 2>&1; then \
		echo "安装 cargo-watch..."; \
		cargo install cargo-watch; \
	fi
	cargo watch -x 'fmt --all -- --check' -x 'clippy --all-targets -- -D warnings' -x 'test'

# CI 模拟 - 模拟 CI 环境
ci-simulate: 
	@echo "模拟 CI 环境..."
	@echo "1️⃣ 格式检查..."
	@$(MAKE) fmt
	@echo "2️⃣ Clippy 检查..."
	@$(MAKE) clippy
	@echo "3️⃣ 构建检查..."
	@$(MAKE) build
	@echo "4️⃣ 测试运行..."
	@$(MAKE) test
	@echo "✅ CI 模拟完成"

# 性能测试
bench:
	@echo "运行性能测试..."
	cargo bench --all-targets --no-run
	cargo bench

# 文档生成
docs:
	@echo "生成文档..."
	cargo doc --all-features --no-deps --open

# 示例编译检查
examples:
	@echo "检查示例..."
	@for example in examples/*.rs; do \
		if [ -f "$$example" ]; then \
			example_name=$$(basename "$$example" .rs); \
			echo "编译示例: $$example_name"; \
			cargo build --example "$$example_name"; \
		fi \
	done