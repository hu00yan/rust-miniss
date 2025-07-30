#!/bin/bash

# 在容器中测试rust-miniss的脚本
set -e

echo "🐳 Starting rust-miniss container development environment..."

# 检查是否使用OrbStack（按照用户偏好）
if command -v orbctl &> /dev/null; then
    echo "🚀 Using OrbStack (user preference)"
    DOCKER_CMD="orbstack"
else
    echo "🐳 Using Docker"
    DOCKER_CMD="docker"
fi

# 构建并启动容器
echo "📦 Building container..."
docker-compose build

echo "🧪 Running tests in container..."
docker-compose run --rm rust-miniss bash -c "
    echo '=== Rust-Miniss Phase 2 Testing ==='
    echo 'System Info:'
    echo '  CPU cores: \$(nproc)'
    echo '  Memory: \$(free -h | grep Mem | awk '\''{ print \$2}'\'')'
    echo '  Kernel: \$(uname -r)'
    echo ''
    
    echo '📋 Running cargo check...'
    cargo check
    
    echo ''
    echo '🧪 Running unit tests...'
    cargo test --lib
    
    echo ''
    echo '🧪 Running integration tests...'
    cargo test --test multicore_tests
    
    echo ''
    echo '🎯 Running multicore demo...'
    cargo run --example multicore_demo
    
    echo ''
    echo '✅ All tests completed!'
"

echo "🎉 Testing completed!"
