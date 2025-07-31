#!/bin/bash

# Development script for rust-miniss
# Provides easy commands for containerized development

set -e

case "${1:-help}" in
    "build")
        echo "🔨 Building development container..."
        docker-compose build
        ;;
    "shell")
        echo "🐚 Starting development shell..."
        docker-compose run --rm rust-miniss bash
        ;;
    "check")
        echo "🔍 Running cargo check..."
        docker-compose run --rm rust-miniss cargo check
        ;;
    "test")
        echo "🧪 Running tests..."
        docker-compose run --rm rust-miniss cargo test ${@:2}
        ;;
    "bench")
        echo "📊 Running benchmarks..."
        docker-compose run --rm rust-miniss cargo bench ${@:2}
        ;;
    "clean")
        echo "🧹 Cleaning up..."
        docker-compose down
        docker-compose rm -f
        ;;
    "help"|*)
        echo "🦀 Rust-Miniss Development Helper"
        echo ""
        echo "Usage: $0 <command>"
        echo ""
        echo "Commands:"
        echo "  build   - Build the development container"
        echo "  shell   - Start an interactive shell in the container"
        echo "  check   - Run cargo check"
        echo "  test    - Run cargo test (pass additional args)"
        echo "  bench   - Run cargo bench (pass additional args)"
        echo "  clean   - Clean up containers"
        echo "  help    - Show this help"
        echo ""
        echo "Examples:"
        echo "  $0 build"
        echo "  $0 shell"
        echo "  $0 test --features=multicore"
        echo "  $0 bench --bench io_benchmark"
        ;;
esac
