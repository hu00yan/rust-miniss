#!/bin/bash

# miniss-test.sh - 完整的本地测试脚本
# 在提交前运行所有CI检查，确保不会在CI中失败

set -euo pipefail

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 日志函数
log_info() {
    echo -e "${BLUE}INFO  $1${NC}"
}

log_success() {
    echo -e "${GREEN}OK   $1${NC}"
}

log_warning() {
    echo -e "${YELLOW}WARN $1${NC}"
}

log_error() {
    echo -e "${RED}ERR  $1${NC}"
}

log_step() {
    echo -e "\n${BLUE}STEP $1${NC}"
}

# 检查必要工具
check_prerequisites() {
    log_step "检查必要工具..."
    
    if ! command -v rustc &> /dev/null; then
        log_error "Rust 未安装"
        exit 1
    fi
    
    if ! rustup toolchain list | grep -q nightly; then
        log_warning "安装 nightly toolchain 用于 sanitizers..."
        rustup toolchain install nightly
        rustup component add rust-src --toolchain nightly
        rustup component add llvm-tools-preview --toolchain nightly
    fi
    
    if ! rustup component list --toolchain nightly | grep -q "miri.*installed"; then
        log_warning "安装 Miri..."
        rustup component add miri --toolchain nightly
    fi
    
    if ! command -v cargo-nextest &> /dev/null; then
        log_warning "安装 cargo-nextest..."
        cargo install cargo-nextest
    fi
    
    log_success "所有工具就绪"
}

# 1. 代码格式检查
check_formatting() {
    log_step "1. 代码格式检查 (cargo fmt)"
    cargo fmt --all -- --check
    log_success "代码格式正确"
}

# 2. Clippy 检查
check_clippy() {
    log_step "2. Clippy 检查"
    cargo clippy --all-targets -- -D warnings
    log_success "Clippy 检查通过"
}

# 3. 编译检查
check_compilation() {
    log_step "3. 编译检查"
    
    log_info "检查默认特性..."
    cargo check
    
    log_info "检查所有特性..."
    cargo check --all-targets
    
    log_info "构建 release 版本..."
    cargo build --release
    
    log_success "编译检查通过"
}

# 4. 基础测试
run_basic_tests() {
    log_step "4. 基础测试套件"
    
    log_info "运行所有测试..."
    cargo test --all-targets
    
    log_info "运行集成测试..."
    cargo test --test integration_tests
    
    log_success "基础测试通过"
}

# 5. Nextest (如果可用)
run_nextest() {
    log_step "5. Nextest 运行"
    if command -v cargo-nextest &> /dev/null; then
        cargo nextest run --all-targets
        log_success "Nextest 通过"
    else
        log_warning "Nextest 未安装，跳过"
    fi
}

# 6. 文档检查
check_docs() {
    log_step "6. 文档检查"
    cargo doc --all-features --no-deps
    log_success "文档生成成功"
}

# 7. Address Sanitizer + Leak Sanitizer
run_asan() {
    log_step "7. AddressSanitizer + LeakSanitizer"
    log_info "这可能需要几分钟..."
    
    export RUSTFLAGS="-Zsanitizer=address,leak"
    cargo +nightly test --all-targets
    unset RUSTFLAGS
    
    log_success "ASan + LSan 通过"
}

# 8. Thread Sanitizer (单独运行，与ASan冲突)
run_tsan() {
    log_step "8. ThreadSanitizer"
    log_info "检测数据竞争和并发错误..."
    
    export RUSTFLAGS="-Zsanitizer=thread"
    cargo +nightly test --all-targets
    unset RUSTFLAGS
    
    log_success "TSan 通过"
}

# 9. Miri (检查 unsafe 代码和 UB)
run_miri() {
    log_step "9. Miri (undefined behavior 检查)"
    log_info "这会比较慢..."
    
    cargo +nightly miri test --all-targets
    log_success "Miri 通过"
}

# 10. 基准测试编译检查
check_benchmarks() {
    log_step "10. 基准测试编译检查"
    cargo bench --all-targets --no-run
    log_success "基准测试编译通过"
}

# 11. 示例编译检查
check_examples() {
    log_step "11. 示例编译检查"
    
    for example in examples/*.rs; do
        if [[ -f "$example" ]]; then
            example_name=$(basename "$example" .rs)
            log_info "编译示例: $example_name"
            cargo build --example "$example_name"
        fi
    done
    
    log_success "所有示例编译通过"
}

# 主函数
main() {
    local start_time=$(date +%s)
    
    echo -e "${BLUE}miniss 完整测试套件${NC}"
    echo -e "${BLUE}═══════════════════════════════════${NC}"
    
    # 解析命令行参数
    local run_fast=false
    local run_sanitizers=true
    local run_miri=true
    
    while [[ $# -gt 0 ]]; do
        case $1 in
            --fast)
                run_fast=true
                run_sanitizers=false
                run_miri=false
                shift
                ;;
            --no-sanitizers)
                run_sanitizers=false
                shift
                ;;
            --no-miri)
                run_miri=false
                shift
                ;;
            --help)
                echo "用法: $0 [选项]"
                echo "选项:"
                echo "  --fast          快速模式 (跳过 sanitizers 和 miri)"
                echo "  --no-sanitizers 跳过 sanitizers"
                echo "  --no-miri       跳过 miri"
                echo "  --help          显示帮助"
                exit 0
                ;;
            *)
                log_error "未知选项: $1"
                exit 1
                ;;
        esac
    done
    
    if [[ "$run_fast" == true ]]; then
        log_info "快速模式: 跳过 sanitizers 和 miri"
    fi
    
    # 检查先决条件
    check_prerequisites
    
    # 运行检查
    check_formatting
    check_clippy
    check_compilation
    run_basic_tests
    run_nextest
    check_docs
    check_benchmarks
    check_examples
    
    # 运行 sanitizers (可选)
    if [[ "$run_sanitizers" == true ]]; then
        run_asan
        run_tsan
    fi
    
    # 运行 Miri (可选)
    if [[ "$run_miri" == true ]]; then
        run_miri
    fi
    
    # 完成
    local end_time=$(date +%s)
    local duration=$((end_time - start_time))
    
    echo -e "\n${GREEN}所有检查通过！${NC}"
    echo -e "${GREEN}总耗时: ${duration}s${NC}"
    echo -e "${GREEN}可以安全提交到 CI${NC}"
}

# 捕获 Ctrl+C
trap 'log_error "测试被中断"; exit 1' INT

# 运行主函数
main "$@"