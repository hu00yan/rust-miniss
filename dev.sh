#!/bin/bash

# Development script for rust-miniss
# Provides easy commands for both local and containerized development

set -e

# Detect platform and available tools
is_linux() {
    [[ "$OSTYPE" == "linux-gnu"* ]]
}

has_docker() {
    command -v docker >/dev/null 2>&1
}

use_docker() {
    if [[ "${USE_DOCKER:-}" == "1" ]] || ! is_linux; then
        return 0
    else
        return 1
    fi
}

# Function to run commands either locally or in docker
run_command() {
    local cmd="$1"
    shift
    if use_docker && has_docker; then
        echo "🐳 Running in Docker container..."
        docker-compose run --rm rust-miniss $cmd "$@"
    else
        echo "💻 Running locally..."
        eval "$cmd" "$@"
    fi
}

# Function to build either locally or in docker
build_project() {
    if use_docker && has_docker; then
        echo "🐳 Building development container..."
        docker-compose build
    else
        echo "💻 Building locally..."
        cargo build
    fi
}

case "${1:-help}" in
    "build")
        build_project
        ;;
    "shell")
        if use_docker && has_docker; then
            echo "🐚 Starting development shell in container..."
            docker-compose run --rm rust-miniss bash
        else
            echo "💻 Starting local shell..."
            bash
        fi
        ;;
    "check")
        echo "🔍 Running cargo check..."
        run_command "cargo check" "$@"
        ;;
    "test")
        echo "🧪 Running tests..."
        # Default to timer feature if no features specified
        if [[ "${@:2}" != *"--features"* ]]; then
            run_command "cargo test --features=timer" "${@:2}"
        else
            run_command "cargo test" "${@:2}"
        fi
        ;;
    "bench")
        echo "📊 Running benchmarks..."
        if use_docker && has_docker; then
            docker-compose run --rm rust-miniss cargo bench "$@"
        else
            echo "⚠️  Benchmarks are best run in Docker for consistency."
            cargo bench "$@"
        fi
        ;;
    "clean")
        if use_docker && has_docker; then
            echo "🧹 Stopping containers..."
            docker-compose down
        else
            echo "🧹 Cleaning up local target directory..."
            cargo clean
        fi
        ;;
    "clean-docker")
        if has_docker; then
            echo "🧹 Cleaning up Docker resources..."
            echo "📊 Current disk usage:"
            docker system df

            echo ""
            echo "🗑️  Removing stopped containers..."
            docker container prune -f

            echo ""
            echo "🗑️  Removing dangling images..."
            docker image prune -f

            echo ""
            echo "🗑️  Removing unused networks..."
            docker network prune -f

            echo ""
            echo "📊 Final disk usage:"
            docker system df
        else
            echo "🐳 Docker not found. Skipping Docker cleanup."
        fi
        ;;
    "test-in-container")
        # Replicate functionality of test-in-container.sh
        echo "🐳 Running comprehensive tests in container..."
        if command -v orbctl &> /dev/null; then
            echo "🚀 Using OrbStack (user preference)"
        else
            echo "🐳 Using Docker"
        fi
        
        echo "📦 Building container..."
        docker-compose build
        
        echo "🧪 Running tests in container..."
        docker-compose run --rm rust-miniss bash -c "
            echo '=== Rust-Miniss Testing ==='
            echo 'System Info:'
            echo '  CPU cores: \$(nproc)'
            echo '  Memory: \$(free -h | grep Mem | awk '{ print \$2}')'
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
        ;;
    "help"|*)
        echo "🦀 Rust-Miniss Development Helper"
        echo ""
        echo "Usage: $0 <command>"
        echo ""
        echo "Commands:"
        echo "  build             - Build the project (locally or in container)"
        echo "  shell             - Start an interactive shell (local or in container)"
        echo "  check             - Run cargo check"
        echo "  test              - Run cargo test (defaults to --features=timer)"
        echo "  bench             - Run cargo bench (Docker recommended for consistency)"
        echo "  clean             - Stop containers or clean local target directory"
        echo "  clean-docker      - Clean up Docker resources (images, containers, networks)"
        echo "  test-in-container - Run comprehensive tests in container"
        echo ""
        echo "Environment:"
        echo "  USE_DOCKER=1      - Force using Docker even on Linux"
        echo ""
        echo "Examples:"
        echo "  $0 build"
        echo "  $0 check"
        echo "  $0 test --features=multicore"
        echo "  $0 bench --bench io_benchmark"
        echo "  USE_DOCKER=1 $0 test"
        ;;
esac
